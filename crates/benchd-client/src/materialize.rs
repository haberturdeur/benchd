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
//! **A symlink to the imported node, with the access control on the node
//! itself.** The lease path is a symlink to `/dev/ttyACM0`; the imported node
//! is given to the agent's uid at `0600` for as long as the lease lasts, and
//! put back on release.
//!
//! This was a private `mknod` until it was measured against real tools, and
//! that is what it cost: a node reachable only through the lease directory is
//! also a node that *no enumeration lists*. Tools that identify a device by
//! looking around the system rather than by opening the path they were handed
//! then misbehave in ways that have nothing to do with permissions. `esptool`
//! resolves a console's USB vendor and product id by matching the path against
//! pyserial's port list, which globs `/dev/tty*`; against a private node it
//! finds nothing, silently assumes a USB-UART bridge, and both picks a reset
//! sequence that cannot reach the bootloader of a native-USB part and skips
//! disabling the RTC watchdog that such a part needs disabled *while flashing*.
//! `usbsdmux` has the same shape for a different reason (D24). Goal 2 is that
//! normal tools work, so the private node was failing the goal it existed for.
//!
//! Giving away the imported node instead is much less of a concession than it
//! reads, and the reason is the same fact that made locking it safe: this is
//! never the machine's own hardware. Every node here belongs to a device that
//! exists on this machine *only because this lease imported it* (see
//! [`benchd_core::sysfs::wait_for_vhci_node`]) and that vanishes when the lease
//! detaches its vhci port. Its lifetime is already exactly the lease's, and
//! there is no other user of it to protect it from — so `0600` to the leasing
//! uid gives that uid exactly what the private node gave it, at a path the rest
//! of the system can also see and describe. The old objection to sharing the
//! inode was really an objection to *inheriting* the source's group and mode,
//! which handed the board to all of `dialout` or `disk`; those are overwritten
//! here rather than kept.
//!
//! What is given up is that a stale reference now has a second way to be wrong.
//! The lease path still fails `ENOENT` once the lease ends, but a tool that
//! resolved it to `/dev/ttyACM0` and cached *that* could reach a later lease's
//! board, since kernel names are reused. A different agent is stopped by the
//! ownership; the same uid is not. Judged acceptable: agents are handed
//! `$LAB_DUT_*` and no tool in this toolchain persists a realpath across leases.
//!
//! Two requirements disappear with the private node, both of them sharp edges:
//! the lease tree no longer has to be on a filesystem mounted without `nodev`
//! (`/run` is not, by default, and a node created there could never be opened),
//! and materialisation no longer needs `CAP_MKNOD` — only `CAP_CHOWN`.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::ids::LeaseKey;
use benchd_core::lease::SessionId;
use benchd_core::sysfs;
use benchd_core::usbip;
use benchd_core::wire::{ChannelHello, ChannelSide, Outcome, ResourceHandle, WantedNode};
use futures::SinkExt;
use tokio_util::codec::{FramedWrite, LinesCodec};

/// Every directory this daemon creates under its root.
///
/// Explicit because nothing sets a umask and the systemd unit does not either,
/// so `create_dir_all` would take `0o777 & ~umask` — which is `0755` today and
/// world-writable the first time someone debugs the daemon from a shell with
/// `umask 0`. An agent that can write a lease directory can plant a symlink
/// where root will create the next one.
const DIR_MODE: u32 = 0o755;

/// A device node an agent has been given, and the ownership it had first.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BorrowedNode {
    path: PathBuf,
    uid: u32,
    gid: u32,
    mode: u32,
}

pub struct Materializer {
    root: PathBuf,
    /// A devtmpfs-backed tree which each sandbox mounts as its `/dev`.
    ///
    /// `/run` is `nodev`, so duplicate nodes cannot live beside the lease
    /// links. Keeping them under the real `/dev` also means a file created
    /// after the sandbox starts appears there immediately.
    device_root: PathBuf,
    /// Needed to dial a relay data channel, which goes to the coordinator that
    /// granted the lease rather than to any particular one.
    coordinators: Vec<crate::Coordinator>,
    /// Highest epoch seen per lease. The client is an executor too, and a
    /// delayed instruction for a superseded lease must be dropped rather than
    /// obeyed — obeying it would expose hardware whose lease is gone (D7).
    ///
    /// This is *not* what keeps a lease's own instructions in order: every
    /// instruction for one lease carries the same epoch (a lease's epochs never
    /// change after the claim), so the fence can never separate a `Materialize`
    /// from the `Unmaterialize` that undoes it. The dispatcher's per-lease queue
    /// does that. What is left for the fence is an instruction carrying an
    /// epoch older than one already obeyed, which means a coordinator
    /// incarnation this daemon has since stopped believing.
    seen: BTreeMap<LeaseKey, benchd_core::lease::Epoch>,
    active: BTreeMap<LeaseKey, PathBuf>,
    /// vhci ports this lease imported, needing an explicit detach.
    imported: BTreeMap<LeaseKey, Vec<u32>>,
    /// Imported `/dev` nodes made over to an agent for the duration of a lease,
    /// and the ownership to put back on release. Taking them over is what makes
    /// a lease exclusive; this is what stops that being permanent.
    restore: BTreeMap<LeaseKey, Vec<BorrowedNode>>,
    /// Per-owner copies of those nodes, under [`Self::device_root`].
    ///
    /// They carry the same device number, so they open the same driver, but
    /// only the owning sandbox has their directory mounted as `/dev`.
    mirrors: BTreeMap<LeaseKey, Vec<PathBuf>>,
    /// The same two things, on disk, so a restarted daemon can undo its own
    /// work and only its own.
    ledger: Ledger,
}

impl Materializer {
    pub fn new(
        root: impl Into<PathBuf>,
        device_root: impl Into<PathBuf>,
        coordinators: Vec<crate::Coordinator>,
    ) -> Self {
        let root = root.into();
        Materializer {
            ledger: Ledger::new(&root),
            root,
            device_root: device_root.into(),
            coordinators,
            seen: BTreeMap::new(),
            active: BTreeMap::new(),
            restore: BTreeMap::new(),
            mirrors: BTreeMap::new(),
            imported: BTreeMap::new(),
        }
    }

    /// Prepare the two live views a sandbox needs before it starts.
    pub async fn prepare_owner(&self, owner: &str) -> Result<(PathBuf, PathBuf), String> {
        let leases = self.root.join(owner);
        create_tree(&self.root, &leases).await?;

        let devices = self.device_root.join(owner);
        create_device_tree(&self.device_root, &devices).await?;
        prepare_standard_devices(&devices).await?;
        Ok((leases, devices))
    }

