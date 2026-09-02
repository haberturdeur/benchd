//! Making device nodes appear in an agent's sandbox, and taking them away.
//!
//! This is the piece that makes normal tools work. `esptool`, `idf.py monitor`,
//! `minicom` and `openocd` all want a character device; an `rfc2217://` URL is a
//! pyserial-only convenience that most of those refuse. So a lease materialises
//! **real device nodes** and the agent uses its normal toolchain.
//!
//! Layout:
//!
//! ```text
//! /run/benchd/<owner>/<lease-id>/<slot>/<resource>
//! ```
//!
//! The lease id is in the path deliberately. If `/dev/lab/dut` meant board A
//! last lease and board B this lease, a stale shell or backgrounded script would
//! write to the wrong board — the exact failure this system exists to prevent,
//! reintroduced through the back door. With the lease id in the path, a stale
//! reference fails with `ENOENT`.
//!
//! Bind mounts rather than symlinks: the agent's sandbox has no `/dev`, so a
//! symlink to `/dev/ttyACM0` would dangle. A bind mount puts the real inode at
//! the destination, which is why the device behaves exactly as it would
//! normally — it *is* the device.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use benchd_core::lease::{LeaseId, SessionId};
use benchd_core::sysfs;
use benchd_core::usbip;
use benchd_core::wire::{ChannelHello, ChannelSide, Outcome, ResourceHandle};
use futures::SinkExt;
use tokio_util::codec::{FramedWrite, LinesCodec};

/// A device node and the uid/gid it had before an agent was given it.
type OwnershipToRestore = (PathBuf, (u32, u32));

pub struct Materializer {
    root: PathBuf,
    coordinator: String,
    /// Highest epoch seen per lease. The client is an executor too, and a
    /// delayed instruction for a superseded lease must be dropped rather than
    /// obeyed — obeying it would expose hardware whose lease is gone (D7).
    /// Until now only the host fenced, and the client relied on TCP ordering.
    seen: BTreeMap<LeaseId, benchd_core::lease::Epoch>,
    active: BTreeMap<LeaseId, PathBuf>,
    /// vhci ports this lease imported, needing an explicit detach.
    imported: BTreeMap<LeaseId, Vec<u32>>,
    /// Device ownership to put back on release. A bind mount shares the source
    /// inode, so handing a device to an agent changes it in `/dev` too.
    restore: BTreeMap<LeaseId, Vec<OwnershipToRestore>>,
}

impl Materializer {
    pub fn new(root: impl Into<PathBuf>, coordinator: String) -> Self {
        Materializer {
            root: root.into(),
            coordinator,
            seen: BTreeMap::new(),
            active: BTreeMap::new(),
            restore: BTreeMap::new(),
            imported: BTreeMap::new(),
        }
    }

    /// Import a remote device and return the device node it produced.
    ///
    /// Dial out, complete the import handshake over the relayed connection, hand
    /// the socket to vhci, then wait for the kernel to enumerate — `attach`
    /// returns before the tty exists.
    async fn import(
        &self,
        channel: &benchd_core::wire::ChannelKey,
        busid: &str,
    ) -> Result<(u32, PathBuf), String> {
        let stream = tokio::net::TcpStream::connect(&self.coordinator)
            .await
            .map_err(|e| format!("dialling the coordinator for a data channel: {e}"))?;
        stream.set_nodelay(true).ok();
        let (read, write) = stream.into_split();

        let mut sink = FramedWrite::new(write, LinesCodec::new());
        let hello = ChannelHello { channel: channel.clone(), side: ChannelSide::Client };
        sink.send(serde_json::to_string(&hello).map_err(|e| e.to_string())?)
            .await
            .map_err(|e| format!("sending the channel hello: {e}"))?;
        let mut stream = read
            .reunite(sink.into_inner())
            .map_err(|e| format!("reuniting the socket: {e}"))?;

        tracing::info!(channel = %channel.0, "data channel open; requesting import");
        let device = usbip::request_import(&mut stream, busid)
            .await
            .map_err(|e| format!("usbip import: {e}"))?;
        tracing::info!(
            channel = %channel.0, busid = %device.busid,
            vid = format!("{:04x}", device.id_vendor),
            pid = format!("{:04x}", device.id_product),
            "importing"
        );

        let port = sysfs::free_vhci_port(device.speed).await.map_err(|e| e.to_string())?;

        sysfs::vhci_attach(port, stream, device.devid(), device.speed)
            .await
            .map_err(|e| format!("vhci attach on port {port}: {e}"))?;
        tracing::info!(port, devid = device.devid(), "attached; waiting for enumeration");

        // Located by vhci port rather than by diffing /dev/serial/by-id: a
        // forwarded device reproduces the *same* by-id name as the one that
        // just vanished locally, so a diff would see nothing at all.
        match sysfs::wait_for_vhci_tty(port, device.speed, std::time::Duration::from_secs(10))
            .await
        {
            Some(path) => Ok((port, path)),
            None => {
                sysfs::vhci_detach(port).await;
                Err(format!(
                    "{busid} was imported on port {port} but no serial device appeared;                      is the right usb-serial driver available on this machine?"
                ))
            }
        }
    }

