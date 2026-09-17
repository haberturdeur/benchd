//! Lease lifecycle: claim, renew, release, revoke, expire.
//!
//! This is a **pure state machine**. Time is a parameter, never read from a
//! clock; nothing here touches a socket or a device. Decisions come out as
//! [`Effect`]s for the coordinator's I/O layer to execute, which keeps the
//! interesting logic testable without daemons, hardware, or waiting.
//!
//! Identifiers are dense integers assigned here, not UUIDs. The public session
//! token an agent carries is a UUID minted at the edge and mapped to a
//! [`SessionId`] before it reaches this module, so the state machine stays
//! deterministic and its tests stay readable.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::limits::{GrantedTtl, LimitError, Limits, Secs};
use crate::matcher::{allocate, BusyInfo, NoMatch};
use crate::model::{ClaimRequest, Inventory};

macro_rules! id_type {
    ($name:ident, $prefix:literal) => {
        // Transparent on the wire: a bare integer, so protocol lines stay
        // readable (D5) rather than nesting `{"0": 4}`.
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}{}", $prefix, self.0)
            }
        }
    };
}

id_type!(SessionId, "s");
id_type!(LeaseId, "l");

/// Monotonically increasing per bench. An executor records the highest it has
/// seen and rejects anything lower, so a delayed instruction for a dead lease
/// cannot hand out live hardware (D7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Epoch(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevokeReason {
    /// The TTL ran out.
    Expired,
    /// An operator took the bench back.
    Forced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    Released,
    Expired,
    Forced,
    SessionLost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseState {
    Held,
    /// Teardown is coming at `teardown_at`. The holder should park the board.
    Revoking {
        reason: RevokeReason,
        teardown_at: Secs,
    },
}

#[derive(Clone, Debug)]
pub struct Lease {
    pub id: LeaseId,
    pub session: SessionId,
    /// slot name -> bench id
    pub slots: BTreeMap<String, String>,
    /// bench id -> epoch granted for this lease
    pub epochs: BTreeMap<String, Epoch>,
    pub created: Secs,
    pub expires_at: Secs,
    pub state: LeaseState,
    pub reason: String,
}

impl Lease {
    pub fn benches(&self) -> impl Iterator<Item = &String> {
        self.slots.values()
    }

    pub fn held_for(&self, now: Secs) -> Secs {
        now.saturating_sub(self.created)
    }