    /// Import a remote device and return the vhci port it landed on, with the
    /// USB speed needed to find that port again in sysfs.
    ///
    /// Dial out, complete the import handshake over the relayed connection, then
    /// hand the socket to vhci. No device node exists yet when this returns: the
    /// kernel enumerates asynchronously, and one device may produce several
    /// nodes, so resolving them is the caller's job.
    async fn import(
        &self,
        coordinator: crate::ids::CoordinatorId,
        channel: &benchd_core::wire::ChannelKey,
        busid: &str,
    ) -> Result<(u32, u32), String> {
        // The data channel goes to the coordinator that granted this lease, not
        // to whichever one happens to be first: the rendezvous key only exists
        // in that coordinator's relay.
        let address = self
            .coordinators
            .iter()
            .find(|c| c.id == coordinator)
            .map(|c| c.address.clone())
            .ok_or_else(|| format!("no address for coordinator {coordinator}"))?;
        let mut stream = tokio::net::TcpStream::connect(&address)
            .await
            .map_err(|e| format!("dialling the coordinator for a data channel: {e}"))?;
        stream.set_nodelay(true).ok();
        benchd_core::protocol::connect(&mut stream)
            .await
            .map_err(|err| err.to_string())?;
        let (read, write) = stream.into_split();

        let mut sink = FramedWrite::new(write, LinesCodec::new());
        let hello = ChannelHello {
            channel: channel.clone(),
            side: ChannelSide::Client,
        };
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

        let port = sysfs::free_vhci_port(device.speed)
            .await
            .map_err(|e| e.to_string())?;

        sysfs::vhci_attach(port, stream, device.devid(), device.speed)
            .await
            .map_err(|e| format!("vhci attach on port {port}: {e}"))?;
        tracing::info!(
            port,
            devid = device.devid(),
            "attached; waiting for enumeration"
        );
        Ok((port, device.speed))
    }

    fn lease_dir(&self, owner: &str, lease: LeaseKey) -> PathBuf {
        // The coordinator is part of the path: two coordinators both issue l1,
        // and without it their device nodes would land on top of each other.
        self.root
            .join(owner)
            .join(format!("{}-{}", lease.coordinator, lease.lease))
    }

    /// Forget everything belonging to ONE coordinator, and release its devices.
    ///
    /// Per coordinator, not global: a blip on the shared lab server must not
    /// tear down leases granted by the local coordinator that owns this
    /// operator's own boards. Those are independent authorities and one being
    /// unreachable says nothing about the other.
    pub async fn clear_coordinator(&mut self, coordinator: crate::ids::CoordinatorId) {
        let mine: Vec<LeaseKey> = self
            .active
            .keys()
            .chain(self.imported.keys())
            .chain(self.seen.keys())
            .filter(|k| k.coordinator == coordinator)
            .copied()
            .collect();
        for lease in mine {
            self.unmaterialize_now(lease, "").await;
            self.seen.remove(&lease);
        }
    }