    fn lease_dir(&self, owner: &str, lease: LeaseId) -> PathBuf {
        self.root.join(owner).join(lease.to_string())
    }

    /// Remove everything under our root.
    ///
    /// Bind mounts are kernel state and outlive the process that made them, so
    /// without this a restart would leave hardware reachable by an agent whose
    /// lease is gone (D6).
    pub async fn clear_stale(&mut self) {
        // Every lease the coordinator granted is void, so none of the
        // bookkeeping about them means anything either.
        //
        // `seen` in particular MUST be cleared. It is keyed by lease id, and a
        // stateless coordinator restarts both lease ids and per-bench epochs at
        // 1 — but they advance at different rates once there is more than one
        // bench, so a reused lease id can arrive with a *lower* epoch than the
        // one recorded against it and be fenced out as stale, permanently. This
        // is the same bug Appendix B records for the host, reintroduced on the
        // client when fencing was added here; the host clears its watermark in
        // `release_all` for exactly this reason. It hid because a single-bench
        // lab keeps lease ids and epochs in lockstep.
        self.seen.clear();
        self.active.clear();
        self.imported.clear();
        self.restore.clear();

        // Imported devices are kernel state too, and outlive us just as mounts do.
        for port in sysfs::attached_ports().await {
            tracing::warn!(port, "detaching a stale imported device from a previous run");
            sysfs::vhci_detach(port).await;
        }
        let Ok(mut owners) = tokio::fs::read_dir(&self.root).await else {
            return;
        };
        while let Ok(Some(owner)) = owners.next_entry().await {
            let Ok(mut leases) = tokio::fs::read_dir(owner.path()).await else {
                continue;
            };
            while let Ok(Some(lease)) = leases.next_entry().await {
                tracing::warn!(path = %lease.path().display(), "clearing a stale lease directory");
                unmount_tree(&lease.path()).await;
            }
        }
    }

    /// Accept an instruction only if it is at least as new as anything we have
    /// already seen for this lease.
    fn fence(&mut self, lease: LeaseId, epoch: benchd_core::lease::Epoch) -> Result<(), Outcome> {
        let seen = self.seen.entry(lease).or_insert(benchd_core::lease::Epoch(0));
        if epoch < *seen {
            tracing::warn!(%lease, ?epoch, ?seen, "dropping a stale instruction");
            return Err(Outcome::Stale { seen: *seen });
        }
        *seen = epoch;
        Ok(())
    }

