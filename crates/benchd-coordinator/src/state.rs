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
    BenchSpec, ChannelKey, RequestId, ResourceHandle, SessionToken, ToClient, ToHost, WantedNode,
};
use benchd_core::Limits;

use crate::conn::Outbox;

/// A connected host, and the bench it owns.
pub struct HostConn {
    pub bench: String,
    /// Which connection owns this entry.
    ///
    /// Hosts are keyed by bench, so without this a lingering old connection's
    /// heartbeats are credited to its replacement (hiding a wedged host from
    /// the liveness reaper) and its eventual disconnect removes the replacement
    /// instead of itself.
    pub conn_id: u64,
    pub out: Outbox,
    /// When we last heard anything from this host. A TCP connection can stay
    /// open long after the peer stops functioning — a wedged process, a
    /// half-open connection after a machine sleeps — so silence, not a socket
    /// close, is what marks a bench unavailable (D19).
    pub last_seen: u64,
}

/// A connected client daemon (one per agent machine).
pub struct ClientConn {
    pub out: Outbox,
    pub last_seen: u64,
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
    /// Which lease each outstanding instruction belongs to, so an executor
    /// reporting failure can be traced back to the lease that must be undone.
    /// Without this a failed export produces a lease the agent believes is
    /// good, and a failed materialisation parks a bench for its whole TTL.
    pub pending: BTreeMap<RequestId, LeaseId>,
    /// Rendezvous keys in flight, so the host's `Export` and the client's
    /// `Materialize` name the same channel. Generated once here because the
    /// keys are random (unguessable) rather than derived, and dropped when the
    /// lease ends.
    channels: BTreeMap<(LeaseId, String, String), ChannelKey>,
    next_request: u64,
    next_conn: u64,
}

impl State {
    /// The vocabulary is central and known at startup; benches are not. They
    /// arrive by host registration (D9), so an unreachable host simply has no
    /// allocatable bench rather than a bench that fails at claim time.
    pub fn new(limits: Limits, vocabulary: benchd_core::tags::Vocabulary) -> Self {
        let inventory = Inventory {
            benches: Default::default(),
            vocabulary,
        };
        State {
            leases: LeaseManager::new(inventory, limits),
            hosts: BTreeMap::new(),
            clients: BTreeMap::new(),
            tokens: BTreeMap::new(),
            session_conn: BTreeMap::new(),
            pending: BTreeMap::new(),
            channels: BTreeMap::new(),
            next_request: 1,
            next_conn: 1,
        }
    }

    /// Note that a peer is alive.
    pub fn touch_host(&mut self, bench: &str, conn_id: u64, now: u64) {
        if let Some(host) = self.hosts.get_mut(bench) {
            if host.conn_id == conn_id {
                host.last_seen = now;
            }
        }
    }

    /// Remove a host entry only if this connection still owns it.
    pub fn remove_host(&mut self, bench: &str, conn_id: u64) -> bool {
        match self.hosts.get(bench) {
            Some(host) if host.conn_id == conn_id => {
                self.hosts.remove(bench);
                true
            }
            _ => false,
        }
    }

    pub fn touch_client(&mut self, conn_id: u64, now: u64) {
        if let Some(client) = self.clients.get_mut(&conn_id) {
            client.last_seen = now;
        }
    }