    /// Clear everything, including kernel state left by a previous process.
    ///
    /// Device nodes and vhci attachments outlive the process that made them, so
    /// without this a restart would leave hardware reachable by an agent whose
    /// lease is gone (D6).
    pub async fn clear_stale(&mut self) {
        // Every lease every coordinator granted is void, so none of the
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
        self.mirrors.clear();

        // Mirrors are only names for nodes tracked by the ledger. They are
        // never useful across a daemon restart, and leaving an old ttyACM0 in
        // an owner's private /dev could point at a later import reusing that
        // device number.
        if let Err(err) = tokio::fs::remove_dir_all(&self.device_root).await {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %self.device_root.display(),
                    ?err,
                    "could not clear stale sandbox device tree"
                );
            }
        }
        if let Err(err) = tokio::fs::create_dir_all(&self.device_root).await {
            tracing::error!(
                path = %self.device_root.display(),
                ?err,
                "could not create sandbox device root"
            );
        }
        let _ = set_mode(&self.device_root, DIR_MODE).await;

        // What a previous incarnation did to this machine. Not the same
        // question as what the machine currently has attached: an engineer's
        // hand-run `usbip attach`, a second client daemon and a co-located host
        // all present as a port in `VDEV_ST_USED` and none of them is ours to
        // take away. Detaching the lot cost someone their debugging session
        // every time `deploy.sh` restarted this daemon.
        let previous = self.ledger.recover().await;

        // Ownership first: detaching a port takes its node with it, and then
        // there is nothing left to give back.
        for node in &previous.nodes {
            if let Err(err) = restore_node(node).await {
                tracing::warn!(path = %node.path.display(), ?err, "could not unlock a device left over from a previous run");
            }
        }
        for port in sysfs::attached_ports().await {
            if !previous.ports.contains(&port) {
                tracing::info!(
                    port,
                    "leaving an imported device alone: this daemon did not attach it"
                );
                continue;
            }
            tracing::warn!(
                port,
                "detaching a stale imported device from a previous run"
            );
            sysfs::vhci_detach(port).await;
        }
        self.ledger.clear().await;

        let Ok(mut owners) = tokio::fs::read_dir(&self.root).await else {
            return;
        };
        while let Ok(Some(owner)) = owners.next_entry().await {
            if owner.file_name() == LEDGER_DIR {
                continue;
            }
            let Ok(mut leases) = tokio::fs::read_dir(owner.path()).await else {
                continue;
            };
            while let Ok(Some(lease)) = leases.next_entry().await {
                tracing::warn!(path = %lease.path().display(), "clearing a stale lease directory");
                remove_tree(&lease.path()).await;
            }
        }
    }

    /// Accept an instruction only if it is at least as new as anything we have
    /// already seen for this lease.
    fn fence(&mut self, lease: LeaseKey, epoch: benchd_core::lease::Epoch) -> Result<(), Outcome> {
        let seen = self
            .seen
            .entry(lease)
            .or_insert(benchd_core::lease::Epoch(0));
        if epoch < *seen {
            tracing::warn!(%lease, ?epoch, ?seen, "dropping a stale instruction");
            return Err(Outcome::Stale { seen: *seen });
        }
        *seen = epoch;
        Ok(())
    }

    pub async fn materialize(
        &mut self,
        lease: LeaseKey,
        epoch: benchd_core::lease::Epoch,
        owner: &str,
        uid: Option<u32>,
        slots: &BTreeMap<String, BTreeMap<String, ResourceHandle>>,
    ) -> Outcome {
        if let Err(stale) = self.fence(lease, epoch) {
            return stale;
        }
        let dir = self.lease_dir(owner, lease);

        // Resolve everything before creating anything: a half-materialised
        // lease is worse than a failed one, because the agent would find some
        // of its devices and reasonably assume it had them all.
        let mut plan: Vec<(PathBuf, PathBuf)> = Vec::new();
        // USB/IP forwards whole devices, so resources that share a channel share
        // one import: a USB-SD-Mux arrives once and yields both the SCSI node
        // that switches the card and the block node that holds it. Importing per
        // resource instead would leave the second attempt waiting for a device
        // the host has already handed over.
        let mut imported: BTreeMap<&benchd_core::wire::ChannelKey, (u32, u32)> = BTreeMap::new();
        for (slot, resources) in slots {
            for (name, handle) in resources {
                // This process is root and about to create a symlink at a
                // path built from these names. They arrive from the
                // coordinator, which validates them — but a privileged daemon
                // that trusts its input has no business being privileged, so
                // check again.
                if !benchd_core::model::valid_component(slot)
                    || !benchd_core::model::valid_component(name)
                {
                    self.roll_back(lease, owner).await;
                    return Outcome::Failed {
                        detail: format!(
                            "refusing to materialise {slot:?}/{name:?}: \
                             not a plain path component"
                        ),
                    };
                }
                let ResourceHandle::UsbIp {
                    channel,
                    busid,
                    node,
                } = handle;
                let (port, speed) = match imported.get(channel) {
                    Some(&already) => already,
                    None => match self.import(lease.coordinator, channel, busid).await {
                        Ok(fresh) => {
                            // Remembered before anything else can fail. An
                            // attach nothing knows about is a device left
                            // reachable with no lease behind it, and the
                            // startup sweep can only undo what it can
                            // recognise as ours.
                            self.imported.entry(lease).or_default().push(fresh.0);
                            self.ledger.record_port(lease, fresh.0).await;
                            imported.insert(channel, fresh);
                            fresh
                        }
                        Err(detail) => {
                            self.roll_back(lease, owner).await;
                            return Outcome::Failed {
                                detail: format!("{slot}/{name}: {detail}"),
                            };
                        }
                    },
                };

                // Located by vhci port rather than by name: a forwarded device
                // reproduces the by-id name of the one that just vanished from
                // the host, and this machine has storage device names of its own
                // that an imported card would collide with.
                match sysfs::wait_for_vhci_node(
                    port,
                    speed,
                    *node,
                    std::time::Duration::from_secs(10),
                )
                .await
                {
                    Some(source) => plan.push((source, dir.join(slot).join(name))),
                    None => {
                        self.roll_back(lease, owner).await;
                        return Outcome::Failed {
                            detail: format!("{slot}/{name}: {}", missing_node(port, *node)),
                        };
                    }
                }
            }
        }

        // A resource path is enough for serial/storage tools. libusb tools
        // open the imported device's usbfs node instead, so reserve and mirror
        // one of those per imported USB device too.
        let mut usb_nodes = Vec::new();
        for &(port, speed) in imported.values() {
            match sysfs::wait_for_vhci_usb_node(port, speed, std::time::Duration::from_secs(10))
                .await
            {
                Some(source) => usb_nodes.push(source),
                None => {
                    // A serial/block resource remains completely usable without
                    // usbfs. Treat this as optional compatibility for libusb
                    // tools rather than failing an otherwise healthy lease.
                    tracing::warn!(
                        %lease,
                        port,
                        "imported device has no /dev/bus/usb node; libusb tools \
                         will not work in the sandbox"
                    );
                }
            }
        }

        for (source, dest) in &plan {
            if let Err(detail) = self.take_over(lease, owner, source, dest, uid).await {
                // Roll back, so a failure never leaves a partial lease behind.
                self.roll_back(lease, owner).await;
                return Outcome::Failed { detail };
            }
        }
        for source in &usb_nodes {
            if let Err(detail) = self.take_over_device(lease, owner, source, uid).await {
                self.roll_back(lease, owner).await;
                return Outcome::Failed { detail };
            }
        }

        self.active.insert(lease, dir.clone());
        tracing::info!(%lease, %owner, path = %dir.display(), nodes = plan.len(), "materialized");
        Outcome::Ok
    }

    /// Give one imported device to the agent that leased it.
    ///
    /// Two halves: the imported node in `/dev` becomes the agent's alone, and
    /// the lease path points at it. The ownership is the half that enforces
    /// anything — the symlink is a stable, lease-scoped name for a node whose
    /// kernel name is neither.
    async fn take_over(
        &mut self,
        lease: LeaseKey,
        owner: &str,
        source: &Path,
        dest: &Path,
        uid: Option<u32>,
    ) -> Result<(), String> {
        link_node(&self.root, source, dest).await?;
        self.take_over_device(lease, owner, source, uid).await
    }

    /// Reserve one imported node and reproduce its kernel name in the owner's
    /// sandbox-only `/dev`.
    async fn take_over_device(
        &mut self,
        lease: LeaseKey,
        owner: &str,
        source: &Path,
        uid: Option<u32>,
    ) -> Result<(), String> {
        let facts = node_facts(source)
            .await
            .ok_or_else(|| format!("{} is not a device node", source.display()))?;
        let mirror = mirror_path(&self.device_root, owner, source)?;
        let borrowed = self.restore.entry(lease).or_default();
        if needs_locking(borrowed, source) {
            let borrowed = BorrowedNode {
                path: source.to_path_buf(),
                uid: facts.uid,
                gid: facts.gid,
                mode: facts.mode,
            };
            // Recorded before the change is made, not after: a crash in between
            // must leave a record that restores the original, never one that
            // has forgotten a node it took.
            self.restore
                .entry(lease)
                .or_default()
                .push(borrowed.clone());
            self.ledger.record_node(lease, &borrowed).await;
            // Fatal, unlike the root-locking this replaced. Then the agent had
            // its own node whatever happened here and the usual cause was a
            // board unplugged a moment ago; now this *is* the agent's access,
            // and a lease that reports success over a node its owner cannot
            // open is the failure mode hardest to diagnose from the agent's
            // side.
            reserve_source(source, uid)
                .await
                .map_err(|e| format!("giving {} to its lease: {e}", source.display()))?;
            match uid {
                // Named so that grepping the journal for a /dev path says which
                // lease holds it and where the agent's own name for it is. The
                // refusal of anyone else happens in the kernel, with nothing of
                // ours on the stack to report it, so this line is the only
                // account of it that exists.
                Some(uid) => tracing::info!(
                    %lease, uid,
                    device = %source.display(),
                    sandbox_node = %mirror.display(),
                    "reserved an imported device for its lease"
                ),
                None => tracing::warn!(
                    %lease,
                    device = %source.display(),
                    "no uid known for the session holding this lease: the device \
                     stays root-only and the agent will not be able to open it"
                ),
            }
        }

        // Expose the private name only after the source is reserved. A sandbox
        // may already be watching its /dev while a claim is materialised;
        // creating this first would briefly hand it a usable, unreserved node.
        mirror_node(&self.device_root, source, &mirror, uid)
            .await
            .map_err(|e| format!("mirroring {} into the sandbox: {e}", source.display()))?;
        if !self.mirrors.entry(lease).or_default().contains(&mirror) {
            self.mirrors.entry(lease).or_default().push(mirror.clone());
        }
        Ok(())
    }

    /// Undo everything done for one lease, whether or not it ever completed.
    async fn roll_back(&mut self, lease: LeaseKey, owner: &str) {
        for mirror in self.mirrors.remove(&lease).unwrap_or_default() {
            if let Err(err) = tokio::fs::remove_file(&mirror).await {
                if err.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %mirror.display(), ?err, "could not remove sandbox device");
                }
            }
        }
        // Give the devices back first: after the detach below the nodes are
        // gone and there is nothing left to give back to.
        for node in self.restore.remove(&lease).unwrap_or_default() {
            if let Err(err) = restore_node(&node).await {
                tracing::warn!(path = %node.path.display(), ?err, "could not unlock a device");
            }
        }
        // Fall back to the computed path so a restarted daemon can still clean
        // up a lease it does not remember.
        let dir = self
            .active
            .remove(&lease)
            .unwrap_or_else(|| self.lease_dir(owner, lease));
        remove_tree(&dir).await;
        // Detach last: the node is what the agent holds, and the vhci port is
        // what the kernel holds.
        for port in self.imported.remove(&lease).unwrap_or_default() {
            sysfs::vhci_detach(port).await;
        }
        self.ledger.forget(lease).await;
    }

    /// URB completion counts for every imported lease, summed across its ports.
    pub async fn urb_totals(&self) -> BTreeMap<LeaseKey, u64> {
        let mut totals = BTreeMap::new();
        for (lease, ports) in &self.imported {
            let mut n = 0u64;
            for port in ports {
                n = n.saturating_add(sysfs::vhci_urbnum(*port).await.unwrap_or(0));
            }
            totals.insert(*lease, n);
        }
        totals
    }

    pub async fn unmaterialize(
        &mut self,
        lease: LeaseKey,
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
    ///
    /// Idempotent: the reaper races voluntary releases, and neither path may
    /// fail.
    pub async fn unmaterialize_now(&mut self, lease: LeaseKey, owner: &str) -> Outcome {
        self.roll_back(lease, owner).await;
        tracing::info!(%lease, %owner, "unmaterialized");
        Outcome::Ok
    }
}