    pub async fn materialize(
        &mut self,
        lease: LeaseId,
        epoch: benchd_core::lease::Epoch,
        owner: &str,
        uid: Option<u32>,
        slots: &BTreeMap<String, BTreeMap<String, ResourceHandle>>,
    ) -> Outcome {
        if let Err(stale) = self.fence(lease, epoch) {
            return stale;
        }
        let dir = self.lease_dir(owner, lease);

        // Resolve everything before mounting anything: a half-materialised
        // lease is worse than a failed one, because the agent would find some
        // of its devices and reasonably assume it had them all.
        let mut plan: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut ports: Vec<u32> = Vec::new();
        for (slot, resources) in slots {
            for (name, handle) in resources {
                // This process is root and about to call mount(2) on a path
                // built from these names. They arrive from the coordinator,
                // which validates them — but a privileged daemon that trusts
                // its input has no business being privileged, so check again.
                if !benchd_core::model::valid_component(slot)
                    || !benchd_core::model::valid_component(name)
                {
                    for port in &ports {
                        sysfs::vhci_detach(*port).await;
                    }
                    return Outcome::Failed {
                        detail: format!(
                            "refusing to materialise {slot:?}/{name:?}: \
                             not a plain path component"
                        ),
                    };
                }
                match handle {
                    ResourceHandle::Local { path } => {
                        let source = match tokio::fs::canonicalize(path).await {
                            Ok(p) => p,
                            Err(err) => {
                                return Outcome::Failed {
                                    detail: format!(
                                        "{slot}/{name}: {path} is not present ({err})"
                                    ),
                                };
                            }
                        };
                        // Checked again after canonicalising, and against the
                        // resolved path rather than the declared one: a symlink
                        // under /dev could otherwise point anywhere. The
                        // coordinator validates too, but this process is root
                        // and must not trust it.
                        if let Err(why) = benchd_core::model::valid_device_path(&source) {
                            return Outcome::Failed {
                                detail: format!("{slot}/{name}: refusing to mount: {why}"),
                            };
                        }
                        match tokio::fs::metadata(&source).await {
                            Ok(meta) => {
                                use std::os::unix::fs::FileTypeExt;
                                if !meta.file_type().is_char_device() {
                                    return Outcome::Failed {
                                        detail: format!(
                                            "{slot}/{name}: {} is not a character device",
                                            source.display()
                                        ),
                                    };
                                }
                            }
                            Err(err) => {
                                return Outcome::Failed {
                                    detail: format!("{slot}/{name}: {err}"),
                                };
                            }
                        }
                        plan.push((source, dir.join(slot).join(name)));
                    }
                    ResourceHandle::UsbIp { channel, busid } => {
                        match self.import(channel, busid).await {
                            Ok((port, source)) => {
                                ports.push(port);
                                plan.push((source, dir.join(slot).join(name)));
                            }
                            Err(detail) => {
                                for port in &ports {
                                    sysfs::vhci_detach(*port).await;
                                }
                                return Outcome::Failed {
                                    detail: format!("{slot}/{name}: {detail}"),
                                };
                            }
                        }
                    }
                }
            }
        }

        for (source, dest) in &plan {
            if let Err(detail) = bind_mount(&self.root, source, dest).await {
                // Roll back, so a failure never leaves a partial lease behind.
                unmount_tree(&dir).await;
                for port in &ports {
                    sysfs::vhci_detach(*port).await;
                }
                return Outcome::Failed { detail };
            }
        }

        // Hand the devices to the agent that asked for them. Without this the
        // node keeps the source device's `root:uucp 0660` and the unprivileged
        // agent cannot open the hardware it just leased — which fails the one
        // promise the whole system makes.
        if let Some(uid) = uid {
            for (_, dest) in &plan {
                match previous_owner(dest).await {
                    Some(prev) => {
                        self.restore.entry(lease).or_default().push((dest.clone(), prev));
                        if let Err(err) = chown(dest, uid).await {
                            tracing::warn!(path = %dest.display(), ?err, "could not hand the device to the agent");
                        }
                    }
                    None => tracing::warn!(path = %dest.display(), "could not read device ownership"),
                }
            }
        }

        self.active.insert(lease, dir.clone());
        if !ports.is_empty() {
            self.imported.insert(lease, ports);
        }
        tracing::info!(%lease, %owner, path = %dir.display(), mounts = plan.len(), "materialized");
        Outcome::Ok
    }

    pub async fn unmaterialize(
        &mut self,
        lease: LeaseId,
        epoch: benchd_core::lease::Epoch,
        owner: &str,
    ) -> Outcome {
        if let Err(stale) = self.fence(lease, epoch) {
            return stale;
        }
        self.unmaterialize_now(lease, owner).await
    }

    /// Teardown without fencing, for paths that already know the lease is dead
    /// (a failed setup, or clearing state at startup).
    pub async fn unmaterialize_now(&mut self, lease: LeaseId, owner: &str) -> Outcome {
        // Give the device back before unmounting: afterwards the path is gone
        // and the inode is unreachable from here.
        for (path, (uid, gid)) in self.restore.remove(&lease).unwrap_or_default() {
            let _ = chown_gid(&path, uid, gid).await;
        }
        // Idempotent: the reaper races voluntary releases, and neither path may
        // fail. Fall back to the computed path so a restarted daemon can still
        // clean up a lease it does not remember.
        let dir = self.active.remove(&lease).unwrap_or_else(|| self.lease_dir(owner, lease));
        unmount_tree(&dir).await;
        // Detach after unmounting: the mount is what the agent holds, and the
        // vhci port is what the kernel holds.
        for port in self.imported.remove(&lease).unwrap_or_default() {
            sysfs::vhci_detach(port).await;
        }
        tracing::info!(%lease, %owner, "unmaterialized");
        Outcome::Ok
    }
}

