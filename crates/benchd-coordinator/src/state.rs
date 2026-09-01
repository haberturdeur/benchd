//! Coordinator state: the lease machine plus who is currently connected.
//!
//! One `Mutex` guards everything. That is not laziness — it is the invariant
//! from D6 made structural: the coordinator is the only writer of lease state,
//! so claim, renew, expiry and forced release are serialised by construction
//! and the read-then-act races that plague distributed lock managers cannot
//! occur here.
//!
//! The lock is held only across pure state-machine calls. [`Effect`]s are
//! collected under the lock and dispatched after it is released, so a slow peer
//! can never stall the reaper.

use std::collections::BTreeMap;

use benchd_core::lease::{Effect, Epoch, LeaseId, LeaseManager, SessionId};
use benchd_core::model::{Bench, Inventory};
use benchd_core::wire::{
    BenchSpec, RequestId, ResourceHandle, SessionToken, ToClient, ToHost,
};
use benchd_core::Limits;

use crate::conn::Outbox;

/// A connected host, and the bench it owns.
pub struct HostConn {
    pub bench: String,
    pub out: Outbox,
    /// Address the host dialled from. Used to decide whether a client is
    /// co-located with it, which is what selects bind-mount versus USB/IP.
    pub peer_ip: std::net::IpAddr,
}

/// A connected client daemon (one per agent machine).
pub struct ClientConn {
    pub out: Outbox,
    pub peer_ip: std::net::IpAddr,
}

pub struct State {
    pub leases: LeaseManager,
    /// bench id -> the host that owns it
    pub hosts: BTreeMap<String, HostConn>,
    /// connection id -> client daemon
    pub clients: BTreeMap<u64, ClientConn>,
    /// public token -> internal id, so an agent never sees a guessable integer
    pub tokens: BTreeMap<SessionToken, SessionId>,
    /// internal id -> which client connection owns it
    pub session_conn: BTreeMap<SessionId, u64>,
    next_request: u64,
    next_conn: u64,
    /// Treat every bench as remote. Exercises the USB/IP path on one machine.
    pub force_relay: bool,
}

impl State {
    /// The vocabulary is central and known at startup; benches are not. They
    /// arrive by host registration (D9), so an unreachable host simply has no
    /// allocatable bench rather than a bench that fails at claim time.
    pub fn new(limits: Limits, vocabulary: benchd_core::tags::Vocabulary) -> Self {
        let inventory = Inventory { benches: Default::default(), vocabulary };
        State {
            leases: LeaseManager::new(inventory, limits),
            hosts: BTreeMap::new(),
            clients: BTreeMap::new(),
            tokens: BTreeMap::new(),
            session_conn: BTreeMap::new(),
            next_request: 1,
            next_conn: 1,
            force_relay: false,
        }
    }

    pub fn next_request(&mut self) -> RequestId {
        let id = RequestId(self.next_request);
        self.next_request += 1;
        id
    }

    pub fn next_conn(&mut self) -> u64 {
        let id = self.next_conn;
        self.next_conn += 1;
        id
    }

    pub fn mint_token(&mut self, id: SessionId) -> SessionToken {
        let token = SessionToken(uuid::Uuid::new_v4().to_string());
        self.tokens.insert(token.clone(), id);
        token
    }

    pub fn resolve(&self, token: &SessionToken) -> Option<SessionId> {
        self.tokens.get(token).copied()
    }

    /// Register a bench declared by a host. The coordinator owns the
    /// vocabulary, so unknown tags are refused here rather than silently
    /// producing a bench that matches nothing (D9).
    pub fn register_bench(&mut self, spec: &BenchSpec) -> Result<Bench, String> {
        if self.hosts.contains_key(&spec.id) {
            return Err(format!("bench {:?} is already registered", spec.id));
        }
        let vocabulary = &self.leases.inventory().vocabulary;
        let declared: benchd_core::tags::TagSet = spec.tags.iter().cloned().collect();
        vocabulary.check(&declared).map_err(|e| e.to_string())?;

        let mut tags = vocabulary.expand(&declared);
        let name_tag = benchd_core::tags::Tag::parse(&format!("name={}", spec.id))
            .map_err(|e| format!("bench id is not usable as a tag value: {e}"))?;
        tags.insert(name_tag);

        Ok(Bench {
            id: spec.id.clone(),
            tags,
            resources: spec.resources.clone(),
            description: spec.description.clone(),
            enabled: true,
        })
    }