/// Why a node did not appear, phrased for whoever has to fix it.
///
/// The import succeeded, so the fault is on this machine: a driver the kernel
/// does not have, or a bench that names an interface the device does not offer.
fn missing_node(port: u32, want: WantedNode) -> String {
    let (what, hint) = match want {
        WantedNode::Tty {
            interface: Some(interface),
        } => (
            format!("no serial device on USB interface {interface}"),
            "the device may not have that interface, or usb-serial support for it is missing",
        ),
        WantedNode::Tty { interface: None } => (
            "no serial device".to_string(),
            "is the right usb-serial driver available on this machine?",
        ),
        WantedNode::Block => (
            "no block device".to_string(),
            "usb-storage support is needed to see the storage behind a device",
        ),
        WantedNode::Scsi => (
            "no SCSI generic device".to_string(),
            "sg support is needed; without it the device cannot be sent control commands",
        ),
    };
    format!("imported on vhci port {port} but {what} appeared: {hint}")
}

/// Whether a source node still has to be taken away from the machine.
///
/// One lease can reach the same node twice. The USB-SD-Mux is the reason the
/// case exists at all — one physical device backing two resources — and there
/// the two nodes differ, but nothing stops a bench naming the same one for two
/// slots. Locking it twice would record `root:root 0600` as the state to
/// restore, and release would then leave a board only root can open, which is a
/// bench quietly lost.
fn needs_locking(borrowed: &[BorrowedNode], source: &Path) -> bool {
    !borrowed.iter().any(|node| node.path == source)
}

/// Point the lease path at the imported device.
///
/// A symlink rather than a copy of the device address, so that everything
/// resolving this path — the kernel, `udev`, pyserial's port list — arrives at
/// the same `/dev` node the device would have if it were plugged in here. That
/// is the whole point; see the module docs.
async fn link_node(root: &Path, source: &Path, dest: &Path) -> Result<(), String> {
    let dest = resolved_dest(root, dest).await?;
    // A lease directory can outlive the daemon that made it, and `symlink`
    // refuses a path that exists.
    if let Err(err) = tokio::fs::remove_file(&dest).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            return Err(format!("clearing {}: {err}", dest.display()));
        }
    }
    tokio::fs::symlink(source, &dest)
        .await
        .map_err(|e| format!("linking {} to {}: {e}", dest.display(), source.display()))
}

/// Where a resource's lease path really goes, with the whole path resolved.
///
/// `Path::starts_with` compares unresolved components, so a symlink planted at
/// any component of the destination passes it while root creates the lease path
/// somewhere else entirely — and root creating a symlink at an attacker-chosen
/// path is a way to be pointed at a file the next `chown` then hands away.
/// Every component is therefore created here, refused if it is anything but a
/// real directory, and the result checked again after resolution.
async fn resolved_dest(root: &Path, dest: &Path) -> Result<PathBuf, String> {
    let outside = || {
        format!(
            "refusing to create a lease path outside {}: {}",
            root.display(),
            dest.display()
        )
    };
    // Lexical first, so an obviously wrong path never reaches `mkdir`.
    if !dest.starts_with(root) {
        return Err(outside());
    }
    let (Some(parent), Some(name)) = (dest.parent(), dest.file_name()) else {
        return Err(outside());
    };
    create_tree(root, parent).await?;

    let real_root = tokio::fs::canonicalize(root)
        .await
        .map_err(|e| format!("resolving {}: {e}", root.display()))?;
    let real_parent = tokio::fs::canonicalize(parent)
        .await
        .map_err(|e| format!("resolving {}: {e}", parent.display()))?;
    if !real_parent.starts_with(&real_root) {
        return Err(format!(
            "refusing to create a lease path outside {}: {} resolves to {}",
            root.display(),
            dest.display(),
            real_parent.join(name).display()
        ));
    }
    Ok(real_parent.join(name))
}