async fn bind_mount(root: &Path, source: &Path, dest: &Path) -> Result<(), String> {
    // Belt and braces. Name validation should already make this impossible; if
    // it ever does not, the failure is a root-privileged mount at an arbitrary
    // location, so it is worth one comparison.
    if !dest.starts_with(root) {
        return Err(format!(
            "refusing to mount outside {}: {}",
            root.display(),
            dest.display()
        ));
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    // The mount target must exist and be file-like; a device node is mounted
    // over a plain file perfectly happily.
    if tokio::fs::metadata(dest).await.is_err() {
        tokio::fs::write(dest, b"").await.map_err(|e| format!("touch {}: {e}", dest.display()))?;
    }

    let status = tokio::process::Command::new("mount")
        .arg("--bind")
        .arg(source)
        .arg(dest)
        .output()
        .await
        .map_err(|e| format!("failed to run mount: {e}"))?;

    if !status.status.success() {
        return Err(format!(
            "mount --bind {} {}: {}",
            source.display(),
            dest.display(),
            String::from_utf8_lossy(&status.stderr).trim()
        ));
    }
    Ok(())
}

/// Unmount everything under `dir`, then remove it.
///
/// Failures are logged, never propagated: the reaper must always be able to
/// finish, and a stuck unmount must not wedge the daemon.
async fn unmount_tree(dir: &Path) {
    let Ok(mut slots) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(slot)) = slots.next_entry().await {
        let Ok(mut resources) = tokio::fs::read_dir(slot.path()).await else {
            continue;
        };
        while let Ok(Some(resource)) = resources.next_entry().await {
            // Lazy: an agent still holding the fd must not block teardown. Its
            // next read fails, which is exactly what "your lease ended" should
            // feel like.
            let _ = tokio::process::Command::new("umount")
                .arg("--lazy")
                .arg(resource.path())
                .output()
                .await;
        }
    }
    if let Err(err) = tokio::fs::remove_dir_all(dir).await {
        tracing::debug!(path = %dir.display(), ?err, "could not remove lease directory");
    }
}

/// The path an agent should use for a materialised resource.
pub fn resource_path(
    root: &Path,
    owner: &str,
    lease: LeaseId,
    slot: &str,
    resource: &str,
) -> PathBuf {
    root.join(owner).join(lease.to_string()).join(slot).join(resource)
}

/// Sanitise an agent's declared name into a directory component.
///
/// Agent-supplied, so it must never escape the root — this is the one place an
/// agent's input reaches a path.
///
/// **Derived from the name alone, deliberately.** The sandbox has to bind-mount
/// this directory when the agent *starts*, which is before the coordinator has
/// issued a session id — so the path cannot depend on one. Two sessions calling
/// themselves the same thing share a directory and are told apart by their lease
/// subdirectories, which is the right answer anyway: it is the same agent.
pub fn owner_dir(session: SessionId, name: &str) -> String {
    let safe: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .take(48)
        .collect();
    // Filtering alone is not enough once dots are allowed: a name of ".." keeps
    // both of them and *is* the parent directory. Run the result through the
    // same check used for slot names, which rejects "." and "..", and fall back
    // to the session id if it does not survive.
    if benchd_core::model::valid_component(&safe) {
        safe
    } else {
        session.to_string()
    }
}


async fn chown(path: &Path, uid: u32) -> std::io::Result<()> {
    let gid = previous_owner(path).await.map(|(_, g)| g).unwrap_or(0);
    chown_gid(path, uid, gid).await
}

async fn chown_gid(path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::os::unix::fs::chown(&path, Some(uid), Some(gid))
    })
    .await
    .map_err(std::io::Error::other)?
}




/// The uid/gid a device node currently has.
async fn previous_owner(path: &Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let meta = tokio::fs::metadata(path).await.ok()?;
    Some((meta.uid(), meta.gid()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owner_directory_cannot_escape_the_root() {
        // Every one of these must end up a single, harmless component. ".." is
        // the dangerous one: dots are allowed in names so that identities like
        // `pi-4f2a.2` work, and filtering alone would let ".." straight through.
        for hostile in ["../../etc/passwd", "..", ".", "/etc", "a/b", ""] {
            let dir = owner_dir(SessionId(3), hostile);
            assert!(!dir.contains('/'), "{hostile:?} produced {dir:?}");
            assert_ne!(dir, "..", "{hostile:?} produced the parent directory");
            assert_ne!(dir, ".", "{hostile:?} produced the current directory");
            assert!(benchd_core::model::valid_component(&dir), "{hostile:?} -> {dir:?}");
        }
    }

    #[test]
    fn an_ordinary_identity_is_used_as_the_directory_name() {
        // It has to be predictable: the sandbox bind-mounts this path before the
        // coordinator has issued a session id.
        assert_eq!(owner_dir(SessionId(3), "pi-4f2a"), "pi-4f2a");
        assert_eq!(owner_dir(SessionId(3), "agent_1.2"), "agent_1.2");
    }

    #[test]
    fn an_empty_name_still_yields_a_directory() {
        assert_eq!(owner_dir(SessionId(7), "!!!"), "s7");
    }
}