    /// Turn state-machine effects into wire messages.
    ///
    /// Effect order is significant and preserved: export before materialise,
    /// unmaterialise before unexport, so a client never holds a device node the
    /// host believes is free.
    pub fn dispatch(&mut self, effects: Vec<Effect>) -> Vec<Outgoing> {
        let mut out = Vec::new();
        for effect in effects {
            match effect {
                Effect::Export { bench, lease, epoch, session } => {
                    let Some(host_out) = self.hosts.get(&bench).map(|h| h.out.clone()) else {
                        tracing::warn!(%bench, "export for a bench whose host has gone");
                        continue;
                    };
                    let relay = self.needs_relay(&bench, session);
                    let request = self.next_request();
                    out.push(Outgoing::Host {
                        out: host_out,
                        msg: ToHost::Export { request, lease, epoch, session, relay },
                    });
                }
                Effect::Unexport { bench, lease, epoch } => {
                    let Some(host_out) = self.hosts.get(&bench).map(|h| h.out.clone()) else {
                        continue;
                    };
                    let request = self.next_request();
                    out.push(Outgoing::Host {
                        out: host_out,
                        msg: ToHost::Unexport { request, lease, epoch },
                    });
                }
                Effect::Materialize { lease, session, slots } => {
                    let Some(conn) = self.client_for(session) else { continue };
                    let epoch = self.epoch_for(&slots);
                    let handles = self.handles_for(&slots, session, lease, epoch);
                    let request = self.next_request();
                    out.push(Outgoing::Client {
                        out: conn,
                        msg: ToClient::Materialize { request, lease, session, slots: handles },
                    });
                }
                Effect::Unmaterialize { lease, session } => {
                    let Some(conn) = self.client_for(session) else { continue };
                    let request = self.next_request();
                    out.push(Outgoing::Client {
                        out: conn,
                        msg: ToClient::Unmaterialize { request, lease, session },
                    });
                }
                Effect::Notify { session, event } => {
                    let Some(conn) = self.client_for(session) else { continue };
                    // `Granted` is delivered by the claim handler, which knows
                    // the request id to correlate against; there is nothing
                    // unsolicited to push here.
                    if let Some(msg) = notify_to_wire(event) {
                        out.push(Outgoing::Client { out: conn, msg });
                    }
                }
            }
        }
        out
    }

    fn client_for(&self, session: SessionId) -> Option<Outbox> {
        let conn_id = self.session_conn.get(&session)?;
        self.clients.get(conn_id).map(|c| c.out.clone())
    }

    /// True when the host and client are on different machines, so the device
    /// must be forwarded rather than bind-mounted (D5).
    ///
    /// `force_relay` makes every bench take the remote path regardless, which is
    /// how the USB/IP code is exercised on a single machine.
    fn needs_relay(&self, bench: &str, session: SessionId) -> bool {
        if self.force_relay {
            return true;
        }
        let Some(host) = self.hosts.get(bench) else { return false };
        let Some(conn_id) = self.session_conn.get(&session) else { return false };
        let Some(client) = self.clients.get(conn_id) else { return false };
        host.peer_ip != client.peer_ip
    }

    /// The epoch granted for these benches. They are all part of one lease and
    /// therefore share a grant, so any of them answers.
    fn epoch_for(&self, slots: &BTreeMap<String, String>) -> Epoch {
        slots
            .values()
            .find_map(|bench| {
                self.leases
                    .leases()
                    .find_map(|l| l.epochs.get(bench).copied())
            })
            .unwrap_or(Epoch(0))
    }

    pub fn handles_for(
        &self,
        slots: &BTreeMap<String, String>,
        session: SessionId,
        lease: LeaseId,
        epoch: Epoch,
    ) -> BTreeMap<String, BTreeMap<String, ResourceHandle>> {
        let mut out = BTreeMap::new();
        for (slot, bench_id) in slots {
            let Some(bench) = self.leases.inventory().benches.get(bench_id) else {
                continue;
            };
            let relay = self.needs_relay(bench_id, session);
            let mut resources = BTreeMap::new();
            for (name, resource) in &bench.resources {
                let handle = match resource {
                    benchd_core::model::Resource::Serial { by_id } if !relay => {
                        ResourceHandle::Local { path: by_id.display().to_string() }
                    }
                    // Remote: the busid is resolved by the host, which is the
                    // machine that can actually see the device. The client only
                    // needs to name what it is asking for, and the host answers
                    // with whatever busid it exported under.
                    benchd_core::model::Resource::Serial { .. } => ResourceHandle::UsbIp {
                        channel: benchd_core::wire::channel_key(lease, epoch, bench_id, name),
                        busid: String::new(),
                    },
                    benchd_core::model::Resource::Usb { busid } => ResourceHandle::UsbIp {
                        channel: benchd_core::wire::channel_key(lease, epoch, bench_id, name),
                        busid: busid.clone(),
                    },
                };
                resources.insert(name.clone(), handle);
            }
            out.insert(slot.clone(), resources);
        }
        out
    }
}

/// A message ready to send, paired with the peer to send it to.
pub enum Outgoing {
    Host { out: Outbox, msg: ToHost },
    Client { out: Outbox, msg: ToClient },
}

impl Outgoing {
    pub fn send(self) {
        match self {
            Outgoing::Host { out, msg } => out.send(&msg),
            Outgoing::Client { out, msg } => out.send(&msg),
        }
    }
}

/// Unsolicited lease events, as pushed to the client.
///
/// Returns `None` for events the request/response path already covers, so a
/// grant is never delivered twice.
fn notify_to_wire(event: benchd_core::lease::LeaseEvent) -> Option<ToClient> {
    use benchd_core::lease::{EndReason, LeaseEvent, RevokeReason};
    Some(match event {
        LeaseEvent::Granted { .. } => return None,
        LeaseEvent::Revoking { lease, reason, teardown_at } => ToClient::Revoking {
            lease,
            reason: match reason {
                RevokeReason::Expired => "expired".into(),
                RevokeReason::Forced => "forced".into(),
            },
            teardown_at,
        },
        LeaseEvent::Ended { lease, reason } => ToClient::Ended {
            lease,
            reason: match reason {
                EndReason::Released => "released",
                EndReason::Expired => "expired",
                EndReason::Forced => "forced",
                EndReason::SessionLost => "session_lost",
            }
            .into(),
        },
    })
}