/// Create the lease and slot directories, one component at a time.
///
/// Component by component so that each can be given an explicit mode and
/// checked for being a directory rather than a symlink to one; `create_dir_all`
/// can do neither.
async fn create_tree(root: &Path, dir: &Path) -> Result<(), String> {
    let relative = dir
        .strip_prefix(root)
        .map_err(|_| format!("{} is not under {}", dir.display(), root.display()))?;
    let mut at = root.to_path_buf();
    for part in relative.components() {
        let Component::Normal(part) = part else {
            return Err(format!(
                "refusing to create {}: {:?} is not a plain path component",
                dir.display(),
                part.as_os_str()
            ));
        };
        at.push(part);
        match tokio::fs::DirBuilder::new()
            .mode(DIR_MODE)
            .create(&at)
            .await
        {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(format!("mkdir {}: {err}", at.display())),
        }
        // A component that is not a directory is one somebody else put there —
        // and a symlink is the interesting case, because root would follow it.
        let meta = tokio::fs::symlink_metadata(&at)
            .await
            .map_err(|e| format!("stat {}: {e}", at.display()))?;
        if !meta.is_dir() {
            return Err(format!("{} is not a directory", at.display()));
        }
        // `mkdir`'s mode is masked by the umask and an existing directory keeps
        // whatever mode it already had, so neither is enough on its own.
        set_mode(&at, DIR_MODE)
            .await
            .map_err(|e| format!("chmod {}: {e}", at.display()))?;
    }
    Ok(())
}

/// Create a root-owned directory tree for a sandbox's private `/dev`.
async fn create_device_tree(root: &Path, dir: &Path) -> Result<(), String> {
    match tokio::fs::DirBuilder::new()
        .mode(DIR_MODE)
        .recursive(true)
        .create(root)
        .await
    {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(format!("mkdir {}: {err}", root.display())),
    }
    let meta = tokio::fs::symlink_metadata(root)
        .await
        .map_err(|e| format!("stat {}: {e}", root.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    set_mode(root, DIR_MODE)
        .await
        .map_err(|e| format!("chmod {}: {e}", root.display()))?;
    create_tree(root, dir).await
}

/// Basic `/dev` entries needed by shells and agent runtimes.
///
/// The owner's directory is mounted over `/dev`, so these cannot come from
/// bubblewrap's usual `--dev /dev`. PTYs are supplied by a separate bind of
/// `/dev/pts`; the stable devices are cheap and safe to reproduce here.
async fn prepare_standard_devices(dev: &Path) -> Result<(), String> {
    create_device_tree(dev, &dev.join("pts")).await?;
    create_device_tree(dev, &dev.join("shm")).await?;

    for name in ["null", "zero", "full", "random", "urandom", "tty"] {
        let source = PathBuf::from("/dev").join(name);
        let facts = node_facts(&source)
            .await
            .ok_or_else(|| format!("{} is not a device node", source.display()))?;
        make_node(&dev.join(name), &facts, Some(0), facts.mode).await?;
    }

    for (name, target) in [
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
        ("ptmx", "pts/ptmx"),
    ] {
        let path = dev.join(name);
        match tokio::fs::symlink_metadata(&path).await {
            Ok(meta) if meta.file_type().is_symlink() => {
                if tokio::fs::read_link(&path).await.ok().as_deref() == Some(Path::new(target)) {
                    continue;
                }
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|e| format!("clearing {}: {e}", path.display()))?;
            }
            Ok(_) => {
                return Err(format!(
                    "refusing to replace non-symlink standard device {}",
                    path.display()
                ));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("stat {}: {err}", path.display())),
        }
        tokio::fs::symlink(target, &path)
            .await
            .map_err(|e| format!("linking {} to {target}: {e}", path.display()))?;
    }
    Ok(())
}

fn mirror_path(device_root: &Path, owner: &str, source: &Path) -> Result<PathBuf, String> {
    let relative = source
        .strip_prefix("/dev")
        .map_err(|_| format!("{} is not under /dev", source.display()))?;
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(format!(
            "{} is not a plain path below /dev",
            source.display()
        ));
    }
    Ok(device_root.join(owner).join(relative))
}

async fn mirror_node(
    device_root: &Path,
    source: &Path,
    dest: &Path,
    uid: Option<u32>,
) -> Result<(), String> {
    let facts = node_facts(source)
        .await
        .ok_or_else(|| format!("{} is not a device node", source.display()))?;
    let parent = dest
        .parent()
        .ok_or_else(|| format!("{} has no parent", dest.display()))?;
    create_device_tree(device_root, parent).await?;
    make_node(dest, &facts, uid, 0o600).await
}

async fn make_node(
    dest: &Path,
    facts: &NodeFacts,
    uid: Option<u32>,
    mode: u32,
) -> Result<(), String> {
    if let Err(err) = tokio::fs::remove_file(dest).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            return Err(format!("clearing {}: {err}", dest.display()));
        }
    }
    let path = dest.to_path_buf();
    let kind = facts.kind;
    let rdev = facts.rdev;
    tokio::task::spawn_blocking(move || {
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &path,
            kind,
            rustix::fs::Mode::from_bits_retain(mode),
            rdev,
        )
    })
    .await
    .map_err(|e| format!("creating {}: {e}", dest.display()))?
    .map_err(|e| format!("creating {}: {e}", dest.display()))?;
    chown(dest, uid.unwrap_or(0), 0)
        .await
        .map_err(|e| format!("chown {}: {e}", dest.display()))?;
    set_mode(dest, mode)
        .await
        .map_err(|e| format!("chmod {}: {e}", dest.display()))
}

/// Reserve an imported device node for the agent that leased it.
///
/// The lease is exclusive over the bench, so for as long as it lasts nothing
/// else on this machine has any business opening this node — and the one thing
/// that does have business with it is the agent, which is why the node becomes
/// the agent's rather than root's.
///
/// Without a uid it is locked to root instead. That leaves the agent unable to
/// open its own lease, which the caller reports; the alternative is leaving the
/// source's `dialout` or `disk` group in place so that access happens to work,
/// and handing a board to everyone in that group is the defect being fixed.
async fn reserve_source(source: &Path, uid: Option<u32>) -> std::io::Result<()> {
    // Mode first, and it is not merely tidiness: it closes group access while
    // the node is still root's, so there is no instant at which the node is
    // both group-readable and owned by somebody new. The source's own group is
    // `dialout` or `disk`, so that instant would be the whole group's.
    set_mode(source, 0o600).await?;
    chown(source, uid.unwrap_or(0), 0).await
}

/// Put a borrowed device node back the way it was found.
///
/// A node that is simply gone is the ordinary case rather than an error: a
/// device unplugged while leased takes its node with it, and so does detaching
/// the vhci port the import landed on.
async fn restore_node(node: &BorrowedNode) -> std::io::Result<()> {
    let result = async {
        chown(&node.path, node.uid, node.gid).await?;
        set_mode(&node.path, node.mode).await
    }
    .await;
    match result {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(path = %node.path.display(), "the device is gone; nothing to give back");
            Ok(())
        }
        other => other,
    }
}

