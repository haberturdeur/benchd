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
use benchd_core::wire::{Outcome, ResourceHandle};

pub struct Materializer {
    root: PathBuf,
    active: BTreeMap<LeaseId, PathBuf>,
}

impl Materializer {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Materializer { root: root.into(), active: BTreeMap::new() }
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

    pub async fn materialize(
        &mut self,
        lease: LeaseId,
        owner: &str,
        slots: &BTreeMap<String, BTreeMap<String, ResourceHandle>>,
    ) -> Outcome {
        let dir = self.lease_dir(owner, lease);

        // Resolve everything before mounting anything: a half-materialised
        // lease is worse than a failed one, because the agent would find some
        // of its devices and reasonably assume it had them all.
        let mut plan: Vec<(PathBuf, PathBuf)> = Vec::new();
        for (slot, resources) in slots {
            for (name, handle) in resources {
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
                        plan.push((source, dir.join(slot).join(name)));
                    }
                    ResourceHandle::UsbIp { .. } => {
                        return Outcome::Failed {
                            detail: format!(
                                "{slot}/{name}: remote benches are not implemented yet"
                            ),
                        };
                    }
                }
            }
        }

        for (source, dest) in &plan {
            if let Err(detail) = bind_mount(source, dest).await {
                // Roll back, so a failure never leaves a partial lease behind.
                unmount_tree(&dir).await;
                return Outcome::Failed { detail };
            }
        }

        self.active.insert(lease, dir.clone());
        tracing::info!(%lease, %owner, path = %dir.display(), mounts = plan.len(), "materialized");
        Outcome::Ok
    }

    pub async fn unmaterialize(&mut self, lease: LeaseId, owner: &str) -> Outcome {
        // Idempotent: the reaper races voluntary releases, and neither path may
        // fail. Fall back to the computed path so a restarted daemon can still
        // clean up a lease it does not remember.
        let dir = self.active.remove(&lease).unwrap_or_else(|| self.lease_dir(owner, lease));
        unmount_tree(&dir).await;
        tracing::info!(%lease, %owner, "unmaterialized");
        Outcome::Ok
    }
}

async fn bind_mount(source: &Path, dest: &Path) -> Result<(), String> {
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

/// Sanitise a session name into a directory component.
///
/// Agent-supplied, so it must never be able to escape the root — this is the
/// one place an agent's input reaches a path.
pub fn owner_dir(session: SessionId, name: &str) -> String {
    let safe: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    if safe.is_empty() {
        session.to_string()
    } else {
        format!("{safe}-{session}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owner_directory_cannot_escape_the_root() {
        // The session id is always appended, so even a hostile name stays a
        // single, unique directory component.
        let dir = owner_dir(SessionId(3), "../../etc/passwd");
        assert!(!dir.contains('/'));
        assert!(!dir.contains(".."));
        assert!(dir.ends_with("s3"));
    }

    #[test]
    fn an_empty_name_still_yields_a_directory() {
        assert_eq!(owner_dir(SessionId(7), "!!!"), "s7");
    }
}
