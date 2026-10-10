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
use std::sync::Arc;

use benchd_core::lease::{Effect, Epoch, LeaseId, LeaseManager, SessionId};
use benchd_core::model::{Bench, Inventory};
use benchd_core::wire::{
    BenchSpec, ChannelKey, RequestId, ResourceHandle, SessionToken, ToClient, ToHost, WantedNode,
};
use benchd_core::Limits;
use tokio::sync::Notify;

use crate::conn::Outbox;

/// A connected host, and the bench it registered.
pub struct HostConn {
    /// The bench as accepted at registration.
    ///
    /// Kept whole rather than by id so a bench withdrawn from one connection
    /// can be handed straight back to another that is still registered for it,
    /// without waiting for that host to say anything.
    pub bench: Bench,
    pub out: Outbox,
    /// Fires when the coordinator has stopped believing this connection serves
    /// its bench and wants it closed.
    ///
    /// There is no "your bench has been withdrawn" message, and a host that
    /// was merely slow has no other way back: its registration is gone, so the
    /// heartbeats it is still sending land on nothing. Losing the connection
    /// is a state it already recovers from, by releasing its exports and
    /// registering again.
    pub hangup: Arc<Notify>,
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

/// An outstanding instruction: which lease it belongs to, and who was told to
/// carry it out.
#[derive(Clone, Copy, Debug)]
pub struct Pending {
    pub lease: LeaseId,
    pub replier: Replier,
}

/// The peer an instruction was sent to.
///
/// Connection ids come from one counter, so the number alone identifies the
/// peer; the kind travels with it because a client answering for a host is a
/// different mistake from a reply that arrived late, and the log should say
/// which. It is also what a failure is attributed to when it is passed on to
/// the holder — that used to be hardcoded per handler, so a host's failed
/// export was reported to the agent as the client's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replier {
    Host(u64),
    Client(u64),
}

impl Replier {
    /// How to name this peer to the lease holder.
    pub fn kind(self) -> &'static str {
        match self {
            Replier::Host(_) => "host",
            Replier::Client(_) => "client",
        }
    }
}

impl std::fmt::Display for Replier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Replier::Host(conn) => write!(f, "host on connection {conn}"),
            Replier::Client(conn) => write!(f, "client on connection {conn}"),
        }
    }
}

/// A teardown its holder has not yet acknowledged.
///
/// "Unmaterialise before unexport" was only ever an ordering of *sends*. The
/// lease leaves the lease table before either instruction goes out, so the
/// bench leaves the busy set at the same moment and the very next claim can
/// have the host export it while the previous holder is still unwinding its
/// import — which the client serialises behind one global mutex held across a
/// USB/IP operation, so the window is tens of seconds rather than microseconds.
///
/// Holding the bench out of the matcher until the holder answers is what makes
/// the invariant real. The deadline is what stops the cure being worse than the
/// disease: a bench waiting for a `Done` that will never come is a worse bug
/// than the stale mount it prevents.
struct Drain {
    exclusive_group: Option<String>,
    groups: Vec<String>,
    benches: Vec<String>,
    deadline: u64,
}