async fn chown(path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || std::os::unix::fs::chown(&path, Some(uid), Some(gid)))
        .await
        .map_err(std::io::Error::other)?
}

async fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    tokio::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode)).await
}

/// The ownership a device node had before a lease took it, which is all that
/// has to be remembered in order to give it back.
#[derive(Clone, Debug, PartialEq, Eq)]
struct NodeFacts {
    uid: u32,
    gid: u32,
    mode: u32,
    kind: rustix::fs::FileType,
    rdev: rustix::fs::Dev,
}

async fn node_facts(path: &Path) -> Option<NodeFacts> {
    use std::os::unix::fs::MetadataExt;
    // Not `metadata`: a source that is a symlink is not a device node, and
    // following one would describe — and later hand out — whatever it happens
    // to point at.
    let meta = tokio::fs::symlink_metadata(path).await.ok()?;
    if !is_device_node(&meta) {
        return None;
    }
    Some(NodeFacts {
        uid: meta.uid(),
        gid: meta.gid(),
        mode: meta.mode() & 0o7777,
        kind: rustix::fs::FileType::from_raw_mode(meta.mode()),
        rdev: meta.rdev(),
    })
}

/// Whether the kernel would call this a device rather than a file.
fn is_device_node(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    let kind = meta.file_type();
    kind.is_char_device() || kind.is_block_device()
}