    /// Benches whose host has stopped answering.
    ///
    /// Returned rather than acted on, so the caller can drop the benches and
    /// their leases with the usual effect ordering.
    pub fn silent_hosts(&self, now: u64, timeout: u64) -> Vec<String> {
        self.hosts
            .values()
            .filter(|h| now.saturating_sub(h.last_seen) > timeout)
            .map(|h| h.bench.clone())
            .collect()
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
        // A bench already registered is *replaced*, not refused.
        //
        // A host that has just connected and registered is demonstrably alive;
        // the one holding the old entry may not be. Refusing meant a host whose
        // previous connection was half-open — a crash, a yanked cable, a
        // sleeping machine — was locked out until the liveness timeout reaped
        // it, retrying and failing the whole time. The old registration is
        // dropped along with its leases, since whoever holds them can no longer
        // be sure the hardware is theirs.
        //
        // Two hosts genuinely configured for one bench will flap, which is
        // loud, visible in the log, and a configuration error worth seeing.
        if let Some(old) = self.hosts.get(&spec.id) {
            tracing::warn!(
                bench = %spec.id, old = old.conn_id,
                "re-registering a bench that was already claimed by another connection"
            );
        }
        for (name, resource) in &spec.resources {
            if let benchd_core::model::Resource::Serial { path, .. } = resource {
                benchd_core::model::valid_device_path(path)
                    .map_err(|e| format!("resource {name:?}: {e}"))?;
            }
        }
        for name in spec.resources.keys() {
            // Resource names are also path components on the client daemon.
            if !benchd_core::model::valid_component(name) {
                return Err(format!(
                    "invalid resource name {name:?}: must be a plain path component"
                ));
            }
        }
        if !benchd_core::model::valid_component(&spec.id) {
            return Err(format!("invalid bench id {:?}", spec.id));
        }
        // Re-checked here even though the host checks it too: the host may be an
        // older build, and this text lands in agent context windows.
        if spec.docs.len() > benchd_core::model::MAX_BENCH_DOCS {
            return Err(format!(
                "docs are {} bytes, over the {} byte limit",
                spec.docs.len(),
                benchd_core::model::MAX_BENCH_DOCS
            ));
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
            docs: spec.docs.clone(),
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
                Effect::Export {
                    bench,
                    lease,
                    epoch,
                    session,
                } => {
                    let Some(host_out) = self.hosts.get(&bench).map(|h| h.out.clone()) else {
                        tracing::warn!(%bench, "export for a bench whose host has gone");
                        continue;
                    };
                    // Mint one key per *device*, not per resource, and hand the
                    // same key to every resource that names it. USB/IP forwards
                    // whole devices, so a USB-SD-Mux claimed as both its SCSI
                    // and its block node is one import; two keys would leave one
                    // of them with nobody to pair with in the relay. The client
                    // is handed these same keys when its Materialize is built
                    // below, and both ends deduplicate by key.
                    let mut channels = BTreeMap::new();
                    let resources: Vec<(String, String)> = self
                        .leases
                        .inventory()
                        .benches
                        .get(&bench)
                        .map(|b| {
                            b.resources
                                .iter()
                                .map(|(name, r)| (name.clone(), r.device_key(name).to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut per_device: BTreeMap<String, ChannelKey> = BTreeMap::new();
                    for (name, device) in resources {
                        let key = per_device
                            .entry(device)
                            .or_insert_with(ChannelKey::generate)
                            .clone();
                        self.channels
                            .insert((lease, bench.clone(), name.clone()), key.clone());
                        channels.insert(name, key);
                    }
                    let request = self.next_request();
                    self.pending.insert(request, lease);
                    out.push(Outgoing::Host {
                        out: host_out,
                        msg: ToHost::Export {
                            request,
                            lease,
                            epoch,
                            session,
                            channels,
                        },
                    });
                }
                Effect::Unexport {
                    bench,
                    lease,
                    epoch,
                } => {
                    self.channels
                        .retain(|(l, b, _), _| !(*l == lease && *b == bench));
                    let Some(host_out) = self.hosts.get(&bench).map(|h| h.out.clone()) else {
                        continue;
                    };
                    let request = self.next_request();
                    out.push(Outgoing::Host {
                        out: host_out,
                        msg: ToHost::Unexport {
                            request,
                            lease,
                            epoch,
                        },
                    });
                }
                Effect::Materialize {
                    lease,
                    session,
                    slots,
                } => {
                    let Some(conn) = self.client_for(session) else {
                        continue;
                    };
                    let epoch = self.lease_epoch(lease);
                    let handles = self.handles_for(&slots, lease);
                    let request = self.next_request();
                    self.pending.insert(request, lease);
                    out.push(Outgoing::Client {
                        out: conn,
                        msg: ToClient::Materialize {
                            request,
                            lease,
                            epoch,
                            session,
                            slots: handles,
                        },
                    });
                }
                Effect::Unmaterialize {
                    lease,
                    session,
                    epoch,
                } => {
                    let Some(conn) = self.client_for(session) else {
                        continue;
                    };
                    let request = self.next_request();
                    out.push(Outgoing::Client {
                        out: conn,
                        msg: ToClient::Unmaterialize {
                            request,
                            lease,
                            epoch,
                            session,
                        },
                    });
                }
                Effect::Notify { session, event } => {
                    let Some(conn) = self.client_for(session) else {
                        continue;
                    };
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

    /// One epoch for a whole lease, for the client to fence on.
    ///
    /// The client holds a lease, not a bench, so it needs a single number. The
    /// highest of the lease's per-bench epochs is monotonic per lease, which is
    /// all fencing requires.
    fn lease_epoch(&self, lease: LeaseId) -> Epoch {
        self.leases
            .lease(lease)
            .and_then(|l| l.epochs.values().copied().max())
            .unwrap_or(Epoch(0))
    }

    /// The key minted for this resource when the host was told to export it.
    fn channel_for(&self, lease: LeaseId, bench: &str, resource: &str) -> ChannelKey {
        self.channels
            .get(&(lease, bench.to_string(), resource.to_string()))
            .cloned()
            .unwrap_or_else(|| {
                // Cannot happen: Export is dispatched before Materialize. If it
                // ever does, an unmatchable key fails the rendezvous loudly
                // rather than pairing with something unintended.
                tracing::error!(%lease, %bench, %resource, "no channel key for a relayed resource");
                ChannelKey::generate()
            })
    }

    /// The epoch this lease was granted for one specific bench.
    ///
    /// Per bench, never shared: every bench has its own counter, so a two-slot
    /// claim can hold epoch 4 of one bench and epoch 1 of another. An earlier
    /// version took the first epoch it found and stamped it on every bench,
    /// which made the client and the host derive different channel keys for the
    /// same resource — the relay could then never pair them.
    fn epoch_for(&self, lease: LeaseId, bench: &str) -> Option<Epoch> {
        self.leases.lease(lease)?.epochs.get(bench).copied()
    }

    /// How the client should reach each resource of each granted bench.
    ///
    /// Always USB/IP, even when the host and client share a machine. The host
    /// keeps its devices bound to the USB/IP stub for its whole lifetime so
    /// that an unleased board has no tty for anyone to open, and a stub-bound
    /// device cannot also be handed over as a local inode — there is none.
    pub fn handles_for(
        &self,
        slots: &BTreeMap<String, String>,
        lease: LeaseId,
    ) -> BTreeMap<String, BTreeMap<String, ResourceHandle>> {
        let mut out = BTreeMap::new();
        for (slot, bench_id) in slots {
            let Some(bench) = self.leases.inventory().benches.get(bench_id) else {
                continue;
            };
            // Keys are no longer derived from the epoch, but this still checks
            // that the lease really holds this bench before handing out a path
            // to it.
            if self.epoch_for(lease, bench_id).is_none() {
                tracing::error!(%lease, %bench_id, "materialising a bench this lease does not hold");
                continue;
            }
            let mut resources = BTreeMap::new();
            for (name, resource) in &bench.resources {
                // The busid is resolved by the host, which is the machine that
                // can actually see the device. The client only needs to name
                // what it is asking for, and the host answers with whatever
                // busid it exported under.
                let handle = match resource {
                    benchd_core::model::Resource::Serial { interface, .. } => {
                        ResourceHandle::UsbIp {
                            channel: self.channel_for(lease, bench_id, name),
                            busid: String::new(),
                            node: WantedNode::Tty {
                                interface: *interface,
                            },
                        }
                    }
                    benchd_core::model::Resource::Usb { busid, node } => ResourceHandle::UsbIp {
                        channel: self.channel_for(lease, bench_id, name),
                        busid: busid.clone(),
                        node: match node {
                            benchd_core::model::UsbNode::Block => WantedNode::Block,
                            benchd_core::model::UsbNode::Scsi => WantedNode::Scsi,
                        },
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
        LeaseEvent::Failed { lease, detail } => ToClient::Failed { lease, detail },
        LeaseEvent::Revoking {
            lease,
            reason,
            teardown_at,
        } => ToClient::Revoking {
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