pub struct State {
    pub leases: LeaseManager,
    /// connection id -> the host on it
    ///
    /// Keyed by connection, not by bench. Two connections can be registered
    /// for one bench: [`State::register_bench`] deliberately lets a restarted
    /// host take its bench back while its half-open predecessor is still
    /// there. Keyed by bench, the loser was forgotten — so when the winner
    /// disconnected the bench went with it, and the host that was still
    /// connected, still heartbeating and still holding the hardware never got
    /// it back.
    pub hosts: BTreeMap<u64, HostConn>,
    /// bench id -> the connection currently exporting it
    pub bench_host: BTreeMap<String, u64>,
    /// connection id -> client daemon
    pub clients: BTreeMap<u64, ClientConn>,
    /// public token -> internal id, so an agent never sees a guessable integer
    pub tokens: BTreeMap<SessionToken, SessionId>,
    /// internal id -> which client connection owns it
    pub session_conn: BTreeMap<SessionId, u64>,
    /// Instructions we are waiting to hear about, and who owes us the answer.
    ///
    /// The lease is here so an executor reporting failure can be traced back to
    /// the lease that must be undone: without it a failed export produces a
    /// lease the agent believes is good, and a failed materialisation parks a
    /// bench for its whole TTL.
    ///
    /// The replier is here because request ids come from one counter shared
    /// between host exports and client materialisations, and start at 1.
    /// Without it any connection at all — one that has never opened a session
    /// and holds no token — could say `{"msg":"done","request":1,...}` and have
    /// the coordinator tear a lease down under its holder; a sweep of the first
    /// twenty ids cleared the lab.
    pub pending: BTreeMap<RequestId, Pending>,
    /// Teardowns whose holder has not answered yet, keyed by the instruction
    /// we are waiting on. See [`Drain`].
    drains: BTreeMap<RequestId, Drain>,
    membership: BTreeMap<String, Option<String>>,
    /// How long a bench waits for its holder to acknowledge a teardown.
    teardown_ack: u64,
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
    pub fn new(
        limits: Limits,
        vocabulary: benchd_core::tags::Vocabulary,
        teardown_ack: u64,
    ) -> Self {
        let inventory = Inventory {
            benches: Default::default(),
            vocabulary,
        };
        State {
            leases: LeaseManager::new(inventory, limits),
            hosts: BTreeMap::new(),
            bench_host: BTreeMap::new(),
            clients: BTreeMap::new(),
            tokens: BTreeMap::new(),
            session_conn: BTreeMap::new(),
            pending: BTreeMap::new(),
            drains: BTreeMap::new(),
            membership: BTreeMap::new(),
            teardown_ack,
            channels: BTreeMap::new(),
            next_request: 1,
            next_conn: 1,
        }
    }

    /// Note that a peer is alive.
    ///
    /// Keyed by connection, so a heartbeat still counts while another
    /// connection owns the bench — and, more importantly, so a host whose
    /// bench was withdrawn is still visibly alive rather than silently
    /// untouchable.
    pub fn touch_host(&mut self, conn_id: u64, now: u64) {
        if let Some(host) = self.hosts.get_mut(&conn_id) {
            host.last_seen = now;
        }
    }

    /// Make this connection the host of a bench, displacing nothing.
    ///
    /// Callers deal with the incumbent first; this only installs the route.
    pub fn register_host(
        &mut self,
        conn_id: u64,
        mut bench: Bench,
        out: Outbox,
        hangup: Arc<Notify>,
        now: u64,
    ) {
        // A bench arrives matchable, unless a teardown by its previous holder
        // is still outstanding. That holder's mount does not go away because
        // the host reconnected, so neither does the hold.
        bench.enabled = !self.held_by_a_teardown(&bench.id);
        self.membership
            .insert(bench.id.clone(), bench.group.clone());
        self.bench_host.insert(bench.id.clone(), conn_id);
        self.leases
            .inventory_mut()
            .benches
            .insert(bench.id.clone(), bench.clone());
        self.hosts.insert(
            conn_id,
            HostConn {
                bench,
                out,
                hangup,
                last_seen: now,
            },
        );
    }

    /// Another live connection registered for this bench, most recent first.
    ///
    /// This is what stops a bench leaving with whichever connection happened to
    /// own it last: a host displaced by a re-registration is still connected,
    /// still heartbeating, and still physically holding the hardware.
    pub fn standby_host(&self, bench: &str) -> Option<u64> {
        self.hosts
            .iter()
            .rev()
            .find(|(_, host)| host.bench.id == bench)
            .map(|(conn_id, _)| *conn_id)
    }

    pub fn touch_client(&mut self, conn_id: u64, now: u64) {
        if let Some(client) = self.clients.get_mut(&conn_id) {
            client.last_seen = now;
        }
    }

    /// Host connections that have stopped answering.
    ///
    /// Returned rather than acted on, so the caller can withdraw them with the
    /// usual effect ordering.
    pub fn silent_hosts(&self, now: u64, timeout: u64) -> Vec<u64> {
        self.hosts
            .iter()
            .filter(|(_, host)| now.saturating_sub(host.last_seen) > timeout)
            .map(|(conn_id, _)| *conn_id)
            .collect()
    }