    pub fn remaining(&self, now: Secs) -> Secs {
        self.expires_at.saturating_sub(now)
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub id: SessionId,
    /// Diagnostic label only. Nothing branches on it (D19).
    pub name: String,
    pub leases: BTreeSet<LeaseId>,
}

/// Something the coordinator's I/O layer must do.
///
/// Order within a returned `Vec` is significant: export before materialise,
/// unmaterialise before unexport. A client must never hold a node the host
/// believes is free.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Export {
        bench: String,
        lease: LeaseId,
        epoch: Epoch,
        session: SessionId,
    },
    Materialize {
        lease: LeaseId,
        session: SessionId,
        slots: BTreeMap<String, String>,
    },
    /// Carries the epoch it was generated at.
    ///
    /// Teardown effects are produced *after* the lease is removed from the
    /// manager, so anything that looks the epoch up later gets 0 — which the
    /// client then fences out as stale, and the mount survives the lease. The
    /// epoch has to travel with the effect.
    Unmaterialize {
        lease: LeaseId,
        session: SessionId,
        epoch: Epoch,
    },
    Unexport {
        bench: String,
        lease: LeaseId,
        epoch: Epoch,
    },
    Notify {
        session: SessionId,
        event: LeaseEvent,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseEvent {
    Granted {
        lease: LeaseId,
        expires_at: Secs,
    },
    /// The lease could not be set up and has been withdrawn. Distinct from
    /// `Ended`: the holder never got working hardware, so it should be told why
    /// rather than left to discover a device that never appears.
    Failed {
        lease: LeaseId,
        detail: String,
    },
    Revoking {
        lease: LeaseId,
        reason: RevokeReason,
        teardown_at: Secs,
    },
    Ended {
        lease: LeaseId,
        reason: EndReason,
    },
}

#[derive(Debug, Error)]
pub enum ClaimError {
    #[error("unknown session")]
    UnknownSession,
    #[error(transparent)]
    Limit(#[from] LimitError),
    #[error("{0}")]
    NoMatch(#[from] NoMatch),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LeaseError {
    #[error("unknown session")]
    UnknownSession,
    #[error("no such lease")]
    UnknownLease,
    #[error("that lease belongs to another session")]
    NotYours,
    #[error("lease is being revoked by an operator and cannot be renewed")]
    ForciblyRevoked,
    #[error(transparent)]
    Limit(#[from] LimitError),
}

/// A successful claim.
#[derive(Clone, Debug)]
pub struct Granted {
    pub lease: LeaseId,
    /// slot -> bench
    pub assignment: BTreeMap<String, String>,
    pub expires_at: Secs,
    pub ttl: GrantedTtl,
    pub effects: Vec<Effect>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Renewed {
    pub expires_at: Secs,
    pub ttl: GrantedTtl,
    pub effects: Vec<Effect>,
}

pub struct LeaseManager {
    inventory: Inventory,
    limits: Limits,
    sessions: BTreeMap<SessionId, Session>,
    leases: BTreeMap<LeaseId, Lease>,
    bench_epoch: BTreeMap<String, u64>,
    next_session: u64,
    next_lease: u64,
}

impl LeaseManager {
    pub fn new(inventory: Inventory, limits: Limits) -> Self {
        LeaseManager {
            inventory,
            limits,
            sessions: BTreeMap::new(),
            leases: BTreeMap::new(),
            bench_epoch: BTreeMap::new(),
            next_session: 1,
            next_lease: 1,
        }
    }

    pub fn inventory(&self) -> &Inventory {
        &self.inventory
    }

    /// Benches arrive by host registration and leave when a host disconnects
    /// (D9), so the inventory is mutable at runtime rather than loaded once.
    pub fn inventory_mut(&mut self) -> &mut Inventory {
        &mut self.inventory
    }

    /// Terminate a lease without asking its holder: the hardware is gone, so
    /// there is nothing to grace. Used when a host disconnects or reports a
    /// lost device.
    pub fn drop_lease(&mut self, lease: LeaseId) -> Vec<Effect> {
        let Some(l) = self.leases.remove(&lease) else {
            return Vec::new();
        };
        if let Some(s) = self.sessions.get_mut(&l.session) {
            s.leases.remove(&lease);
        }
        let mut effects = teardown_effects(&l);
        effects.push(Effect::Notify {
            session: l.session,
            event: LeaseEvent::Ended {
                lease,
                reason: EndReason::Forced,
            },
        });
        effects
    }

    pub fn lease(&self, id: LeaseId) -> Option<&Lease> {
        self.leases.get(&id)
    }

    pub fn leases(&self) -> impl Iterator<Item = &Lease> {
        self.leases.values()
    }

    /// Which benches are unavailable, and why. Feeds the matcher's near-miss
    /// reporting, so a contended claim can say who holds what and for how long.
    pub fn busy(&self, now: Secs) -> BTreeMap<String, BusyInfo> {
        let mut busy = BTreeMap::new();
        for lease in self.leases.values() {
            let owner = self
                .sessions
                .get(&lease.session)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| lease.session.to_string());
            // A lease being revoked frees its bench at `teardown_at`, which can
            // be far sooner than its nominal expiry — an operator taking a
            // 1-hour lease back frees it in 30s. Quoting `expires_at` told the
            // next claimant to wait a hundred times too long.
            let free_in = match lease.state {
                LeaseState::Revoking { teardown_at, .. } => teardown_at.saturating_sub(now),
                LeaseState::Held => lease.remaining(now),
            };
            for bench in lease.benches() {
                busy.insert(
                    bench.clone(),
                    BusyInfo {
                        owner: owner.clone(),
                        expires_in: Some(free_in as f64),
                        reason: lease.reason.clone(),
                    },
                );
            }
        }
        busy
    }

    // -- sessions --------------------------------------------------------

    /// Registration is a request for a token; it always succeeds (D19).
    pub fn register(&mut self, name: impl Into<String>) -> SessionId {
        let id = SessionId(self.next_session);
        self.next_session += 1;
        self.sessions.insert(
            id,
            Session {
                id,
                name: name.into(),
                leases: BTreeSet::new(),
            },
        );
        id
    }

    pub fn session(&self, id: SessionId) -> Option<&Session> {
        self.sessions.get(&id)
    }

    /// The agent is gone. Release its leases **immediately** rather than
    /// waiting out their TTLs: there is nobody left to warn, so a grace period
    /// would only keep hardware idle.
    pub fn end_session(&mut self, id: SessionId, _now: Secs) -> Vec<Effect> {
        let Some(session) = self.sessions.remove(&id) else {
            return Vec::new();
        };
        let mut effects = Vec::new();
        for lease_id in session.leases {
            if let Some(lease) = self.leases.remove(&lease_id) {
                effects.extend(teardown_effects(&lease));
            }
        }
        effects
    }

    // -- lease lifecycle -------------------------------------------------

    pub fn claim(
        &mut self,
        session: SessionId,
        request: &ClaimRequest,
        now: Secs,
    ) -> Result<Granted, ClaimError> {
        // Benches, not slots: one board held through two slots of one lease is
        // one board, and `max_benches` counts boards.
        let held = self
            .sessions
            .get(&session)
            .ok_or(ClaimError::UnknownSession)?
            .leases
            .iter()
            .filter_map(|id| self.leases.get(id))
            .flat_map(|l| l.benches())
            .collect::<BTreeSet<_>>()
            .len();

        // The fewest benches this claim could take: slots that must differ from
        // each other need one each, the rest can share. Nothing sharper is
        // knowable before the matcher has assigned anything, so the assignment
        // is checked again below.
        let least_benches = request
            .slots
            .keys()
            .filter(|slot| request.distinct.applies_to(slot))
            .count()
            .max(1);
        let ttl = self
            .limits
            .grant(request.ttl_seconds, least_benches, held)?;

        let benches = self.inventory.enabled_benches();
        let allocation = allocate(
            request,
            &benches,
            &self.busy(now),
            self.inventory.vocabulary.key_weights(),
        )?;
        let unique: BTreeSet<&String> = allocation.assignment.values().collect();
        self.limits.check_benches(held, unique.len())?;

        let id = LeaseId(self.next_lease);
        self.next_lease += 1;

        // A fresh epoch per bench, so any instruction still in flight for a
        // previous lease on this bench is now detectably stale (D7).
        // De-duplicated: with `distinct: false` two slots can share a bench, and
        // iterating the assignment directly bumped that bench's epoch twice and
        // emitted two Exports. The host short-circuits the second as already
        // active, so it serves the first channel key while the client is handed
        // the second — and the relay can never pair them.
        let mut epochs = BTreeMap::new();
        let mut effects = Vec::new();
        for bench in unique {
            let counter = self.bench_epoch.entry(bench.clone()).or_insert(0);
            *counter += 1;
            let epoch = Epoch(*counter);
            epochs.insert(bench.clone(), epoch);
            effects.push(Effect::Export {
                bench: bench.clone(),
                lease: id,
                epoch,
                session,
            });
        }
        // Export on the host strictly before materialising on the client.
        effects.push(Effect::Materialize {
            lease: id,
            session,
            slots: allocation.assignment.clone(),
        });

        let expires_at = now + ttl.granted;
        effects.push(Effect::Notify {
            session,
            event: LeaseEvent::Granted {
                lease: id,
                expires_at,
            },
        });

        self.leases.insert(
            id,
            Lease {
                id,
                session,
                slots: allocation.assignment.clone(),
                epochs,
                created: now,
                expires_at,
                state: LeaseState::Held,
                reason: request.reason.clone(),
            },
        );
        self.sessions
            .get_mut(&session)
            .expect("checked above")
            .leases
            .insert(id);

        Ok(Granted {
            lease: id,
            assignment: allocation.assignment,
            expires_at,
            ttl,
            effects,
        })
    }

    /// Extend a lease. A heartbeat keepalive would recreate the never-expiring
    /// hold this system exists to remove. Traffic on the leased device is a
    /// different proof — the holder is still working — and the client may send
    /// this same call when it sees that (D15).
    ///
    /// Renewing a lease already `Revoking` because it **expired** cancels the
    /// revocation, which lets an agent that notices the warning rescue its
    /// work. Renewing one revoked by an **operator** is refused.
    pub fn renew(
        &mut self,
        session: SessionId,
        lease: LeaseId,
        extra: Secs,
        now: Secs,
    ) -> Result<Renewed, LeaseError> {
        let limits = self.limits;
        let l = self.owned_mut(session, lease)?;
        if let LeaseState::Revoking {
            reason: RevokeReason::Forced,
            ..
        } = l.state
        {
            return Err(LeaseError::ForciblyRevoked);
        }

        let ttl = limits.renew(extra, l.held_for(now))?;
        l.expires_at = now + ttl.granted;
        l.state = LeaseState::Held;
        let expires_at = l.expires_at;

        Ok(Renewed {
            expires_at,
            ttl,
            effects: Vec::new(),
        })
    }

    /// The holder is done. No grace period: it asked.
    pub fn release(
        &mut self,
        session: SessionId,
        lease: LeaseId,
        _now: Secs,
    ) -> Result<Vec<Effect>, LeaseError> {
        self.owned_mut(session, lease)?;
        let l = self.leases.remove(&lease).expect("checked above");
        if let Some(s) = self.sessions.get_mut(&session) {
            s.leases.remove(&lease);
        }
        let mut effects = teardown_effects(&l);
        effects.push(Effect::Notify {
            session,
            event: LeaseEvent::Ended {
                lease,
                reason: EndReason::Released,
            },
        });
        Ok(effects)
    }

    /// Operator escape hatch (D16): take a bench back. Graced, because the
    /// holder had no warning and may be mid-flash.
    pub fn force_release(&mut self, lease: LeaseId, now: Secs) -> Vec<Effect> {
        let grace = self.limits.grace;
        let Some(l) = self.leases.get_mut(&lease) else {
            return Vec::new();
        };
        // Never later than the lease would have ended anyway: taking a bench
        // back must not hand the holder extra time.
        let teardown_at = (now + grace).min(l.expires_at.max(now));
        l.state = LeaseState::Revoking {
            reason: RevokeReason::Forced,
            teardown_at,
        };
        vec![Effect::Notify {
            session: l.session,
            event: LeaseEvent::Revoking {
                lease,
                reason: RevokeReason::Forced,
                teardown_at,
            },
        }]
    }

    /// Advance time. Moves expiring leases into `Revoking` and tears down ones
    /// whose grace has elapsed.
    ///
    /// The warning lands *inside* the requested TTL: a lease due at `T` goes
    /// `Revoking` at `T - grace` and is gone at `T`. An agent that asked for
    /// fifteen minutes gets exactly fifteen, and the bench frees when the
    /// matcher said it would.
    pub fn tick(&mut self, now: Secs) -> Vec<Effect> {
        let grace = self.limits.grace;
        let mut effects = Vec::new();
        let mut expired = Vec::new();

        for lease in self.leases.values_mut() {
            // Bring each lease fully up to date with `now`, not forward by one
            // state. A tick can arrive arbitrarily late - a busy coordinator, a
            // suspended laptop, a clock jump - and a lease whose teardown is
            // overdue must end on this tick rather than surviving until the
            // next one.
            if let LeaseState::Held = lease.state {
                // The warning lands inside the requested TTL, so an agent that
                // asked for 600s gets 600s. Clamped to half the lease's own
                // length, or a lease shorter than the grace window would spend
                // its whole life in Revoking.
                let span = lease.expires_at.saturating_sub(lease.created);
                let warn_at = lease.expires_at.saturating_sub(grace.min(span / 2));
                if now >= warn_at {
                    lease.state = LeaseState::Revoking {
                        reason: RevokeReason::Expired,
                        teardown_at: lease.expires_at,
                    };
                    effects.push(Effect::Notify {
                        session: lease.session,
                        event: LeaseEvent::Revoking {
                            lease: lease.id,
                            reason: RevokeReason::Expired,
                            teardown_at: lease.expires_at,
                        },
                    });
                }
            }

            if let LeaseState::Revoking { teardown_at, .. } = lease.state {
                if now >= teardown_at {
                    expired.push(lease.id);
                }
            }
        }

        for id in expired {
            let lease = self.leases.remove(&id).expect("just listed");
            let reason = match lease.state {
                LeaseState::Revoking {
                    reason: RevokeReason::Forced,
                    ..
                } => EndReason::Forced,
                _ => EndReason::Expired,
            };
            if let Some(s) = self.sessions.get_mut(&lease.session) {
                s.leases.remove(&id);
            }
            effects.extend(teardown_effects(&lease));
            effects.push(Effect::Notify {
                session: lease.session,
                event: LeaseEvent::Ended { lease: id, reason },
            });
        }

        effects
    }

    fn owned_mut(&mut self, session: SessionId, lease: LeaseId) -> Result<&mut Lease, LeaseError> {
        if !self.sessions.contains_key(&session) {
            return Err(LeaseError::UnknownSession);
        }
        let l = self
            .leases
            .get_mut(&lease)
            .ok_or(LeaseError::UnknownLease)?;
        if l.session != session {
            return Err(LeaseError::NotYours);
        }
        Ok(l)
    }
}

/// Tear down in the safe order: the client lets go before the host does.
fn teardown_effects(lease: &Lease) -> Vec<Effect> {
    // The highest of this lease's per-bench epochs: monotonic per lease, which
    // is what the client fences on.
    let epoch = lease.epochs.values().copied().max().unwrap_or(Epoch(0));
    let mut effects = vec![Effect::Unmaterialize {
        lease: lease.id,
        session: lease.session,
        epoch,
    }];
    for (bench, epoch) in &lease.epochs {
        effects.push(Effect::Unexport {
            bench: bench.clone(),
            lease: lease.id,
            epoch: *epoch,
        });
    }
    effects
}
