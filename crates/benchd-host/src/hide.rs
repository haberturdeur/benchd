//! Keeping a bench's devices off the machine they are plugged into.
//!
//! A USB serial device is reachable only through the tty its driver creates.
//! Bind it to `usbip-host` instead and no driver claims the serial interface,
//! so there is no `/dev/ttyUSB*` for anyone to open — not another agent, not an
//! unprivileged user, not root. That is a stronger guarantee than permissions
//! or a sandbox can give, because there is no object left to guard.
//!
//! So a host hides every device its bench declares, for its whole lifetime, and
//! reaches it only over USB/IP and only for the duration of a lease. This is
//! what lets the client side stop caring about sandboxes: an agent that ignores
//! the skill and reaches for `/dev/ttyUSB0` finds nothing there, on any machine.
//!
//! **This is the only place in the workspace that changes which driver owns a
//! device.** A lease attaches a socket to a device that is already bound and
//! ending one takes that socket away again, so a device changes driver twice in
//! a host's life and both times in this file. When exporting owned the binding
//! too, the first lease to end gave its board back to `cdc_acm` — tty and all —
//! and nothing existed that would ever hide it again.
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

/// Release every device named in `state`, and forget the ones that came back.
///
/// Must run before the bench is resolved, because resolution goes through the
/// tty and a hidden device has none. Without it a host killed while hiding
/// could never start again: it would wait forever for hardware it had itself
/// made invisible.
///
/// A device that did *not* come back keeps its line in the record. Deleting it
/// would leave a board stub-bound with nothing naming it, and there is no way
/// back from that by hand or otherwise: resolving a serial resource needs the
/// tty the device no longer has.
pub async fn release_recorded(state: &Path) {
    let Ok(text) = tokio::fs::read_to_string(state).await else {
        return;
    };
    let mut still_hidden = Vec::new();
    for busid in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        tracing::warn!(%busid, "releasing a device hidden by a previous run");
        if let Err(detail) = sysfs::unbind(busid).await {
            tracing::error!(%busid, %detail, "this device is still hidden and has no tty");
            still_hidden.push(busid.to_string());
        }
    }
    keep(state, &still_hidden).await;
}

/// Write down what is hidden, or remove the record when nothing is.
async fn record(state: &Path, busids: &[String]) -> std::io::Result<()> {
    if busids.is_empty() {
        return match tokio::fs::remove_file(state).await {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        };
    }
    if let Some(dir) = state.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    tokio::fs::write(state, busids.join("\n")).await
}

/// [`record`], for the paths that can only complain about failing.
async fn keep(state: &Path, busids: &[String]) {
    if let Err(err) = record(state, busids).await {
        tracing::warn!(
            path = %state.display(), ?err,
            "could not update the record of hidden devices"
        );
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

        // A bench with nothing to hide is not a hidden bench. Registration
        // refuses one already; saying so here as well keeps the security
        // boundary from ever reporting success over an empty list.
        if busids.is_empty() {
            return Err("this bench resolved no devices, so there is nothing to hide".into());
        }

        record(&state, &busids)
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
    ///
    /// Reports success only when every device actually came back. The sysfs
    /// writes can time out on a wedged usbip driver, and announcing a release
    /// that did not happen — while deleting the record naming the boards it did
    /// not happen to — is how a bench becomes unrecoverable: a stub-bound
    /// device has no tty, and a serial resource cannot be resolved without one.
    pub async fn release(self) {
        let mut still_hidden = Vec::new();
        for busid in &self.busids {
            if let Err(detail) = sysfs::unbind(busid).await {
                tracing::error!(%busid, %detail, "could not give this device back");
                still_hidden.push(busid.clone());
            }
        }
        keep(&self.state, &still_hidden).await;
        if still_hidden.is_empty() {
            tracing::info!(
                devices = self.busids.len(),
                "bench released; its devices are back on their normal drivers"
            );
        } else {
            tracing::error!(
                devices = still_hidden.len(),
                busids = %still_hidden.join(" "),
                path = %self.state.display(),
                "these devices are still bound to the usbip stub and have no tty; they stay \
                 recorded so the next start can try again"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{record, state_path};

    /// The record is the only thing that names a hidden device, so a release
    /// that did not happen must leave it behind. Deleting it while the boards
    /// were still stub-bound made them unrecoverable: nothing named them, and
    /// resolving a serial resource needs the tty they no longer had.
    #[tokio::test]
    async fn a_device_that_did_not_come_back_keeps_its_line_in_the_record() {
        let dir = std::env::temp_dir().join(format!("benchd-hide-{}", std::process::id()));
        let state = state_path(&dir, "esp32s3-a");

        let hidden = ["1-2".to_string(), "3-1.1".to_string()];
        record(&state, &hidden).await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&state).await.unwrap(),
            "1-2\n3-1.1"
        );

        // One came back, one did not.
        record(&state, &hidden[1..]).await.unwrap();
        assert_eq!(tokio::fs::read_to_string(&state).await.unwrap(), "3-1.1");

        // Everything is back: nothing left to remember, and forgetting twice is
        // not a failure — `--release-all` runs on a bench that may have been
        // released already.
        record(&state, &[]).await.unwrap();
        assert!(!state.exists());
        record(&state, &[]).await.unwrap();

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