    /// Hold `benches` out of the matcher until `request` is answered.
    ///
    /// `enabled` is the flag the inventory already has for "in the inventory
    /// but not matchable", and nothing else in the coordinator writes it: a
    /// bench arrives by registration and arrives enabled.
    fn hold(
        &mut self,
        request: RequestId,
        benches: Vec<String>,
        exclusive_group: Option<String>,
        now: u64,
    ) {
        for id in &benches {
            if let Some(bench) = self.leases.inventory_mut().benches.get_mut(id) {
                bench.enabled = false;
            }
        }
        let deadline = now + self.teardown_ack;
        let groups = benches
            .iter()
            .filter_map(|id| self.membership.get(id).cloned().flatten())
            .collect();
        self.drains.insert(
            request,
            Drain {
                benches,
                deadline,
                exclusive_group,
                groups,
            },
        );
        self.sync_group_drains();
    }

    /// The holder has answered: its benches can be allocated again.
    ///
    /// Idempotent, because the reaper races voluntary releases and neither path
    /// may fail.
    pub fn release_hold(&mut self, request: RequestId) {
        if let Some(drain) = self.drains.remove(&request) {
            self.reenable(&drain.benches);
        }
    }

    /// Give up on holds nobody is going to answer.
    ///
    /// The bounded escape: a client that never replies — because it died, or
    /// because its unmaterialisation failed outright — costs its bench this
    /// wait and no more.
    pub fn expire_holds(&mut self, now: u64) {
        let overdue: Vec<RequestId> = self
            .drains
            .iter()
            .filter(|(_, drain)| now >= drain.deadline)
            .map(|(request, _)| *request)
            .collect();
        for request in overdue {
            let drain = self.drains.remove(&request).expect("just listed");
            tracing::warn!(
                request = request.0, benches = ?drain.benches,
                "no acknowledgement of a teardown; allowing the bench to be claimed anyway"
            );
            self.reenable(&drain.benches);
        }
    }

    fn sync_group_drains(&mut self) {
        self.leases.draining_groups = self
            .drains
            .values()
            .flat_map(|d| d.groups.iter().cloned())
            .collect();
        self.leases.group_drains = self
            .drains
            .values()
            .filter_map(|d| {
                d.exclusive_group.as_ref().map(|g| {
                    (
                        g.clone(),
                        benchd_core::matcher::BusyInfo {
                            owner: "teardown".into(),
                            expires_in: None,
                            reason: format!("exclusive group {g} is still releasing"),
                        },
                    )
                })
            })
            .collect();
    }

    /// Diagnose the whole topology, including temporarily disabled members.
    /// A draining match for one slot does not make a cross-group or distinctness
    /// conflict satisfiable. Exhaustion remains retryable because it proves nothing.
    pub fn possible_after_teardown(&self, request: &benchd_core::model::ClaimRequest) -> bool {
        let benches: Vec<_> = self
            .leases
            .inventory()
            .benches
            .values()
            .cloned()
            .map(|mut bench| {
                if self.held_by_a_teardown(&bench.id) {
                    bench.enabled = true;
                }
                bench
            })
            .collect();
        match benchd_core::matcher::allocate(
            request,
            &benches.iter().collect::<Vec<_>>(),
            &BTreeMap::new(),
            self.leases.inventory().vocabulary.key_weights(),
        ) {
            Ok(_) => true,
            Err(error) => !error.unsatisfiable(),
        }
    }

    pub fn membership_allowed(&self, id: &str, group: &Option<String>) -> Result<(), String> {
        if let Some(old) = self.membership.get(id) {
            if old != group
                && (self.held_by_a_teardown(id)
                    || self.leases.leases().any(|l| l.benches().any(|b| b == id))
                    || old
                        .iter()
                        .chain(group.iter())
                        .any(|g| self.leases.group_reserved(g)))
            {
                return Err(format!(
                    "cannot change group for {id}: bench or group is held or releasing"
                ));
            }
        }
        Ok(())
    }

    fn reenable(&mut self, benches: &[String]) {
        self.sync_group_drains();
        for id in benches {
            // Not while another teardown still holds it. A bench can be
            // withdrawn and re-registered while a drain on it is outstanding,
            // which leaves two overlapping.
            if self.held_by_a_teardown(id) {
                continue;
            }
            if let Some(bench) = self.leases.inventory_mut().benches.get_mut(id) {
                bench.enabled = true;
            }
        }
    }

    fn held_by_a_teardown(&self, bench: &str) -> bool {
        self.drains
            .values()
            .any(|drain| drain.benches.iter().any(|id| id == bench))
    }