/// Remove a lease directory and everything in it.
///
/// Failures are logged, never propagated: the reaper must always be able to
/// finish, and a stuck unmount must not wedge the daemon.
async fn remove_tree(dir: &Path) {
    let Ok(mut slots) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(slot)) = slots.next_entry().await {
        let Ok(mut resources) = tokio::fs::read_dir(slot.path()).await else {
            continue;
        };
        while let Ok(Some(resource)) = resources.next_entry().await {
            // A node this daemon created is an ordinary file on the same
            // filesystem as its directory and is just unlinked below. This is
            // for the other case: a lease directory left behind by a build that
            // still bind-mounted the device inode, which `remove_dir_all` would
            // fail on for as long as the mount existed.
            if !mounted_over(&slot.path(), &resource.path()).await {
                continue;
            }
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

/// Whether something is mounted at `path` rather than being a file in `parent`.
///
/// A mount point is on a different filesystem than the directory it sits in,
/// which is exactly what a bind mount of a `/dev` inode looks like from here.
async fn mounted_over(parent: &Path, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(parent), Ok(path)) = (
        tokio::fs::symlink_metadata(parent).await,
        tokio::fs::symlink_metadata(path).await,
    ) else {
        return false;
    };
    parent.dev() != path.dev()
}

/// The path an agent should use for a materialised resource.
pub fn resource_path(
    root: &Path,
    owner: &str,
    lease: LeaseKey,
    slot: &str,
    resource: &str,
) -> PathBuf {
    root.join(owner)
        .join(format!("{}-{}", lease.coordinator, lease.lease))
        .join(slot)
        .join(resource)
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

/// Where the ledger lives, under the root.
///
/// The colon is deliberate and load-bearing: [`owner_dir`] filters it out, so
/// no agent identity can ever name this directory and collide with it.
const LEDGER_DIR: &str = "kernel:undo";

/// What this daemon has done to the machine that outlives the process.
///
/// Two things need undoing after an unclean shutdown, and neither is visible
/// from a fresh process: the vhci ports it attached, and the `/dev` nodes whose
/// ownership it took away. The kernel remembers both but not who is
/// responsible — the vhci status table says a port is in `VDEV_ST_USED` and
/// nothing more, so an engineer's hand-run `usbip attach`, a second client
/// daemon and a co-located host all look exactly like our own leftovers.
///
/// One file per lease, appended to as the lease is built up, so a crash halfway
/// through leaves a record of what had already happened rather than nothing.
///
/// Lives under the root, which is a tmpfs in every real deployment — so the
/// record has precisely the lifetime of the kernel state it describes, because
/// a reboot clears both. On a root that is *not* a tmpfs it survives a reboot
/// and describes ports that no longer exist, which costs one skipped detach
/// each and nothing else.
struct Ledger {
    dir: PathBuf,
}

/// Kernel state a previous incarnation of this daemon left behind.
#[derive(Debug, Default, PartialEq, Eq)]
struct Abandoned {
    ports: Vec<u32>,
    nodes: Vec<BorrowedNode>,
}

impl Ledger {
    fn new(root: &Path) -> Ledger {
        Ledger {
            dir: root.join(LEDGER_DIR),
        }
    }

    /// Create the directory, root-only: it names every device this machine has
    /// imported and where its node is, and no agent has any use for that.
    async fn prepare(&self) -> std::io::Result<()> {
        match tokio::fs::DirBuilder::new()
            .mode(0o700)
            .create(&self.dir)
            .await
        {
            Err(err) if err.kind() != std::io::ErrorKind::AlreadyExists => return Err(err),
            _ => {}
        }
        set_mode(&self.dir, 0o700).await
    }

    fn file(&self, lease: LeaseKey) -> PathBuf {
        self.dir
            .join(format!("{}-{}", lease.coordinator, lease.lease))
    }

    async fn record_port(&self, lease: LeaseKey, port: u32) {
        self.append(lease, &format!("port {port}\n")).await;
    }

    async fn record_node(&self, lease: LeaseKey, node: &BorrowedNode) {
        // The path goes last, so a path containing a space still parses.
        self.append(
            lease,
            &format!(
                "node {} {} {:o} {}\n",
                node.uid,
                node.gid,
                node.mode,
                node.path.display()
            ),
        )
        .await;
    }

    async fn append(&self, lease: LeaseKey, line: &str) {
        if let Err(err) = self.try_append(lease, line).await {
            // Not fatal, but it does mean an unclean shutdown will leave this
            // behind: worth saying out loud rather than at debug level.
            tracing::warn!(%lease, ?err, "could not record what this lease has attached");
        }
    }

    async fn try_append(&self, lease: LeaseKey, line: &str) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        self.prepare().await?;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.file(lease))
            .await?;
        file.write_all(line.as_bytes()).await?;
        // Flushed, not synced: the record only has to beat this process's own
        // death, and `/run` is a tmpfs where a sync means nothing anyway.
        file.flush().await
    }

    async fn forget(&self, lease: LeaseKey) {
        if let Err(err) = tokio::fs::remove_file(self.file(lease)).await {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%lease, ?err, "could not drop a lease's record");
            }
        }
    }

    /// Everything a previous incarnation left attached or locked.
    async fn recover(&self) -> Abandoned {
        let mut found = Abandoned::default();
        let Ok(mut entries) = tokio::fs::read_dir(&self.dir).await else {
            return found;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(text) = tokio::fs::read_to_string(entry.path()).await else {
                continue;
            };
            parse_records(&text, &mut found);
        }
        found
    }

    async fn clear(&self) {
        let Ok(mut entries) = tokio::fs::read_dir(&self.dir).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

/// Read one ledger file. Unparseable lines are skipped rather than fatal: a
/// half-written last line is exactly what a crash leaves behind.
fn parse_records(text: &str, into: &mut Abandoned) {
    for line in text.lines() {
        let mut fields = line.split(' ');
        match fields.next() {
            Some("port") => {
                if let Some(port) = fields.next().and_then(|p| p.parse().ok()) {
                    into.ports.push(port);
                }
            }
            Some("node") => {
                let (Some(uid), Some(gid), Some(mode)) =
                    (fields.next(), fields.next(), fields.next())
                else {
                    continue;
                };
                let rest: Vec<&str> = fields.collect();
                let (Ok(uid), Ok(gid), Ok(mode)) =
                    (uid.parse(), gid.parse(), u32::from_str_radix(mode, 8))
                else {
                    continue;
                };
                if rest.is_empty() {
                    continue;
                }
                into.nodes.push(BorrowedNode {
                    path: PathBuf::from(rest.join(" ")),
                    uid,
                    gid,
                    mode,
                });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A directory to make a mess in, removed when the test ends.
    ///
    /// Hand-rolled because this crate has no dev-dependencies and adding
    /// `tempfile` is a manifest change; it needs to do very little.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(what: &str) -> TempTree {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "benchd-{what}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempTree(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }

    fn lease(n: u64) -> LeaseKey {
        LeaseKey::new(crate::ids::CoordinatorId(0), benchd_core::lease::LeaseId(n))
    }

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
            assert!(
                benchd_core::model::valid_component(&dir),
                "{hostile:?} -> {dir:?}"
            );
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

    #[test]
    fn a_sandbox_device_keeps_the_kernels_name_below_its_owner() {
        let root = Path::new("/dev/benchd");
        assert_eq!(
            mirror_path(root, "agent-1", Path::new("/dev/ttyACM0")).unwrap(),
            Path::new("/dev/benchd/agent-1/ttyACM0")
        );
        assert_eq!(
            mirror_path(root, "agent-1", Path::new("/dev/bus/usb/003/017")).unwrap(),
            Path::new("/dev/benchd/agent-1/bus/usb/003/017")
        );
    }

    #[test]
    fn a_sandbox_device_can_only_come_from_dev() {
        assert!(mirror_path(
            Path::new("/dev/benchd"),
            "agent-1",
            Path::new("/etc/shadow")
        )
        .is_err());
    }

    #[test]
    fn no_agent_identity_can_name_the_ledger_directory() {
        // The ledger sits beside the owner directories, and the sweep skips it
        // by name. An agent that could take that name would have its leases
        // parsed as records and its directory cleared out from under it.
        assert_ne!(owner_dir(SessionId(1), LEDGER_DIR), LEDGER_DIR);
        assert!(!benchd_core::model::valid_component(LEDGER_DIR));
    }

    #[test]
    fn a_regular_file_is_not_a_device_and_is_not_handed_over() {
        // The source always comes from `wait_for_vhci_node`, so this should be
        // unreachable — but what follows a successful check here is `chown` to
        // the agent, and doing that to something that is not a device is how a
        // path traversal ends with an agent owning a file it named.
        let tree = TempTree::new("notadevice");
        let file = tree.path().join("console");
        std::fs::write(&file, b"").unwrap();
        assert!(!is_device_node(&std::fs::symlink_metadata(&file).unwrap()));
    }

    #[tokio::test]
    async fn the_lease_path_resolves_to_the_imported_node_itself() {
        // The property the whole scheme exists for. A tool that identifies a
        // device by looking around the system — pyserial's port list, and so
        // esptool's choice of reset sequence — only finds it if what the agent
        // was handed leads back to the node udev knows about. A copy of the
        // device address would open the same driver and still be invisible to
        // every one of them.
        let tree = TempTree::new("linked");
        let root = tree.path().join("run");
        let source = tree.path().join("ttyACM0");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&source, b"").unwrap();

        let dest = root
            .join("pi-4f2a")
            .join("c0-l7")
            .join("dut")
            .join("console");
        link_node(&root, &source, &dest).await.unwrap();

        assert!(
            std::fs::symlink_metadata(&dest).unwrap().is_symlink(),
            "the lease path must be a link, not a node of its own"
        );
        assert_eq!(std::fs::read_link(&dest).unwrap(), source);

        // Materialising twice must not trip over what the first one left, which
        // happens for real whenever a lease outlives the daemon that made it.
        link_node(&root, &source, &dest).await.unwrap();
        assert_eq!(std::fs::read_link(&dest).unwrap(), source);
    }

    #[tokio::test]
    async fn the_lease_tree_gets_explicit_modes_rather_than_the_umask() {
        // Nothing in the daemon sets a umask and the unit file does not either,
        // so `create_dir_all` would take whatever the shell that started it
        // had. Run it by hand with `umask 0` and the lease directories become
        // world-writable, which is a symlink planted where root creates the
        // next lease.
        let tree = TempTree::new("dirmodes");
        let root = tree.path();
        let slot = root.join("pi-4f2a").join("c0-l7").join("dut");
        create_tree(root, &slot).await.unwrap();

        for dir in [
            root.join("pi-4f2a"),
            root.join("pi-4f2a").join("c0-l7"),
            slot.clone(),
        ] {
            assert_eq!(mode_of(&dir), DIR_MODE, "{}", dir.display());
        }

        // A directory left too loose by an earlier run is tightened, not
        // inherited: the mode is pinned every time, not only at creation.
        std::fs::set_permissions(&slot, PermissionsExt::from_mode(0o777)).unwrap();
        create_tree(root, &slot).await.unwrap();
        assert_eq!(mode_of(&slot), DIR_MODE);
    }

    #[tokio::test]
    async fn a_planted_symlink_cannot_move_a_device_node_out_of_the_root() {
        // The containment check used to be `dest.starts_with(root)`, which is a
        // comparison of unresolved components: a symlink at any component of
        // the path passes it while root creates the node wherever it points.
        let tree = TempTree::new("symlink");
        let root = tree.path().join("run");
        let elsewhere = tree.path().join("elsewhere");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("pi-4f2a")).unwrap();

        let dest = root
            .join("pi-4f2a")
            .join("c0-l7")
            .join("dut")
            .join("console");
        let err = resolved_dest(&root, &dest).await.expect_err("must refuse");
        assert!(err.contains("pi-4f2a"), "{err}");
        assert!(
            !elsewhere.join("c0-l7").exists(),
            "nothing may be created through the symlink"
        );

        // ...while an ordinary path resolves and is accepted.
        let plain = root
            .join("pi-9a1c")
            .join("c0-l7")
            .join("dut")
            .join("console");
        let resolved = resolved_dest(&root, &plain).await.unwrap();
        assert!(resolved.starts_with(std::fs::canonicalize(&root).unwrap()));
        assert_eq!(resolved.file_name().unwrap(), "console");
    }

    #[tokio::test]
    async fn nothing_is_created_outside_the_root() {
        let tree = TempTree::new("outside");
        let root = tree.path().join("run");
        std::fs::create_dir_all(&root).unwrap();
        for hostile in ["/etc/cron.d/x", "../elsewhere/x"] {
            let dest = PathBuf::from(hostile);
            assert!(
                resolved_dest(&root, &dest).await.is_err(),
                "{hostile} was accepted"
            );
        }
    }

    #[test]
    fn one_device_behind_two_resources_is_only_taken_away_once() {
        // The USB-SD-Mux arrives as one device and yields two nodes, and those
        // two are locked separately. What must not happen twice is one *node*
        // reached twice: the second record would capture the locked-down
        // ownership as the state to put back, and release would leave a board
        // nobody but root can open.
        let sg = BorrowedNode {
            path: PathBuf::from("/dev/sg0"),
            uid: 0,
            gid: 6,
            mode: 0o660,
        };
        assert!(needs_locking(&[], Path::new("/dev/sg0")));
        assert!(
            needs_locking(std::slice::from_ref(&sg), Path::new("/dev/sda")),
            "the block node is a different node and needs locking too"
        );
        assert!(!needs_locking(
            std::slice::from_ref(&sg),
            Path::new("/dev/sg0")
        ));
    }

    #[tokio::test]
    async fn a_device_that_vanished_while_leased_is_not_a_failed_release() {
        // Unplug a board mid-lease and its node is gone; so is the node of an
        // imported device once its vhci port is detached. Release must not
        // report that as an error, or a reaper would log a failure on every
        // ordinary teardown.
        let tree = TempTree::new("vanished");
        let gone = BorrowedNode {
            path: tree.path().join("ttyACM0"),
            uid: 0,
            gid: 986,
            mode: 0o660,
        };
        restore_node(&gone).await.expect("a missing node is fine");
    }

    #[tokio::test]
    async fn a_released_node_gets_its_mode_back() {
        // The ownership half needs root, so it is the mode that is checked
        // here: a lease that put a node back as 0600 would leave a board that
        // only root can open, which is a bench lost until someone notices.
        let tree = TempTree::new("restore");
        let node = tree.path().join("ttyACM0");
        std::fs::write(&node, b"").unwrap();
        std::fs::set_permissions(&node, PermissionsExt::from_mode(0o660)).unwrap();

        let facts = node_facts(&node).await;
        assert!(facts.is_none(), "a plain file is not a device node");

        let meta = std::fs::symlink_metadata(&node).unwrap();
        use std::os::unix::fs::MetadataExt;
        let borrowed = BorrowedNode {
            path: node.clone(),
            uid: meta.uid(),
            gid: meta.gid(),
            mode: 0o660,
        };
        set_mode(&node, 0o600).await.unwrap();
        assert_eq!(mode_of(&node), 0o600);
        restore_node(&borrowed).await.unwrap();
        assert_eq!(mode_of(&node), 0o660);
    }

    #[tokio::test]
    async fn the_sweep_only_knows_about_ports_this_daemon_attached() {
        // The defect: `clear_stale` detached every port in VDEV_ST_USED,
        // including a board an engineer had attached by hand for debugging and
        // anything a second daemon or a co-located host had imported. Restarting
        // the client daemon — which `deploy.sh` does — took them all away.
        let tree = TempTree::new("ledger");
        let ledger = Ledger::new(tree.path());
        ledger.prepare().await.unwrap();
        assert_eq!(mode_of(&tree.path().join(LEDGER_DIR)), 0o700);

        ledger.record_port(lease(7), 3).await;
        ledger.record_port(lease(7), 4).await;
        ledger
            .record_node(
                lease(7),
                &BorrowedNode {
                    path: PathBuf::from("/dev/ttyACM0"),
                    uid: 0,
                    gid: 986,
                    mode: 0o660,
                },
            )
            .await;
        ledger.record_port(lease(9), 5).await;

        // Sorted because `read_dir` yields the leases in no particular order.
        let mut found = ledger.recover().await;
        found.ports.sort_unstable();
        assert_eq!(found.ports, vec![3, 4, 5]);
        assert_eq!(found.nodes.len(), 1);
        assert_eq!(found.nodes[0].path, PathBuf::from("/dev/ttyACM0"));
        assert_eq!(found.nodes[0].mode, 0o660);
        // Port 6 is somebody else's and must survive the sweep.
        assert!(!found.ports.contains(&6));

        // A released lease stops being the sweep's business immediately.
        ledger.forget(lease(7)).await;
        assert_eq!(ledger.recover().await.ports, vec![5]);
        ledger.forget(lease(7)).await;

        ledger.clear().await;
        assert_eq!(ledger.recover().await, Abandoned::default());
    }

    #[test]
    fn a_half_written_record_does_not_lose_the_rest() {
        // A crash mid-append is exactly the case the ledger exists for, so the
        // last line is routinely truncated garbage.
        let mut found = Abandoned::default();
        parse_records(
            "port 3\nnode 0 986 660 /dev/ttyACM0\nnonsense\nnode 0 986\npo",
            &mut found,
        );
        assert_eq!(found.ports, vec![3]);
        assert_eq!(found.nodes.len(), 1);
        assert_eq!(found.nodes[0].gid, 986);
    }

    #[tokio::test]
    async fn a_node_in_its_own_directory_is_not_mistaken_for_a_mount() {
        // Teardown only shells out to `umount` for a leftover bind mount from
        // an older build. Doing it for every node would be two processes per
        // resource on every release, and would hide a real failure to unlink.
        let tree = TempTree::new("mountcheck");
        let slot = tree.path().join("dut");
        std::fs::create_dir_all(&slot).unwrap();
        let node = slot.join("console");
        std::fs::write(&node, b"").unwrap();
        assert!(!mounted_over(&slot, &node).await);
        assert!(!mounted_over(&slot, &slot.join("missing")).await);
    }
}
