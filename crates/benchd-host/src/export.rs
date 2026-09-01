//! Making a bench's resources reachable, and taking them back.
//!
//! Two cases, and the difference is where the client is:
//!
//! * **Co-located** — nothing to do. The client bind-mounts the real inode, so
//!   `export` is bookkeeping only.
//! * **Remote** — the device is bound to `usbip-host` and its socket handed to
//!   the kernel. We never run `usbipd`: it binds wildcard with no authentication
//!   (verified in its source), so we dial out and hand the kernel the resulting
//!   fd instead. Nothing here ever listens.
//!
//! Everything is **epoch-fenced and idempotent**. A delayed instruction for a
//! superseded lease must be dropped, not obeyed: obeying it would hand live
//! hardware to an agent whose lease is gone (D7).

use std::collections::BTreeMap;

use benchd_core::lease::{Epoch, LeaseId, SessionId};
use benchd_core::wire::{BenchSpec, ChannelKey, Outcome};

/// What this host currently has exported.
pub struct Exports {
    spec: BenchSpec,
    /// Highest epoch seen for this bench. Anything lower is stale.
    seen: Epoch,
    active: BTreeMap<LeaseId, Active>,
}

struct Active {
    /// The epoch this was exported at. Kept for diagnostics: when fencing goes
    /// wrong, the first question is always which epoch a device was handed out
    /// under versus which one the coordinator thinks is current.
    epoch: Epoch,
    session: SessionId,
    channel: Option<ChannelKey>,
}

impl Exports {
    pub fn new(spec: BenchSpec) -> Self {
        Exports { spec, seen: Epoch(0), active: BTreeMap::new() }
    }

    /// Undo anything a previous incarnation of this process left behind.
    ///
    /// The kernel takes its own reference to a handed-over socket, so an export
    /// outlives the process that made it. Without this, a restart would leave
    /// hardware reachable by an agent whose lease no longer exists — violating
    /// the whole point of the system by accident (D6).
    pub async fn clear_stale(&mut self) {
        for (name, resource) in &self.spec.resources {
            if let benchd_core::model::Resource::Usb { busid } = resource {
                if is_bound(busid).await {
                    tracing::warn!(%name, %busid, "clearing a stale export from a previous run");
                    unbind(busid).await;
                }
            }
        }
    }

    /// Accept an instruction only if it is at least as new as everything we
    /// have seen for this bench.
    fn fence(&mut self, epoch: Epoch) -> Result<(), Outcome> {
        if epoch < self.seen {
            tracing::warn!(?epoch, seen = ?self.seen, "dropping a stale instruction");
            return Err(Outcome::Stale { seen: self.seen });
        }
        self.seen = epoch;
        Ok(())
    }

    pub async fn export(
        &mut self,
        lease: LeaseId,
        epoch: Epoch,
        session: SessionId,
        channel: Option<ChannelKey>,
    ) -> Outcome {
        if let Err(stale) = self.fence(epoch) {
            return stale;
        }
        if self.active.contains_key(&lease) {
            return Outcome::Ok; // idempotent: a retry is not an error
        }

        match &channel {
            None => {
                tracing::info!(bench = %self.spec.id, %lease, ?epoch, "exported (co-located)");
            }
            Some(key) => {
                // Remote: bind every USB resource so the kernel will accept a
                // socket for it. The relay connection itself is opened lazily
                // when the client dials in with this key.
                for (name, resource) in &self.spec.resources {
                    if let benchd_core::model::Resource::Usb { busid } = resource {
                        if !bind(busid).await {
                            return Outcome::Failed {
                                detail: format!("failed to bind {name} ({busid})"),
                            };
                        }
                    }
                }
                tracing::info!(bench = %self.spec.id, %lease, ?epoch, channel = %key.0, "exported (relayed)");
            }
        }

        self.active.insert(lease, Active { epoch, session, channel });
        Outcome::Ok
    }

    pub async fn unexport(&mut self, lease: LeaseId, epoch: Epoch) -> Outcome {
        if let Err(stale) = self.fence(epoch) {
            return stale;
        }
        // Unexporting something we do not have is a no-op, not an error: the
        // reaper races voluntary releases and neither path may fail.
        let Some(active) = self.active.remove(&lease) else {
            return Outcome::Ok;
        };
        if active.channel.is_some() {
            for resource in self.spec.resources.values() {
                if let benchd_core::model::Resource::Usb { busid } = resource {
                    unbind(busid).await;
                }
            }
        }
        tracing::info!(
            bench = %self.spec.id, %lease,
            exported_at = ?active.epoch, session = %active.session,
            "unexported"
        );
        Outcome::Ok
    }

    /// The coordinator is gone, so every lease it granted is void (D6).
    pub async fn release_all(&mut self) {
        let leases: Vec<LeaseId> = self.active.keys().copied().collect();
        for lease in leases {
            let epoch = self.seen;
            self.unexport(lease, epoch).await;
        }
    }
}

// ---------------------------------------------------------------------------
// usbip plumbing
//
// Driven through sysfs rather than the `usbip` CLI: the operations we need are
// two file writes, and shelling out would add a dependency and a parsing step
// for no benefit.
// ---------------------------------------------------------------------------

const USBIP_HOST: &str = "/sys/bus/usb/drivers/usbip-host";

async fn is_bound(busid: &str) -> bool {
    tokio::fs::metadata(format!("{USBIP_HOST}/{busid}")).await.is_ok()
}

async fn bind(busid: &str) -> bool {
    if is_bound(busid).await {
        return true;
    }
    // Detach from whatever driver currently owns the interface, then attach the
    // stub. Order matters: the stub refuses a device another driver holds.
    let _ = tokio::fs::write(format!("/sys/bus/usb/devices/{busid}/driver/unbind"), busid).await;
    match tokio::fs::write(format!("{USBIP_HOST}/bind"), busid).await {
        Ok(()) => true,
        Err(err) => {
            tracing::error!(%busid, ?err, "usbip bind failed");
            false
        }
    }
}

async fn unbind(busid: &str) {
    if let Err(err) = tokio::fs::write(format!("{USBIP_HOST}/unbind"), busid).await {
        tracing::debug!(%busid, ?err, "usbip unbind failed (probably already gone)");
    }
}