    /// Benches held out of the matcher by an unacknowledged teardown.
    ///
    /// The claim path needs these: to the matcher a held bench is
    /// indistinguishable from one that does not exist, and that answer tells an
    /// agent the capability will never appear when in fact it is seconds away.
    pub fn draining_benches(&self) -> Vec<&Bench> {
        self.drains
            .values()
            .flat_map(|drain| &drain.benches)
            .filter_map(|id| self.leases.inventory().benches.get(id))
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
        if spec
            .group
            .as_ref()
            .is_some_and(|g| !benchd_core::model::valid_component(g))
        {
            return Err("invalid group: must be a plain path component".into());
        }
        self.membership_allowed(&spec.id, &spec.group)?;
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
        // The displaced connection is kept as a standby rather than forgotten,
        // so two hosts genuinely configured for one bench take turns. Without
        // that they did not flap either, whatever this warning used to claim:
        // the bench simply left with whichever of them disconnected first and
        // never came back.
        if let Some(&old) = self.bench_host.get(&spec.id) {
            tracing::warn!(
                bench = %spec.id, old,
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
        // Not `Vocabulary::check`: a host must not declare a `name=` tag, which
        // the coordinator appends below from the id the host registered under.
        // Nothing stops a host on the LAN claiming to be another bench, and a
        // forged identity would be indistinguishable from the real one.
        benchd_core::model::check_declared_tags(vocabulary, &declared)
            .map_err(|e| e.to_string())?;

        let mut tags = vocabulary.expand(&declared);
        let name_tag = benchd_core::tags::Tag::parse(&format!("name={}", spec.id))
            .map_err(|e| format!("bench id is not usable as a tag value: {e}"))?;
        tags.insert(name_tag);

        Ok(Bench {
            group: spec.group.clone(),
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
        // Which benches each teardown in this batch covers.
        //
        // `Effect::Unmaterialize` names only the lease, and by the time
        // teardown effects exist the lease has already left the lease table, so
        // there is nothing left to ask. The `Unexport`s beside it name the
        // benches, and teardown always emits the two together.
        let mut torn_down: BTreeMap<LeaseId, Vec<String>> = BTreeMap::new();
        for effect in &effects {
            if let Effect::Unexport { bench, lease, .. } = effect {
                torn_down.entry(*lease).or_default().push(bench.clone());
            }
        }

        let now = crate::now();
        let mut out = Vec::new();
        for effect in effects {
            match effect {
                Effect::Export {
                    bench,
                    lease,
                    epoch,
                    session,
                } => {
                    let Some((conn_id, host_out)) = self.host_for(&bench) else {
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
                    self.pending.insert(
                        request,
                        Pending {
                            lease,
                            replier: Replier::Host(conn_id),
                        },
                    );
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
                    let Some((_, host_out)) = self.host_for(&bench) else {
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
                    let Some((conn_id, conn)) = self.client_for(session) else {
                        continue;
                    };
                    let epoch = self.lease_epoch(lease);
                    let handles = self.handles_for(&slots, lease);
                    let request = self.next_request();
                    self.pending.insert(
                        request,
                        Pending {
                            lease,
                            replier: Replier::Client(conn_id),
                        },
                    );
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
                    exclusive_group,
                    lease,
                    session,
                    epoch,
                } => {
                    let Some((conn_id, conn)) = self.client_for(session) else {
                        // Nobody to ask, so nothing to wait for. Holding the
                        // bench for an acknowledgement that cannot arrive
                        // would strand it for the whole deadline.
                        continue;
                    };
                    let request = self.next_request();
                    self.pending.insert(
                        request,
                        Pending {
                            lease,
                            replier: Replier::Client(conn_id),
                        },
                    );
                    // The benches this lease held do not go back into the
                    // allocatable pool until the client says it has let go.
                    // Sending this before the `Unexport` is not enough on its
                    // own: the lease is already out of the lease table, so
                    // without the hold the next claim can be granted the bench
                    // and the host told to export it again while the previous
                    // holder is still inside a USB/IP detach.
                    if let Some(benches) = torn_down.remove(&lease) {
                        self.hold(request, benches, exclusive_group, now);
                    }
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
                    let Some((_, conn)) = self.client_for(session) else {
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

    fn client_for(&self, session: SessionId) -> Option<(u64, Outbox)> {
        let conn_id = *self.session_conn.get(&session)?;
        let client = self.clients.get(&conn_id)?;
        Some((conn_id, client.out.clone()))
    }

    /// The connection currently exporting a bench.
    fn host_for(&self, bench: &str) -> Option<(u64, Outbox)> {
        let conn_id = *self.bench_host.get(bench)?;
        let host = self.hosts.get(&conn_id)?;
        Some((conn_id, host.out.clone()))
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

#[cfg(test)]
mod grouping_tests {
    use super::*;
    use benchd_core::model::{ClaimRequest, Distinct, Grouping, Requirement};

    fn setup() -> (State, SessionId) {
        let inventory = Inventory::from_toml_str(
            r#"
            open_keys = ["name"]
            [benches.one]
            group = "setup"
            tags = []
            [benches.two]
            group = "setup"
            tags = []
            [benches.other]
            group = "other"
            tags = []
        "#,
        )
        .unwrap();
        let mut state = State::new(Limits::default(), inventory.vocabulary.clone(), 10);
        for bench in inventory.benches.values() {
            state
                .membership
                .insert(bench.id.clone(), bench.group.clone());
        }
        state.leases = LeaseManager::new(inventory, Limits::default());
        let session = state.leases.register("test");
        (state, session)
    }

    fn claim(
        state: &mut State,
        session: SessionId,
        bench: &str,
        mode: Grouping,
    ) -> Result<benchd_core::Granted, benchd_core::ClaimError> {
        state.leases.claim(
            session,
            &ClaimRequest {
                slots: [(
                    "dut".into(),
                    Requirement::parse([format!("name={bench}")].iter().map(String::as_str))
                        .unwrap(),
                )]
                .into(),
                grouping: mode,
                distinct: Distinct::All,
                ttl_seconds: 60,
                reason: String::new(),
            },
            0,
        )
    }

    #[test]
    fn exclusive_drain_blocks_new_members_until_ack_or_deadline() {
        for acknowledge in [true, false] {
            let (mut state, session) = setup();
            state.hold(RequestId(1), vec!["one".into()], Some("setup".into()), 0);
            state.leases.inventory_mut().benches.remove("one");
            let mut added = state.leases.inventory().benches["two"].clone();
            added.id = "new".into();
            state
                .leases
                .inventory_mut()
                .benches
                .insert("new".into(), added);
            assert!(state.leases.busy(0).contains_key("new"));
            assert!(claim(&mut state, session, "two", Grouping::None).is_err());
            assert!(state.membership_allowed("one", &None).is_err());
            state.expire_holds(9);
            assert!(state.leases.group_reserved("setup"));
            if acknowledge {
                state.release_hold(RequestId(1));
            } else {
                state.expire_holds(10);
            }
            assert!(!state.leases.group_reserved("setup"));
            assert!(state.membership_allowed("one", &None).is_ok());
            assert!(claim(&mut state, session, "two", Grouping::Exclusive).is_ok());
        }
    }

    #[test]
    fn ordinary_drain_blocks_exclusive_admission_but_not_disjoint_normal_claims() {
        let (mut state, session) = setup();
        state.hold(RequestId(1), vec!["one".into()], None, 0);
        state.leases.inventory_mut().benches.remove("one");
        assert!(claim(&mut state, session, "two", Grouping::Exclusive).is_err());
        assert!(claim(&mut state, session, "two", Grouping::Same).is_ok());
    }

    #[test]
    fn membership_cannot_move_a_held_bench_or_enter_or_leave_a_reserved_group() {
        let (mut state, session) = setup();
        let grant = claim(&mut state, session, "one", Grouping::Same).unwrap();
        assert!(state.membership_allowed("one", &None).is_err());
        assert!(state.membership_allowed("two", &None).is_ok());
        state.leases.release(session, grant.lease, 0).unwrap();
        claim(&mut state, session, "one", Grouping::Exclusive).unwrap();
        assert!(state.membership_allowed("two", &None).is_err());
        assert!(state
            .membership_allowed("other", &Some("setup".into()))
            .is_err());
        assert!(state
            .membership_allowed("two", &Some("setup".into()))
            .is_ok());
        // A genuinely new member may join; busy() then blocks it automatically.
        assert!(state
            .membership_allowed("new", &Some("setup".into()))
            .is_ok());
    }
}
