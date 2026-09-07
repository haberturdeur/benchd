//! Keeping a bench's devices off the machine they are plugged into.
//!
//! A USB serial device is reachable only through the tty its driver creates.
//! Bind it to `usbip-host` instead and no driver claims the serial interface,
//! so there is no `/dev/ttyUSB*` for anyone to open — not another agent, not an
//! unprivileged user, not root. That is a stronger guarantee than permissions
//! or a sandbox can give, because there is no object left to guard.
//!
//! So a host hides every device its bench declares, for its whole lifetime, and
//! gives one up only for the duration of a lease and only over USB/IP. This is
//! what lets the client side stop caring about sandboxes: an agent that ignores
//! the skill and reaches for `/dev/ttyUSB0` finds nothing there, on any machine.
//!
//! **The busids are written to disk before they are bound, never after.** A
//! stub binding is kernel state that outlives the process which made it, so a
//! host that is `SIGKILL`ed leaves boards with no tty and nothing running that
//! remembers why. Recording first means the worst case is a note about a device
//! that never got bound, and releasing one of those is harmless — whereas
//! recording last would lose the device that was bound as we died.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use benchd_core::sysfs;

/// Where a bench records the devices it has hidden.
///
/// Under `/run` deliberately. It is a tmpfs, so the record does not survive a
/// reboot — and neither does a stub binding, which is runtime kernel state. The
/// two therefore cannot disagree about a machine that has restarted.
pub fn state_path(dir: &Path, bench: &str) -> PathBuf {
    dir.join(format!("{bench}.busids"))
}

/// Release every device named in `state`, then forget it.
///
/// Must run before the bench is resolved, because resolution goes through the
/// tty and a hidden device has none. Without it a host killed while hiding
/// could never start again: it would wait forever for hardware it had itself
/// made invisible.
pub async fn release_recorded(state: &Path) {
    let Ok(text) = tokio::fs::read_to_string(state).await else {
        return;
    };
    for busid in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        tracing::warn!(%busid, "releasing a device hidden by a previous run");
        sysfs::unbind(busid).await;
    }
    forget(state).await;
}

async fn forget(state: &Path) {
    if let Err(err) = tokio::fs::remove_file(state).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                path = %state.display(), ?err,
                "could not remove the record of hidden devices"
            );
        }
    }
}

/// The devices this process has hidden, and the record of them on disk.
pub struct Hidden {
    state: PathBuf,
    busids: Vec<String>,
}

impl Hidden {
    /// Bind every busid to the USB/IP stub, so this bench has no tty anywhere.
    ///
    /// All or nothing. A partial success is undone before returning, because a
    /// bench with some of its boards still visible is precisely the state this
    /// exists to prevent, and it would be reported as a working bench.
    pub async fn hide(state: PathBuf, busids: &BTreeMap<String, String>) -> Result<Self, String> {
        // Deduplicated: two resources can name the same physical device, and
        // hiding it twice would also try to release it twice.
        let busids: Vec<String> = busids
            .values()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        if let Some(dir) = state.parent() {
            tokio::fs::create_dir_all(dir)
                .await
                .map_err(|e| format!("creating {}: {e}", dir.display()))?;
        }
        tokio::fs::write(&state, busids.join("\n"))
            .await
            .map_err(|e| format!("recording hidden devices in {}: {e}", state.display()))?;

        let mut hidden = Hidden {
            state,
            busids: Vec::new(),
        };
        for busid in busids {
            // Recorded as ours before the attempt, not after: `bind` detaches
            // the normal driver first and can fail after that, leaving the
            // device with no driver at all. It needs releasing just as much as
            // the ones that succeeded.
            hidden.busids.push(busid.clone());
            if let Err(err) = sysfs::bind(&busid).await {
                let detail = format!("hiding {busid}: {err}");
                hidden.release().await;
                return Err(detail);
            }
        }
        tracing::info!(
            devices = hidden.busids.len(),
            "bench hidden; its devices have no tty on this machine"
        );
        Ok(hidden)
    }

    /// Give every device back to its normal driver.
    pub async fn release(self) {
        for busid in &self.busids {
            sysfs::unbind(busid).await;
        }
        forget(&self.state).await;
        tracing::info!(
            devices = self.busids.len(),
            "bench released; its devices are back on their normal drivers"
        );
    }
}
