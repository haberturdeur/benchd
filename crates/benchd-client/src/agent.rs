//! The local agent socket: where per-agent MCP shims connect.
//!
//! Each shim owns one session per coordinator. The daemon multiplexes every
//! shim onto every coordinator link, correlating replies by request id and
//! routing unsolicited lease events to whichever agent holds the lease.
//!
//! This is also where the single-lab illusion is built and where it can break:
//! the agent must see one merged view, so a request answered by only some of
//! the coordinators is not an answer, and no request may go unanswered or be
//! answered twice.
//!
//! Requests are forwarded almost verbatim — the daemon rewrites request ids into
//! its own space and substitutes the session token, but does not interpret
//! claims. Policy lives in the coordinator; this is plumbing.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::ids::{CoordinatorId, LeaseKey, SessionKey};
use anyhow::Result;
use benchd_core::lease::SessionId;
use benchd_core::wire::{
    ClientMsg, CoordinatorInventory, OperatorMsg, RequestId, SessionToken, ToClient, ToOperator,
};
use futures::{future::join_all, SinkExt, StreamExt};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

use crate::materialize::owner_dir;
use crate::Shared;

/// How long the daemon will wait for the coordinators before answering a
/// request itself.
///
/// Everything below is completed by events — a fanout by its last reply, a
/// claim by a grant or by running out of coordinators — and a coordinator that
/// stays connected and simply never answers is not an event. This is the only
/// deadline on this side of the agent socket: the MCP shim has one, but at 30s
/// and on the far side, where it can clean nothing up, and `benchd lease` has
/// none at all and waits for a matching request id forever. Comfortably under
/// the shim's, so the agent is given a reason rather than a timeout.
const REQUEST_DEADLINE: Duration = Duration::from_secs(20);

/// How long a request waits for a coordinator that is connected but has not
/// finished re-registering.
///
/// Bounded, because a coordinator that accepts a connection and never completes
/// the handshake must not hold up every request on the machine.
const HANDSHAKE_GRACE: Duration = Duration::from_secs(5);

/// One connected MCP shim.
struct Agent {
    name: String,
    /// One session per coordinator: each is an independent authority and issues
    /// its own token, so an agent holds several at once and the daemon picks the
    /// right one per request.
    sessions: BTreeMap<CoordinatorId, SessionToken>,
    internal: BTreeMap<CoordinatorId, SessionId>,
    /// The uid on the other end of the unix socket, from `SO_PEERCRED`.
    ///
    /// A materialised device node keeps the source device's ownership, which is
    /// typically `root:uucp` and mode 0660 — so an unprivileged agent cannot
    /// open the device it was just granted. Since the lease is exclusive and
    /// this daemon is root, the node is handed to the agent's own uid for the
    /// duration and given back on release.
    uid: u32,
    out: mpsc::UnboundedSender<String>,
}

#[derive(Default)]
pub struct Agents {
    inner: Mutex<Inner>,
    /// Woken whenever a session appears or a registration gives up, so a
    /// request that arrives mid-handshake can wait for it without polling.
    registered: Notify,
}

/// A claim working its way down the coordinator list.
struct ClaimAttempt {
    claim: benchd_core::wire::ClaimSpec,
    remaining: Vec<CoordinatorId>,
    /// The most actionable failure seen so far. "Everything is busy" beats
    /// "no such bench", because only the first is worth waiting for (D14).
    best_error: Option<(String, bool)>,
}

/// What the caller should do after a reply was delivered.
pub struct ClaimRetry {
    pub to: CoordinatorId,
    pub request: RequestId,
    pub session: SessionToken,
    pub claim: benchd_core::wire::ClaimSpec,
}

/// What a fanned-out request will answer with once every coordinator is in.
///
/// Recorded when the request goes out rather than inferred from the last reply
/// to arrive, because a fanout also has to be answerable when a coordinator
/// never replies at all — whether because the link went down under it, or
/// because it stayed up and the coordinator said nothing.
#[derive(Clone, Copy, Default, PartialEq)]
enum FanoutKind {
    Session,
    Tags,
    Leases,
    /// Nothing to merge; success or the most useful failure.
    #[default]
    Ack,
}

/// A request the daemon asked of several coordinators at once.
#[derive(Default)]
struct Fanout {
    kind: FanoutKind,
    outstanding: usize,
    /// How many coordinators were asked, and how many of them will never give a
    /// usable answer. A merged view is only a view of the whole lab if the
    /// second is zero.
    expected: usize,
    failed: usize,
    tags: BTreeMap<String, benchd_core::wire::TagInfo>,
    leases: Vec<benchd_core::wire::LeaseStatus>,
    /// The first session a coordinator handed back. Registration is fanned out
    /// too, and the agent must not be told it has a session until every
    /// coordinator has one — see the comment on the `OpenSession` arm of
    /// `forward`.
    session: Option<(SessionToken, SessionId)>,
    /// The most useful failure so far. A request no coordinator can ever
    /// satisfy is only unsatisfiable if *every* one says so; if any says it is
    /// merely busy, the agent should wait rather than give up (D14).
    error: Option<(String, bool)>,
}

impl Fanout {
    /// The single reply that stands for all of them.
    fn merged(&self, theirs: RequestId) -> ToClient {
        let failed = |fallback: &str| match &self.error {
            Some((error, retryable)) => ToClient::Error {
                request: theirs,
                error: error.clone(),
                retryable: *retryable,
            },
            None => ToClient::Error {
                request: theirs,
                error: fallback.into(),
                retryable: true,
            },
        };
        match self.kind {
            // A coordinator refusing a registration another accepted must not
            // cost the agent the session it did get: it would be left unable to
            // claim anything anywhere.
            FanoutKind::Session => match &self.session {
                Some((session, id)) => ToClient::SessionOpened {
                    request: theirs,
                    session: session.clone(),
                    id: *id,
                },
                None => failed("no coordinator opened a session"),
            },
            // A view assembled from part of the lab is not a view of the lab,
            // and there is nowhere in a `Tags` or a `Status` to say so.
            FanoutKind::Tags | FanoutKind::Leases if self.failed > 0 => self.incomplete(theirs),
            FanoutKind::Tags => ToClient::Tags {
                request: theirs,
                tags: self.tags.values().cloned().collect(),
            },
            FanoutKind::Leases => ToClient::Status {
                request: theirs,
                leases: self.leases.clone(),
            },
            FanoutKind::Ack => match self.failed {
                0 => ToClient::Ok { request: theirs },
                _ => failed("a coordinator did not answer"),
            },
        }
    }

    /// What to say when some coordinator could not be seen.
    ///
    /// An answer the agent cannot tell apart from a complete one is the worst
    /// of the options: `benchd mcp` renders an empty lease list as the
    /// successful "no leases held", so an agent holding a board on the lab
    /// coordinator that asks the moment the link drops is told, as a success,
    /// that it holds no hardware — and a short `tag_list` reads as a lab
    /// without that hardware in it. So an incomplete view is a failure, and
    /// always a retryable one: a link that dropped and a session a coordinator
    /// has forgotten are both conditions this daemon repairs by itself, and
    /// telling the agent otherwise makes the shim advise it not to retry.
    ///
    /// Phrased in labs rather than coordinators, and without a count: the agent
    /// has to be told its picture is incomplete, but not how many authorities
    /// there are behind it.
    fn incomplete(&self, theirs: RequestId) -> ToClient {
        let reason = match &self.error {
            Some((error, _)) => error.as_str(),
            None => "no reason given",
        };
        let error = if self.failed >= self.expected {
            format!("the lab could not be reached: {reason}")
        } else {
            format!(
                "part of the lab could not be reached, so this would be a partial \
                 picture of it rather than the whole one: {reason}"
            )
        };
        ToClient::Error {
            request: theirs,
            error,
            retryable: true,
        }
    }
}

/// Who a request was issued for.
///
/// The daemon asks things on its own account — re-registration after a
/// reconnect, the `CloseSession` that follows a disconnect, and a `Renew`
/// triggered by device traffic — and those replies are not an agent's to
/// swallow. `RequestId(0)` used to stand in for "mine", but zero is a perfectly
/// legal id for an agent to pick, so an agent with a request outstanding under
/// it swallowed the daemon's re-registration reply. A variant cannot be
/// collided with.
#[derive(Clone, Copy, PartialEq)]
enum Origin {
    Agent(RequestId),
    Daemon,
}

/// One request this daemon has outstanding with one coordinator.
struct Pending {
    agent: u64,
    origin: Origin,
    to: CoordinatorId,
    /// Set for `OpenSession`. A request that needs a session waits for one of
    /// these instead of writing the coordinator off as absent.
    opening_session: bool,
    /// Set when this is a traffic-triggered `Renew`, so the reply can update
    /// the hold without being forwarded as an agent's answer.
    auto_lease: Option<LeaseKey>,
}

#[derive(Default)]
struct Inner {
    agents: BTreeMap<u64, Agent>,
    /// Our request id -> who it was for and where it went.
    pending: BTreeMap<u64, Pending>,
    /// Requests fanned out to several coordinators, keyed by the agent's own
    /// request id: how many replies are still outstanding, and what has been
    /// collected so far. Without this a `tag_list` would answer with whichever
    /// coordinator replied first and silently hide the rest.
    fanout: BTreeMap<(u64, u64), Fanout>,
    /// Claims still looking for a coordinator that can satisfy them.
    ///
    /// A claim cannot be broadcast — two coordinators satisfying it would hold
    /// hardware the agent never asked for — so it is offered to them one at a
    /// time, in configuration order, moving on when one says no.
    claims: BTreeMap<(u64, u64), ClaimAttempt>,
    /// Which agent holds a lease, so events reach the right one.
    lease_owner: BTreeMap<LeaseKey, u64>,
    /// When each lease expires and how it has been sliding, for activity renew.
    holds: BTreeMap<LeaseKey, Hold>,
    /// Session -> directory component, for materialisation paths.
    owners: BTreeMap<SessionKey, String>,
    /// Session -> the uid that should own its device nodes.
    uids: BTreeMap<SessionKey, u32>,
    next_agent: u64,
    next_request: u64,
}

/// Enough to decide whether device traffic should slide a lease forward.
struct Hold {
    expires_at: u64,
    extra: u64,
    last_urbs: Option<u64>,
    in_flight: bool,
}

fn note_expiry(inner: &mut Inner, lease: LeaseKey, expires_at: u64) {
    let extra = expires_at
        .saturating_sub(crate::activity::unix_now())
        .max(1);
    inner
        .holds
        .entry(lease)
        .and_modify(|hold| {
            hold.expires_at = expires_at;
            hold.extra = extra;
            hold.in_flight = false;
        })
        .or_insert(Hold {
            expires_at,
            extra,
            last_urbs: None,
            in_flight: false,
        });
}

impl Agents {
    pub async fn owner_for(&self, session: SessionKey) -> String {
        let inner = self.inner.lock().await;
        inner
            .owners
            .get(&session)
            .cloned()
            .unwrap_or_else(|| session.session.to_string())
    }

    /// Which uid should be able to open this session's devices.
    /// Remember which coordinators a claim may still be offered to.
    async fn begin_claim(
        &self,
        agent_id: u64,
        theirs: RequestId,
        claim: benchd_core::wire::ClaimSpec,
        remaining: Vec<CoordinatorId>,
    ) {
        let mut inner = self.inner.lock().await;
        inner.claims.insert(
            (agent_id, theirs.0),
            ClaimAttempt {
                claim,
                remaining,
                best_error: None,
            },
        );
    }

    /// Record that `theirs` was fanned out to `n` coordinators, so the merge
    /// knows when every answer is in.
    ///
    /// The entry is always fresh: `forward` refuses a request whose id is
    /// already outstanding, so two of an agent's requests can never be folded
    /// into one fanout and answered once between them.
    async fn expect_fanout(&self, agent_id: u64, theirs: RequestId, n: usize, kind: FanoutKind) {
        let mut inner = self.inner.lock().await;
        let entry = inner.fanout.entry((agent_id, theirs.0)).or_default();
        entry.kind = kind;
        entry.outstanding += n;
        entry.expected += n;
    }

    /// Whether this agent already has a request outstanding under this id.
    ///
    /// An id is how a reply finds its question, so reusing one while the first
    /// is unanswered makes the two indistinguishable — and the second would be
    /// absorbed into the first's fanout and never answered at all.
    async fn already_outstanding(&self, agent_id: u64, theirs: RequestId) -> bool {
        let inner = self.inner.lock().await;
        inner
            .pending
            .values()
            .any(|p| p.agent == agent_id && p.origin == Origin::Agent(theirs))
    }

    /// This agent's session token on one coordinator.
    pub async fn session_on(&self, agent_id: u64, to: CoordinatorId) -> Option<SessionToken> {
        let inner = self.inner.lock().await;
        inner.agents.get(&agent_id)?.sessions.get(&to).cloned()
    }

    /// The same, but waiting out a registration that is still in flight.
    ///
    /// On a reconnect the link is published before `reregister_all` has a reply,
    /// so for a moment a coordinator is connected and has no session on it.
    /// Filtering targets on session presence writes it off as not a target,
    /// silently and without counting it — which makes `tag_list` describe a
    /// smaller lab and offers a claim only to whoever finished handshaking
    /// first, and if none of them can satisfy it the agent is told the request
    /// is unsatisfiable. Waiting is bounded twice over: by `within`, and by the
    /// registration itself disappearing from `pending` if it fails.
    async fn session_soon(
        &self,
        agent_id: u64,
        to: CoordinatorId,
        within: Duration,
    ) -> Option<SessionToken> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let woken = self.registered.notified();
            tokio::pin!(woken);
            // Enrolled before the look, or a handshake landing between the two
            // would never wake us.
            woken.as_mut().enable();
            {
                let inner = self.inner.lock().await;
                if let Some(agent) = inner.agents.get(&agent_id) {
                    if let Some(token) = agent.sessions.get(&to) {
                        return Some(token.clone());
                    }
                }
                let handshaking = inner
                    .pending
                    .values()
                    .any(|p| p.agent == agent_id && p.to == to && p.opening_session);
                if !handshaking {
                    return None;
                }
            }
            if tokio::time::timeout_at(deadline, woken).await.is_err() {
                return None;
            }
        }
    }

    pub async fn uid_for(&self, session: SessionKey) -> Option<u32> {
        self.inner.lock().await.uids.get(&session).copied()
    }

    /// Rewrite a request into our id space and remember the mapping.
    async fn track(
        &self,
        agent: u64,
        origin: Origin,
        to: CoordinatorId,
        opening_session: bool,
    ) -> RequestId {
        let mut inner = self.inner.lock().await;
        inner.next_request += 1;
        let ours = RequestId(inner.next_request);
        inner.pending.insert(
            ours.0,
            Pending {
                agent,
                origin,
                to,
                opening_session,
                auto_lease: None,
            },
        );
        ours
    }

    pub(crate) async fn track_auto_renew(&self, agent: u64, lease: LeaseKey) -> RequestId {
        let ours = self
            .track(agent, Origin::Daemon, lease.coordinator, false)
            .await;
        let mut inner = self.inner.lock().await;
        if let Some(pending) = inner.pending.get_mut(&ours.0) {
            pending.auto_lease = Some(lease);
        }
        ours
    }

    async fn attach_lease(&self, request: RequestId, lease: LeaseKey) {
        let mut inner = self.inner.lock().await;
        if let Some(pending) = inner.pending.get_mut(&request.0) {
            pending.auto_lease = Some(lease);
        }
    }

    pub(crate) async fn holder_of(&self, lease: LeaseKey) -> Option<u64> {
        self.inner.lock().await.lease_owner.get(&lease).copied()
    }

    pub(crate) async fn due_extensions(
        &self,
        urbs: &BTreeMap<LeaseKey, u64>,
        now: u64,
    ) -> Vec<(LeaseKey, SessionToken, u64)> {
        let mut inner = self.inner.lock().await;
        let mut due = Vec::new();
        let mut candidates = Vec::new();
        for (lease, hold) in inner.holds.iter_mut() {
            let Some(&count) = urbs.get(lease) else {
                continue;
            };
            let grew = hold.last_urbs.is_some_and(|prev| count > prev);
            hold.last_urbs = Some(count);
            if crate::activity::should_extend(
                now,
                hold.expires_at,
                hold.extra,
                grew,
                hold.in_flight,
            ) {
                candidates.push((*lease, hold.extra));
            }
        }
        for (lease, extra) in candidates {
            let Some(agent_id) = inner.lease_owner.get(&lease) else {
                continue;
            };
            let Some(session) = inner
                .agents
                .get(agent_id)
                .and_then(|agent| agent.sessions.get(&lease.coordinator))
                .cloned()
            else {
                continue;
            };
            if let Some(hold) = inner.holds.get_mut(&lease) {
                hold.in_flight = true;
            }
            due.push((lease, session, extra));
        }
        due
    }

    /// Drop an agent and everything keyed by it, returning the sessions that
    /// still need closing.
    ///
    /// Only the agent row used to go, so the other five maps outlived it — and
    /// a shim is one process per agent session, which makes that a leak per
    /// session for the life of a daemon expected to run for months.
    ///
    /// `owners` and `uids` go too. A lease this process materialised is
    /// recorded in the materialiser against the directory it actually used, so
    /// the teardown that follows the `CloseSession` below does not need them;
    /// they exist to place a `Materialize` that has not happened yet, and no
    /// more of those are coming for an agent that has gone.
    async fn forget_agent(&self, agent_id: u64) -> BTreeMap<CoordinatorId, SessionToken> {
        let mut inner = self.inner.lock().await;
        let Some(agent) = inner.agents.remove(&agent_id) else {
            return BTreeMap::new();
        };
        for (coordinator, id) in &agent.internal {
            let key = SessionKey::new(*coordinator, *id);
            inner.owners.remove(&key);
            inner.uids.remove(&key);
        }
        inner.fanout.retain(|(a, _), _| *a != agent_id);
        inner.claims.retain(|(a, _), _| *a != agent_id);
        inner.pending.retain(|_, p| p.agent != agent_id);
        let gone: Vec<_> = inner
            .lease_owner
            .iter()
            .filter(|(_, a)| **a == agent_id)
            .map(|(k, _)| *k)
            .collect();
        for lease in gone {
            inner.holds.remove(&lease);
            inner.lease_owner.remove(&lease);
        }
        agent.sessions
    }

    /// Answer a request whose deadline ran out, and forget it.
    ///
    /// The backstop for every way a request can stop making progress without
    /// anything happening: a coordinator that holds the connection open and
    /// never replies, a claim walking a list of coordinators that all go quiet,
    /// a `renew` sent to a link that is up but wedged. Answering here is safe
    /// exactly once, because every path that answers first removes what it
    /// answered — the fanout entry, the claim, and the `pending` rows behind
    /// them — so there is nothing left for this to find.
    async fn expire(&self, agent_id: u64, theirs: RequestId) {
        let mut inner = self.inner.lock().await;
        let key = (agent_id, theirs.0);
        const TIMED_OUT: &str = "a coordinator did not answer in time";

        if let Some(entry) = inner.fanout.get_mut(&key) {
            entry.failed += entry.outstanding;
            entry.outstanding = 0;
            keep_best_error(&mut entry.error, TIMED_OUT, true);
            settle_fanout(&mut inner, agent_id, theirs);
            return;
        }
        if let Some(attempt) = inner.claims.get_mut(&key) {
            keep_best_error(&mut attempt.best_error, TIMED_OUT, true);
            finish_claim(&mut inner, agent_id, theirs);
            return;
        }
        // A lease-bearing request goes to one coordinator and has neither a
        // fanout nor a claim behind it.
        if forget_request(&mut inner, agent_id, theirs) {
            send_to(
                &inner,
                agent_id,
                &ToClient::Error {
                    request: theirs,
                    error: TIMED_OUT.into(),
                    retryable: true,
                },
            );
        }
    }

    /// Route a coordinator reply back to the agent that asked, restoring its
    /// own request id.
    pub async fn deliver_reply(&self, from: CoordinatorId, msg: ToClient) -> Option<ClaimRetry> {
        let ours = match &msg {
            ToClient::SessionOpened { request, .. }
            | ToClient::Ok { request }
            | ToClient::Error { request, .. }
            | ToClient::Granted { request, .. }
            | ToClient::Renewed { request, .. }
            | ToClient::Status { request, .. }
            | ToClient::Tags { request, .. }
            | ToClient::Inventory { request, .. } => request.0,
            _ => return None,
        };

        let mut inner = self.inner.lock().await;
        let Some(pending) = inner.pending.remove(&ours) else {
            tracing::debug!(ours, "reply for an unknown request");
            return None;
        };
        let agent_id = pending.agent;

        let theirs = match pending.origin {
            Origin::Agent(theirs) => theirs,
            // Ours, not the agent's. All it leaves behind is the session, and
            // whoever is waiting for that wants to know either way.
            Origin::Daemon => {
                if let ToClient::SessionOpened { session, id, .. } = &msg {
                    record_session(&mut inner, agent_id, from, session, *id);
                    // Says nothing about which coordinator: the agent is not
                    // told there is more than one.
                    send_event(
                        &inner,
                        agent_id,
                        serde_json::json!({
                            "msg": "reconnected",
                            "detail": "a coordinator came back and your session \
                                       there was re-registered."
                        }),
                    );
                }
                if let (Some(lease), ToClient::Renewed { expires_at, .. }) =
                    (pending.auto_lease, &msg)
                {
                    note_expiry(&mut inner, lease, *expires_at);
                    send_event(
                        &inner,
                        agent_id,
                        serde_json::json!({
                            "msg": "renewed",
                            "lease": lease.to_public(),
                            "expires_at": expires_at,
                            "reason": "device in use",
                        }),
                    );
                }
                if let (Some(lease), ToClient::Error { error, .. }) = (pending.auto_lease, &msg) {
                    if let Some(hold) = inner.holds.get_mut(&lease) {
                        hold.in_flight = false;
                        if error.contains("maximum total hold")
                            || error.contains("revoked by an operator")
                        {
                            hold.extra = 0;
                        }
                    }
                    tracing::warn!(%lease, error, "activity renew refused");
                }
                self.registered.notify_waiters();
                return None;
            }
        };

        // A claim that one coordinator could not satisfy moves to the next.
        let claim_key = (agent_id, theirs.0);
        if inner.claims.contains_key(&claim_key) {
            match &msg {
                ToClient::Granted { .. } => {
                    inner.claims.remove(&claim_key);
                }
                ToClient::Error {
                    error, retryable, ..
                } => {
                    if let Some(attempt) = inner.claims.get_mut(&claim_key) {
                        keep_best_error(&mut attempt.best_error, error, *retryable);
                    }
                    if let Some(retry) = next_claim_target(&mut inner, agent_id, theirs) {
                        return Some(retry);
                    }
                    finish_claim(&mut inner, agent_id, theirs);
                    return None;
                }
                _ => {}
            }
        }

        // Remember the session and lease ownership as they are handed out, so
        // later events and materialisations can be routed without asking.
        match &msg {
            ToClient::SessionOpened { session, id, .. } => {
                record_session(&mut inner, agent_id, from, session, *id);
                self.registered.notify_waiters();
            }
            ToClient::Granted {
                lease, expires_at, ..
            } => {
                inner
                    .lease_owner
                    .insert(LeaseKey::new(from, *lease), agent_id);
                note_expiry(&mut inner, LeaseKey::new(from, *lease), *expires_at);
            }
            ToClient::Renewed { expires_at, .. } => {
                if let Some(lease) = pending.auto_lease {
                    note_expiry(&mut inner, lease, *expires_at);
                }
            }
            _ => {}
        }

        // Fanned-out requests are merged rather than raced: `tag_list` and
        // `lease_status` must describe every coordinator, and a request that
        // failed everywhere should report the most actionable reason.
        //
        // Whether a reply belongs to a fanout is decided by the fanout table
        // and not by the shape of the reply. Listing the shapes missed
        // `ToClient::Ok`, which is precisely what a coordinator answers a
        // `CloseSession` broadcast with — so every one of them was forwarded
        // verbatim as a second reply to a request already answered, and the
        // entry, never decremented, stayed to absorb some later error.
        let fanout_key = (agent_id, theirs.0);
        if inner.fanout.contains_key(&fanout_key) {
            let entry = inner.fanout.entry(fanout_key).or_default();
            entry.outstanding = entry.outstanding.saturating_sub(1);
            match &msg {
                ToClient::SessionOpened { session, id, .. } => {
                    entry.session.get_or_insert((session.clone(), *id));
                }
                ToClient::Tags { tags, .. } => merge_tags(&mut entry.tags, tags),
                ToClient::Status { leases, .. } => {
                    // Rewrite to public ids here too, or two coordinators that
                    // both issued `l1` would show up as one lease twice and
                    // `release` would be ambiguous.
                    entry.leases.extend(leases.iter().cloned().map(|mut l| {
                        l.lease =
                            benchd_core::lease::LeaseId(LeaseKey::new(from, l.lease).to_public());
                        l
                    }));
                }
                ToClient::Error {
                    error, retryable, ..
                } => {
                    entry.failed += 1;
                    keep_best_error(&mut entry.error, error, *retryable);
                }
                _ => {}
            }
            settle_fanout(&mut inner, agent_id, theirs);
            return None;
        }

        let agent = inner.agents.get(&agent_id)?;
        let msg = with_request(msg, theirs);
        // The lease id is rewritten so the agent works with one opaque number
        // and never learns which coordinator granted it.
        let msg = match msg {
            ToClient::Granted {
                request,
                lease,
                slots,
                docs,
                expires_at,
                note,
            } => ToClient::Granted {
                request,
                lease: benchd_core::lease::LeaseId(LeaseKey::new(from, lease).to_public()),
                slots,
                docs,
                expires_at,
                note,
            },
            other => other,
        };
        if let Ok(line) = serde_json::to_string(&msg) {
            let _ = agent.out.send(line);
        }
        None
    }

    /// A claim is only useful once the device nodes exist, so the shim is told
    /// where they are as a separate message after materialisation succeeds.
    pub async fn deliver_paths(
        &self,
        lease: LeaseKey,
        paths: BTreeMap<String, BTreeMap<String, String>>,
    ) {
        let inner = self.inner.lock().await;
        let Some(agent_id) = inner.lease_owner.get(&lease) else {
            return;
        };
        let Some(agent) = inner.agents.get(agent_id) else {
            return;
        };
        let msg = serde_json::json!({
            "msg": "paths", "lease": lease.to_public(), "slots": paths
        });
        let _ = agent.out.send(msg.to_string());
    }

    pub async fn notify_revoking(&self, lease: LeaseKey, reason: &str, teardown_at: u64) {
        self.notify(
            lease,
            serde_json::json!({
                "msg": "revoking", "lease": lease.to_public(), "reason": reason, "teardown_at": teardown_at
            }),
        )
        .await;
    }

    /// A lease was withdrawn before it ever worked. Unlike `ended`, the agent
    /// is probably still blocked waiting for device paths, so this must reach it.
    pub async fn notify_failed(&self, lease: LeaseKey, detail: &str) {
        // Deliberately does NOT forget the lease owner: the teardown for this
        // lease runs afterwards and still needs it to find the right directory.
        // It is cleaned up by notify_ended, or when the agent disconnects.
        self.notify(
            lease,
            serde_json::json!({
                "msg": "failed", "lease": lease.to_public(), "detail": detail
            }),
        )
        .await;
    }

    /// The internal session id that owns a lease, for path construction.
    pub async fn session_for_lease(&self, lease: LeaseKey) -> SessionKey {
        let inner = self.inner.lock().await;
        inner
            .lease_owner
            .get(&lease)
            .and_then(|agent| inner.agents.get(agent))
            .and_then(|a| a.internal.get(&lease.coordinator).copied())
            .map(|id| SessionKey::new(lease.coordinator, id))
            .unwrap_or_else(|| SessionKey::new(lease.coordinator, SessionId(0)))
    }

    pub async fn notify_ended(&self, lease: LeaseKey, reason: &str) {
        self.notify(
            lease,
            serde_json::json!({
                "msg": "ended", "lease": lease.to_public(), "reason": reason
            }),
        )
        .await;
        let mut inner = self.inner.lock().await;
        inner.lease_owner.remove(&lease);
        inner.holds.remove(&lease);
    }

    async fn notify(&self, lease: LeaseKey, msg: serde_json::Value) {
        let inner = self.inner.lock().await;
        let Some(agent_id) = inner.lease_owner.get(&lease) else {
            return;
        };
        let Some(agent) = inner.agents.get(agent_id) else {
            return;
        };
        let _ = agent.out.send(msg.to_string());
    }

    /// The coordinator link dropped, so every session token we hold is void.
    ///
    /// The agents' unix sockets are still open — only the upstream link broke —
    /// so they are re-registered automatically when it comes back. An earlier
    /// version cleared the session and left the shim to notice, but a shim only
    /// registers once at startup and its socket never closed, so a one-second
    /// coordinator restart wedged every agent on the machine permanently.
    ///
    /// Per coordinator, not global: a lab server restarting must not invalidate
    /// sessions held against the local coordinator that owns this machine's own
    /// boards. They are independent authorities.
    pub async fn invalidate(&self, coordinator: CoordinatorId) {
        let mut inner = self.inner.lock().await;

        // Every request this coordinator still owes an answer to. It will never
        // give one, and an agent blocked on it would stay blocked for as long
        // as the link stayed down. Registration is fanned out too, so without
        // this a coordinator dying at the wrong moment wedges every agent on
        // the machine at startup.
        //
        // The window is wide, not a race: the reconnect path unmounts and
        // detaches every resource before it gets here and only removes the link
        // afterwards, so for those seconds the coordinator still looks live and
        // still has a session, and every request aimed at it in that time ends
        // up here.
        const GONE: &str = "a coordinator went away before answering";
        let orphaned: Vec<(u64, RequestId)> = inner
            .pending
            .values()
            .filter(|p| p.to == coordinator)
            .filter_map(|p| match p.origin {
                Origin::Agent(theirs) => Some((p.agent, theirs)),
                Origin::Daemon => None,
            })
            .collect();
        inner.pending.retain(|_, p| p.to != coordinator);

        for (agent_id, theirs) in orphaned {
            let key = (agent_id, theirs.0);
            if let Some(entry) = inner.fanout.get_mut(&key) {
                entry.outstanding = entry.outstanding.saturating_sub(1);
                entry.failed += 1;
                keep_best_error(&mut entry.error, GONE, true);
                settle_fanout(&mut inner, agent_id, theirs);
                continue;
            }
            // A claim is deliberately not a fanout — broadcasting one would let
            // two coordinators satisfy it at once — so it lives in `claims`,
            // which this used to walk straight past. The claim was then dropped
            // on the floor: no reply, the remaining coordinators never tried,
            // and the entry left behind.
            if let Some(attempt) = inner.claims.get_mut(&key) {
                keep_best_error(&mut attempt.best_error, GONE, true);
                finish_claim(&mut inner, agent_id, theirs);
                continue;
            }
            // A lease-bearing request, which only ever went to this one.
            send_to(
                &inner,
                agent_id,
                &ToClient::Error {
                    request: theirs,
                    error: GONE.into(),
                    retryable: true,
                },
            );
        }

        inner
            .lease_owner
            .retain(|k, _| k.coordinator != coordinator);
        inner.holds.retain(|k, _| k.coordinator != coordinator);
        inner.owners.retain(|k, _| k.coordinator != coordinator);
        inner.uids.retain(|k, _| k.coordinator != coordinator);
        for agent in inner.agents.values_mut() {
            if agent.sessions.remove(&coordinator).is_some() {
                agent.internal.remove(&coordinator);
                // Which coordinator is deliberately absent: naming it would
                // tell the agent there is more than one, and let it count them.
                let msg = serde_json::json!({
                    "msg": "disconnected",
                    "detail": "a coordinator restarted; its leases are void. \
                               Your session there is being re-registered."
                });
                let _ = agent.out.send(msg.to_string());
            }
        }
        drop(inner);
        // Anything waiting for a handshake on this coordinator is waiting for
        // one that is no longer in flight.
        self.registered.notify_waiters();
    }

    /// Make sure this agent has a session, or one on the way, on every
    /// coordinator that is up.
    ///
    /// A registration can be lost with nothing noticing: a coordinator can
    /// refuse one, or the reply can be in flight when the link goes down.
    /// Nothing retried, so that coordinator stayed connected and sessionless
    /// for the life of the daemon — which used to silently shrink every merged
    /// view, and now that a short view is reported as a failure would make
    /// every one of them fail instead. Idempotent, so it is safe on the path of
    /// every request.
    async fn ensure_registered(&self, shared: &Arc<Shared>, agent_id: u64) {
        let name = {
            let inner = self.inner.lock().await;
            match inner.agents.get(&agent_id) {
                // Never registered: there is nothing to restore, and inventing
                // a registration the agent did not ask for would hand it a
                // session it does not know it has.
                Some(agent) if !agent.name.is_empty() => agent.name.clone(),
                _ => return,
            }
        };
        for to in shared.live_links().await {
            {
                let inner = self.inner.lock().await;
                let known = inner
                    .agents
                    .get(&agent_id)
                    .is_some_and(|a| a.sessions.contains_key(&to));
                let in_flight = inner
                    .pending
                    .values()
                    .any(|p| p.agent == agent_id && p.to == to && p.opening_session);
                if known || in_flight {
                    continue;
                }
            }
            let request = self.track(agent_id, Origin::Daemon, to, true).await;
            tracing::info!(agent_id, %name, coordinator = %to, "registering again");
            shared
                .send(
                    to,
                    &ClientMsg::OpenSession {
                        request,
                        name: name.clone(),
                    },
                )
                .await;
        }
    }

    /// Re-register every connected agent after the link is restored.
    ///
    /// Names are remembered per agent precisely so this can happen without the
    /// shim doing anything.
    pub async fn reregister_all(&self, shared: &Shared, to: CoordinatorId) {
        let agents: Vec<(u64, String)> = {
            let inner = self.inner.lock().await;
            let in_flight = |id: &u64| {
                inner
                    .pending
                    .values()
                    .any(|p| p.agent == *id && p.to == to && p.opening_session)
            };
            inner
                .agents
                .iter()
                .filter(|(id, a)| {
                    !a.sessions.contains_key(&to) && !a.name.is_empty() && !in_flight(id)
                })
                .map(|(id, a)| (*id, a.name.clone()))
                .collect()
        };
        for (agent_id, name) in agents {
            let request = self.track(agent_id, Origin::Daemon, to, true).await;
            tracing::info!(agent_id, %name, coordinator = %to, "re-registering");
            shared
                .send(to, &ClientMsg::OpenSession { request, name })
                .await;
        }
    }
}

/// Remember a session and where its device nodes will live, so a later
/// `Materialize` can be placed without another round trip.
fn record_session(
    inner: &mut Inner,
    agent_id: u64,
    from: CoordinatorId,
    session: &SessionToken,
    id: SessionId,
) {
    let Some(agent) = inner.agents.get_mut(&agent_id) else {
        return;
    };
    agent.sessions.insert(from, session.clone());
    agent.internal.insert(from, id);
    let (name, uid) = (agent.name.clone(), agent.uid);
    let key = SessionKey::new(from, id);
    inner.owners.insert(key, owner_dir(id, &name));
    inner.uids.insert(key, uid);
}

fn send_to(inner: &Inner, agent_id: u64, msg: &ToClient) {
    let Some(agent) = inner.agents.get(&agent_id) else {
        return;
    };
    if let Ok(line) = serde_json::to_string(msg) {
        let _ = agent.out.send(line);
    }
}

fn send_event(inner: &Inner, agent_id: u64, msg: serde_json::Value) {
    if let Some(agent) = inner.agents.get(&agent_id) {
        let _ = agent.out.send(msg.to_string());
    }
}

/// Drop whatever is still outstanding under one of an agent's request ids, and
/// say whether there was anything.
///
/// Answering a request means nothing else may answer it, so the rows that would
/// let a late reply through have to go with the answer.
fn forget_request(inner: &mut Inner, agent_id: u64, theirs: RequestId) -> bool {
    let before = inner.pending.len();
    inner
        .pending
        .retain(|_, p| !(p.agent == agent_id && p.origin == Origin::Agent(theirs)));
    inner.pending.len() != before
}

/// Answer a fanout, if nothing is left to wait for.
fn settle_fanout(inner: &mut Inner, agent_id: u64, theirs: RequestId) {
    let key = (agent_id, theirs.0);
    let Some(entry) = inner.fanout.get(&key) else {
        return;
    };
    if entry.outstanding > 0 {
        return;
    }
    // Removed before the reply goes out, and with it any coordinator still
    // holding a place in the merge: this is the only thing that makes the
    // answer single, and a fanout force-completed early still has siblings that
    // would otherwise arrive and be forwarded as a second reply.
    let merged = entry.merged(theirs);
    inner.fanout.remove(&key);
    forget_request(inner, agent_id, theirs);
    send_to(inner, agent_id, &merged);
}

/// Offer a claim to the next coordinator that can be offered one.
///
/// A coordinator whose session went away mid-walk cannot be asked, but the ones
/// behind it still can. Taking one off the list and giving up if it happened to
/// be the sessionless one threw away every coordinator after it unexamined —
/// with three configured and the middle one reconnecting, the third was never
/// asked for a board it had.
fn next_claim_target(inner: &mut Inner, agent_id: u64, theirs: RequestId) -> Option<ClaimRetry> {
    let key = (agent_id, theirs.0);
    loop {
        let attempt = inner.claims.get_mut(&key)?;
        let next = attempt.remaining.pop()?;
        let claim = attempt.claim.clone();
        let session = inner
            .agents
            .get(&agent_id)
            .and_then(|a| a.sessions.get(&next).cloned());
        let Some(session) = session else { continue };
        inner.next_request += 1;
        let request = RequestId(inner.next_request);
        inner.pending.insert(
            request.0,
            Pending {
                agent: agent_id,
                origin: Origin::Agent(theirs),
                to: next,
                opening_session: false,
                auto_lease: None,
            },
        );
        return Some(ClaimRetry {
            to: next,
            request,
            session,
            claim,
        });
    }
}

/// Answer a claim that has nowhere left to go, with the most actionable reason
/// seen on the way.
fn finish_claim(inner: &mut Inner, agent_id: u64, theirs: RequestId) {
    let key = (agent_id, theirs.0);
    let Some(attempt) = inner.claims.remove(&key) else {
        return;
    };
    forget_request(inner, agent_id, theirs);
    let (error, retryable) = attempt
        .best_error
        .unwrap_or_else(|| ("no coordinator could satisfy this".into(), false));
    send_to(
        inner,
        agent_id,
        &ToClient::Error {
            request: theirs,
            error,
            retryable,
        },
    );
}

fn with_request(msg: ToClient, request: RequestId) -> ToClient {
    use ToClient::*;
    match msg {
        SessionOpened { session, id, .. } => SessionOpened {
            request,
            session,
            id,
        },
        Ok { .. } => Ok { request },
        Error {
            error, retryable, ..
        } => Error {
            request,
            error,
            retryable,
        },
        Granted {
            lease,
            slots,
            docs,
            expires_at,
            note,
            ..
        } => Granted {
            request,
            lease,
            slots,
            docs,
            expires_at,
            note,
        },
        Renewed { expires_at, .. } => Renewed {
            request,
            expires_at,
        },
        Status { leases, .. } => Status { request, leases },
        Tags { tags, .. } => Tags { request, tags },
        Inventory { coordinators, .. } => Inventory {
            request,
            coordinators,
        },
        other => other,
    }
}

pub async fn serve(shared: Arc<Shared>, path: &str) -> Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path)?;
    // The shim is unprivileged and may run as any user; the socket is the
    // machine's local trust boundary, which §9 already assumes.
    let _ = std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o666));
    tracing::info!(%path, "agent socket ready");

    loop {
        let (socket, _) = listener.accept().await?;
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            if let Err(err) = serve_agent(shared, socket).await {
                tracing::debug!(?err, "agent disconnected");
            }
        });
    }
}

async fn serve_agent(shared: Arc<Shared>, mut socket: tokio::net::UnixStream) -> Result<()> {
    // Ask the kernel who is on the other end rather than trusting anything the
    // peer says: this decides who gets to open a device node.
    let uid = socket.peer_cred().map(|c| c.uid()).unwrap_or(0);
    benchd_core::protocol::accept(&mut socket).await?;
    let (read, write) = socket.into_split();
    let mut lines = FramedRead::new(read, LinesCodec::new());
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    tokio::spawn(async move {
        let mut sink = FramedWrite::new(write, LinesCodec::new());
        while let Some(line) = rx.recv().await {
            if sink.send(line).await.is_err() {
                break;
            }
        }
    });

    let agent_id = {
        let mut inner = shared.agents.inner.lock().await;
        inner.next_agent += 1;
        let id = inner.next_agent;
        inner.agents.insert(
            id,
            Agent {
                name: String::new(),
                sessions: BTreeMap::new(),
                internal: BTreeMap::new(),
                uid,
                out: tx.clone(),
            },
        );
        id
    };

    while let Some(line) = lines.next().await {
        let line = line?;
        let msg: ClientMsg = match serde_json::from_str(&line) {
            Ok(msg) => msg,
            Err(err) => {
                tracing::warn!(?err, "undecodable agent message");
                continue;
            }
        };
        forward(&shared, agent_id, msg).await;
    }

    // The MCP process exited, so its agent is gone. Closing the session on
    // every coordinator it had one with makes each release that agent's leases
    // immediately rather than waiting out a TTL — nobody is left to warn, so
    // gracing would only idle hardware.
    let sessions = shared.agents.forget_agent(agent_id).await;
    for (to, session) in sessions {
        let request = shared
            .agents
            .track(agent_id, Origin::Daemon, to, false)
            .await;
        shared
            .send(to, &ClientMsg::CloseSession { request, session })
            .await;
    }
    tracing::info!(agent_id, "agent disconnected");
    Ok(())
}

/// Ask every coordinator whose executor link is live for its operator view.
///
/// A fresh connection is intentional. The long-lived connection identifies as
/// a client on its first message and cannot then switch protocol roles to send
/// an [`OperatorMsg`]. Keeping the result grouped also matters: two independent
/// authorities may both have a bench or lease numbered the same.
async fn inspect_connected(shared: &Shared) -> Result<Vec<CoordinatorInventory>, String> {
    let live = shared.live_links().await;
    if live.is_empty() {
        return Err("no coordinator is reachable".into());
    }

    let coordinators: Vec<_> = shared
        .coordinators
        .iter()
        .filter(|coordinator| live.contains(&coordinator.id))
        .collect();
    join_all(coordinators.into_iter().map(inspect_coordinator))
        .await
        .into_iter()
        .collect()
}

async fn inspect_coordinator(
    coordinator: &crate::Coordinator,
) -> Result<CoordinatorInventory, String> {
    let mut socket = tokio::net::TcpStream::connect(&coordinator.address)
        .await
        .map_err(|err| {
            format!(
                "could not inspect coordinator {} at {}: {err}",
                coordinator.name, coordinator.address
            )
        })?;
    socket.set_nodelay(true).ok();
    benchd_core::protocol::connect(&mut socket)
        .await
        .map_err(|err| format!("coordinator {}: {err}", coordinator.name))?;
    let (read, write) = socket.into_split();
    let mut lines = FramedRead::new(read, LinesCodec::new());
    let mut sink = FramedWrite::new(write, LinesCodec::new());
    sink.send(
        serde_json::to_string(&OperatorMsg::Inspect)
            .map_err(|err| format!("could not encode inspection request: {err}"))?,
    )
    .await
    .map_err(|err| format!("could not ask coordinator {}: {err}", coordinator.name))?;

    let line = tokio::time::timeout(Duration::from_secs(10), lines.next())
        .await
        .map_err(|_| format!("coordinator {} did not reply within 10s", coordinator.name))?
        .ok_or_else(|| format!("coordinator {} closed the connection", coordinator.name))?
        .map_err(|err| format!("could not read coordinator {}: {err}", coordinator.name))?;
    let reply: ToOperator = serde_json::from_str(&line).map_err(|err| {
        format!(
            "could not understand coordinator {}'s reply: {err}",
            coordinator.name
        )
    })?;
    match reply {
        ToOperator::State { benches, leases } => Ok(CoordinatorInventory {
            name: coordinator.name.clone(),
            local: coordinator.name == "local",
            benches,
            leases,
        }),
        ToOperator::Error { error } => Err(format!("coordinator {}: {error}", coordinator.name)),
        other => Err(format!(
            "coordinator {} sent an unexpected reply: {other:?}",
            coordinator.name
        )),
    }
}

async fn forward(shared: &Arc<Shared>, agent_id: u64, msg: ClientMsg) {
    // Handled locally: no coordinator knows where this machine puts device
    // nodes, and the caller is a sandbox launcher rather than an agent.
    if let ClientMsg::PrepareOwner { request, name } = &msg {
        // PrepareOwner runs before the caller has a coordinator session id.
        // Reject a name that owner_dir would have to rewrite: materialisation
        // later uses the real session id as its fallback, and the two paths
        // would otherwise differ (for example s0 here versus s7 later).
        let prepared = if benchd_core::model::valid_component(name) {
            shared
                .materializer
                .lock()
                .await
                .prepare_owner(&owner_dir(SessionId(0), name))
                .await
        } else {
            Err("invalid sandbox identity".to_owned())
        };
        let reply = match prepared {
            Ok((dir, devices)) => ToClient::OwnerReady {
                request: *request,
                path: dir.display().to_string(),
                device_path: devices.display().to_string(),
            },
            Err(error) => ToClient::Error {
                request: *request,
                error,
                retryable: false,
            },
        };
        send_to_agent(shared, agent_id, &reply).await;
        return;
    }

    // Operator discovery is also local. The daemon is the only process that
    // knows every configured coordinator and which links are currently alive.
    if let ClientMsg::Inspect { request } = &msg {
        let reply = match inspect_connected(shared).await {
            Ok(coordinators) => ToClient::Inventory {
                request: *request,
                coordinators,
            },
            Err(error) => ToClient::Error {
                request: *request,
                error,
                retryable: true,
            },
        };
        send_to_agent(shared, agent_id, &reply).await;
        return;
    }

    // A lease-bearing request goes to the coordinator that granted it. Anything
    // else is asked of every coordinator and the answers merged.
    let theirs = request_of(&msg);
    let routed = !matches!(msg, ClientMsg::Heartbeat | ClientMsg::Done { .. });
    if routed && shared.agents.already_outstanding(agent_id, theirs).await {
        // An id is how a reply finds its question. Two live questions under one
        // id cannot both be answered, and the second used to be folded into the
        // first's fanout and never answered at all.
        reply_error(
            shared,
            agent_id,
            theirs,
            "a request with this id is already outstanding",
            false,
        )
        .await;
        return;
    }

    // Registration goes to every coordinator: the agent gets one session on
    // each, and never learns there is more than one.
    if let ClientMsg::OpenSession { request, name } = &msg {
        {
            let mut inner = shared.agents.inner.lock().await;
            if let Some(agent) = inner.agents.get_mut(&agent_id) {
                agent.name = name.clone();
            }
        }
        let live = shared.live_links().await;
        if live.is_empty() {
            // Retryable: the reconnect loops are still running, so this is a
            // wait, not a request the agent should rewrite.
            reply_error(
                shared,
                agent_id,
                *request,
                "no coordinator is reachable",
                true,
            )
            .await;
            return;
        }
        // One reply, once every coordinator has answered. An earlier version
        // answered on the first, which raced: a claim is only offered to
        // coordinators the agent already has a session with, so claiming the
        // moment registration "succeeded" quietly skipped every coordinator
        // slower than the fastest one. On a laptop with a local coordinator and
        // a lab across the network that is *always* the lab, so every claim for
        // remote hardware failed as unsatisfiable.
        //
        // This fences the first registration only. Reconnection reintroduced
        // the identical race with no guard at all — the link is published
        // before its `OpenSession` is answered — and what fences that is
        // `session_soon`, which waits for a handshake in flight instead of
        // reading it as a coordinator that is not there.
        shared
            .agents
            .expect_fanout(agent_id, *request, live.len(), FanoutKind::Session)
            .await;
        for to in live {
            let ours = shared
                .agents
                .track(agent_id, Origin::Agent(*request), to, true)
                .await;
            shared
                .send(
                    to,
                    &ClientMsg::OpenSession {
                        request: ours,
                        name: name.clone(),
                    },
                )
                .await;
        }
        arm_deadline(shared, agent_id, *request);
        return;
    }

    // Everything below needs a session on every coordinator it is going to
    // reach, and a request is the moment to notice one has gone missing.
    shared.agents.ensure_registered(shared, agent_id).await;

    match msg {
        ClientMsg::Claim { claim, .. } => {
            // Offered to coordinators one at a time, in configuration order:
            // broadcasting would let two of them satisfy it at once and hold
            // hardware the agent never asked for. When one says no, the reply
            // path moves on to the next (see `deliver_reply`).
            //
            // A coordinator that is connected but still handshaking is waited
            // for rather than skipped. Skipping it offered the claim to
            // whichever coordinators happened to be ready, and if none of those
            // could satisfy it the merged answer was `retryable: false` — the
            // shim renders that as "change the request rather than retrying",
            // so the agent was told to give up because of a coordinator it was
            // never told existed.
            let mut targets = Vec::new();
            for to in shared.live_links().await {
                if let Some(session) = shared
                    .agents
                    .session_soon(agent_id, to, HANDSHAKE_GRACE)
                    .await
                {
                    targets.push((to, session));
                }
            }
            if targets.is_empty() {
                reply_error(
                    shared,
                    agent_id,
                    theirs,
                    "no coordinator is reachable",
                    true,
                )
                .await;
                return;
            }
            let (first, session) = targets.remove(0);
            // Reversed so `pop` walks them in configuration order.
            targets.reverse();
            let rest = targets.into_iter().map(|(to, _)| to).collect();
            shared
                .agents
                .begin_claim(agent_id, theirs, claim.clone(), rest)
                .await;
            let ours = shared
                .agents
                .track(agent_id, Origin::Agent(theirs), first, false)
                .await;
            shared
                .send(
                    first,
                    &ClientMsg::Claim {
                        request: ours,
                        session,
                        claim,
                    },
                )
                .await;
            arm_deadline(shared, agent_id, theirs);
        }
        ClientMsg::Renew { lease, extra, .. } => {
            route_by_lease(
                shared,
                agent_id,
                theirs,
                lease,
                |request, session, lease| ClientMsg::Renew {
                    request,
                    session,
                    lease,
                    extra,
                },
            )
            .await;
        }
        ClientMsg::Release { lease, .. } => {
            route_by_lease(
                shared,
                agent_id,
                theirs,
                lease,
                |request, session, lease| ClientMsg::Release {
                    request,
                    session,
                    lease,
                },
            )
            .await;
        }
        ClientMsg::Status { .. } => {
            broadcast(
                shared,
                agent_id,
                theirs,
                FanoutKind::Leases,
                |request, session| ClientMsg::Status { request, session },
            )
            .await;
        }
        ClientMsg::TagList { .. } => {
            broadcast(shared, agent_id, theirs, FanoutKind::Tags, |request, _| {
                ClientMsg::TagList { request }
            })
            .await;
        }
        ClientMsg::CloseSession { .. } => {
            broadcast(
                shared,
                agent_id,
                theirs,
                FanoutKind::Ack,
                |request, session| ClientMsg::CloseSession { request, session },
            )
            .await;
        }
        _ => {}
    }
}

/// Send a lease-bearing request to the coordinator that granted that lease.
///
/// The agent works with a single opaque lease number; the coordinator it
/// belongs to is encoded in it (see `LeaseKey::to_public`).
async fn route_by_lease(
    shared: &Arc<Shared>,
    agent_id: u64,
    theirs: RequestId,
    public: benchd_core::lease::LeaseId,
    build: impl Fn(RequestId, SessionToken, benchd_core::lease::LeaseId) -> ClientMsg,
) {
    let key = LeaseKey::from_public(public.0);
    let session = shared
        .agents
        .session_soon(agent_id, key.coordinator, HANDSHAKE_GRACE)
        .await;
    let Some(session) = session else {
        // Says nothing about coordinators. The old wording announced that there
        // were several and that this id belonged to one of them, which both
        // breaks the single-lab view and lets an agent enumerate them by
        // feeding ids in and reading the two different refusals apart.
        reply_error(
            shared,
            agent_id,
            theirs,
            &format!("no lease {} is held here", public.0),
            false,
        )
        .await;
        return;
    };
    let ours = shared
        .agents
        .track(agent_id, Origin::Agent(theirs), key.coordinator, false)
        .await;
    shared.agents.attach_lease(ours, key).await;
    shared
        .send(key.coordinator, &build(ours, session, key.lease))
        .await;
    arm_deadline(shared, agent_id, theirs);
}

/// Promise the agent an answer to `theirs`, whatever the coordinators do.
fn arm_deadline(shared: &Arc<Shared>, agent_id: u64, theirs: RequestId) {
    let shared = Arc::clone(shared);
    tokio::spawn(async move {
        tokio::time::sleep(REQUEST_DEADLINE).await;
        shared.agents.expire(agent_id, theirs).await;
    });
}

/// Ask every connected coordinator; `deliver_reply` merges the answers.
async fn broadcast(
    shared: &Arc<Shared>,
    agent_id: u64,
    theirs: RequestId,
    kind: FanoutKind,
    build: impl Fn(RequestId, SessionToken) -> ClientMsg,
) {
    let live = shared.live_links().await;
    if live.is_empty() {
        reply_error(
            shared,
            agent_id,
            theirs,
            "no coordinator is reachable",
            true,
        )
        .await;
        return;
    }

    // A coordinator mid-handshake is waited for, not passed over: a merged view
    // that quietly leaves one out is the thing this whole path exists to
    // prevent.
    let mut targets = Vec::new();
    let mut absent = 0;
    for to in live {
        match shared
            .agents
            .session_soon(agent_id, to, HANDSHAKE_GRACE)
            .await
        {
            Some(session) => targets.push((to, session)),
            None => absent += 1,
        }
    }
    if targets.is_empty() {
        reply_error(
            shared,
            agent_id,
            theirs,
            "not registered with any coordinator",
            true,
        )
        .await;
        return;
    }
    if absent > 0 {
        reply_error(
            shared,
            agent_id,
            theirs,
            "part of the lab is still coming back after a restart, so this \
             would be a partial picture of it rather than the whole one",
            true,
        )
        .await;
        return;
    }

    // Counted before any is sent, or a fast reply could complete the merge
    // while later coordinators are still being asked.
    shared
        .agents
        .expect_fanout(agent_id, theirs, targets.len(), kind)
        .await;
    for (to, session) in targets {
        let ours = shared
            .agents
            .track(agent_id, Origin::Agent(theirs), to, false)
            .await;
        shared.send(to, &build(ours, session)).await;
    }
    arm_deadline(shared, agent_id, theirs);
}

async fn send_to_agent(shared: &Arc<Shared>, agent_id: u64, msg: &ToClient) {
    let inner = shared.agents.inner.lock().await;
    if let Some(agent) = inner.agents.get(&agent_id) {
        if let Ok(line) = serde_json::to_string(msg) {
            let _ = agent.out.send(line);
        }
    }
}

fn request_of(msg: &ClientMsg) -> RequestId {
    match msg {
        ClientMsg::OpenSession { request, .. }
        | ClientMsg::CloseSession { request, .. }
        | ClientMsg::Claim { request, .. }
        | ClientMsg::Renew { request, .. }
        | ClientMsg::Release { request, .. }
        | ClientMsg::Status { request, .. }
        | ClientMsg::TagList { request }
        | ClientMsg::Inspect { request }
        | ClientMsg::PrepareOwner { request, .. }
        | ClientMsg::Done { request, .. } => *request,
        ClientMsg::Heartbeat => RequestId(0),
    }
}

/// `retryable` is the machine-readable unsatisfiable-versus-contended
/// distinction (D14), and both front ends turn it into advice: a false here
/// tells the agent to rewrite its request rather than repeat it, so anything
/// the daemon will repair by itself has to say true.
async fn reply_error(
    shared: &Arc<Shared>,
    agent_id: u64,
    request: RequestId,
    error: &str,
    retryable: bool,
) {
    let inner = shared.agents.inner.lock().await;
    send_to(
        &inner,
        agent_id,
        &ToClient::Error {
            request,
            error: error.into(),
            retryable,
        },
    );
}

/// Fold one coordinator's tag list into the merged view.
///
/// Counts add up: three free ESP32-S3s in the lab plus one of my own is four
/// available to the agent, which is the number it should act on.
fn merge_tags(
    into: &mut BTreeMap<String, benchd_core::wire::TagInfo>,
    from: &[benchd_core::wire::TagInfo],
) {
    for tag in from {
        let slot = into
            .entry(tag.tag.clone())
            .or_insert_with(|| benchd_core::wire::TagInfo {
                tag: tag.tag.clone(),
                description: tag.description.clone(),
                benches: 0,
                free: 0,
            });
        slot.benches += tag.benches;
        slot.free += tag.free;
    }
}

/// Keep the most actionable of several failures.
///
/// "Every bench is busy" (retryable) is worth more to the caller than "no such
/// bench" (permanent): the first is worth waiting for, the second never will
/// be. So a retryable answer from any coordinator wins over a permanent one,
/// and a request is only reported unsatisfiable when they all agree it is.
fn keep_best_error(best: &mut Option<(String, bool)>, error: &str, retryable: bool) {
    let better = match best {
        None => true,
        Some((_, was_retryable)) => retryable && !*was_retryable,
    };
    if better {
        *best = Some((error.to_string(), retryable));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use benchd_core::wire::TagInfo;

    /// An agent connected to `Agents`, with the receiving end of its socket.
    async fn connect(agents: &Agents) -> (u64, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut inner = agents.inner.lock().await;
        inner.next_agent += 1;
        let id = inner.next_agent;
        inner.agents.insert(
            id,
            Agent {
                name: "tester".into(),
                sessions: BTreeMap::new(),
                internal: BTreeMap::new(),
                uid: 1000,
                out: tx,
            },
        );
        (id, rx)
    }

    fn opened(request: RequestId, id: u64) -> ToClient {
        ToClient::SessionOpened {
            request,
            session: SessionToken(format!("token-{id}")),
            id: SessionId(id),
        }
    }

    fn refused(request: RequestId, error: &str) -> ToClient {
        ToClient::Error {
            request,
            error: error.into(),
            retryable: false,
        }
    }

    /// An agent request on its way to one coordinator.
    async fn sent(agents: &Agents, agent: u64, theirs: RequestId, to: CoordinatorId) -> RequestId {
        agents.track(agent, Origin::Agent(theirs), to, false).await
    }

    /// A registration on its way to one coordinator.
    async fn registering(
        agents: &Agents,
        agent: u64,
        theirs: RequestId,
        to: CoordinatorId,
    ) -> RequestId {
        agents.track(agent, Origin::Agent(theirs), to, true).await
    }

    /// An agent that already holds a session on one coordinator.
    async fn with_session(agents: &Agents, agent: u64, to: CoordinatorId, id: u64) {
        let mut inner = agents.inner.lock().await;
        let entry = inner.agents.get_mut(&agent).expect("connected");
        entry
            .sessions
            .insert(to, SessionToken(format!("token-{id}")));
        entry.internal.insert(to, SessionId(id));
    }

    fn a_claim() -> benchd_core::wire::ClaimSpec {
        benchd_core::wire::ClaimSpec {
            slots: [("dut".to_string(), vec!["soc=esp32s3".to_string()])]
                .into_iter()
                .collect(),
            ttl: 900,
            reason: "wifi reconnect regression".into(),
            distinct: true,
        }
    }

    fn reply(out: &mut mpsc::UnboundedReceiver<String>) -> ToClient {
        let line = out.try_recv().expect("an answer");
        serde_json::from_str(&line).expect("a protocol reply")
    }

    fn a_lease(id: u64) -> benchd_core::wire::LeaseStatus {
        benchd_core::wire::LeaseStatus {
            lease: benchd_core::lease::LeaseId(id),
            slots: [("dut".to_string(), "bench-7".to_string())]
                .into_iter()
                .collect(),
            expires_at: 1000,
            remaining: 600,
            state: "active".into(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn registration_is_answered_only_once_every_coordinator_has_a_session() {
        // The race this guards: a claim is only offered to coordinators the
        // agent already has a session with, so answering on the first reply
        // meant claims silently skipped every slower coordinator. With a local
        // coordinator and a lab across the network, the slower one is always
        // the lab — so every claim for remote hardware failed as unsatisfiable.
        let agents = Agents::default();
        let (agent, mut out) = connect(&agents).await;

        let lab = CoordinatorId(0);
        let local = CoordinatorId(1);
        let theirs = RequestId(1);
        agents
            .expect_fanout(agent, theirs, 2, FanoutKind::Session)
            .await;
        let for_lab = registering(&agents, agent, theirs, lab).await;
        let for_local = registering(&agents, agent, theirs, local).await;

        agents.deliver_reply(local, opened(for_local, 7)).await;
        assert!(
            out.try_recv().is_err(),
            "the agent must not be told it has a session while a coordinator is \
             still registering"
        );

        agents.deliver_reply(lab, opened(for_lab, 3)).await;
        let line = out
            .try_recv()
            .expect("one reply once everyone has answered");
        let reply: ToClient = serde_json::from_str(&line).unwrap();
        assert!(matches!(
            reply,
            ToClient::SessionOpened { request, .. } if request == theirs
        ));
        assert!(out.try_recv().is_err(), "exactly one reply, not one each");

        // Both sessions are recorded, which is what a claim looks for.
        assert!(agents.session_on(agent, lab).await.is_some());
        assert!(agents.session_on(agent, local).await.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_coordinator_refusing_still_leaves_a_usable_session() {
        // Registration always succeeds today (D19), but a coordinator that is
        // reachable and unhappy must not cost the agent the sessions it did
        // get: it would be left unable to claim anything anywhere.
        let agents = Agents::default();
        let (agent, mut out) = connect(&agents).await;

        let theirs = RequestId(1);
        agents
            .expect_fanout(agent, theirs, 2, FanoutKind::Session)
            .await;
        let good = registering(&agents, agent, theirs, CoordinatorId(0)).await;
        let bad = registering(&agents, agent, theirs, CoordinatorId(1)).await;

        agents
            .deliver_reply(CoordinatorId(0), opened(good, 1))
            .await;
        agents
            .deliver_reply(CoordinatorId(1), refused(bad, "no"))
            .await;

        let line = out.try_recv().expect("an answer");
        let reply: ToClient = serde_json::from_str(&line).unwrap();
        assert!(
            matches!(reply, ToClient::SessionOpened { .. }),
            "got {reply:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lab_nobody_could_be_asked_about_is_not_an_empty_lab() {
        // Both coordinators want a fresh session, so nothing is known about any
        // bench anywhere — and the answer used to be a successful, empty tag
        // list, which reads as a lab with no hardware in it.
        let agents = Agents::default();
        let (agent, mut out) = connect(&agents).await;

        let theirs = RequestId(2);
        agents
            .expect_fanout(agent, theirs, 2, FanoutKind::Tags)
            .await;
        let lab = sent(&agents, agent, theirs, CoordinatorId(0)).await;
        let local = sent(&agents, agent, theirs, CoordinatorId(1)).await;

        agents
            .deliver_reply(
                CoordinatorId(0),
                refused(lab, "unknown session; register again"),
            )
            .await;
        agents
            .deliver_reply(
                CoordinatorId(1),
                refused(local, "unknown session; register again"),
            )
            .await;

        match reply(&mut out) {
            ToClient::Error {
                request, retryable, ..
            } => {
                assert_eq!(request, theirs);
                assert!(retryable, "the daemon re-registers by itself");
            }
            other => panic!("a total failure must not be a successful answer: {other:?}"),
        }
        assert!(out.try_recv().is_err(), "exactly one answer");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lab_only_half_of_which_could_be_seen_is_not_the_whole_lab() {
        // The frequent case, and the more dangerous one: an agent holding a
        // board on the coordinator that failed is handed a lease list without
        // it in, which `benchd mcp` renders as the successful "no leases held".
        let agents = Agents::default();
        let (agent, mut out) = connect(&agents).await;

        let theirs = RequestId(3);
        agents
            .expect_fanout(agent, theirs, 2, FanoutKind::Leases)
            .await;
        let lab = sent(&agents, agent, theirs, CoordinatorId(0)).await;
        let local = sent(&agents, agent, theirs, CoordinatorId(1)).await;

        agents
            .deliver_reply(
                CoordinatorId(0),
                refused(lab, "unknown session; register again"),
            )
            .await;
        agents
            .deliver_reply(
                CoordinatorId(1),
                ToClient::Status {
                    request: local,
                    leases: vec![a_lease(1)],
                },
            )
            .await;

        match reply(&mut out) {
            ToClient::Error {
                request,
                error,
                retryable,
            } => {
                assert_eq!(request, theirs);
                assert!(retryable);
                assert!(error.contains("part of the lab"), "got {error}");
                assert!(
                    !error.contains('2'),
                    "the agent is not told how many authorities there are: {error}"
                );
            }
            other => panic!("a partial view must not look complete: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_link_drop_answers_the_claim_it_stranded() {
        // A claim is not a fanout, and `invalidate` only rescued fanouts — so
        // the claim was dropped on the floor: never answered, the remaining
        // coordinators never tried, the entry never removed. Through the shim
        // that is a 30s timeout blaming "the coordinator"; through
        // `benchd lease`, which has no timeout, it hangs forever.
        let agents = Agents::default();
        let (agent, mut out) = connect(&agents).await;
        let lab = CoordinatorId(0);
        let local = CoordinatorId(1);
        with_session(&agents, agent, lab, 1).await;
        with_session(&agents, agent, local, 2).await;

        let theirs = RequestId(4);
        agents
            .begin_claim(agent, theirs, a_claim(), vec![local])
            .await;
        let _ = sent(&agents, agent, theirs, lab).await;

        agents.invalidate(lab).await;

        match reply(&mut out) {
            ToClient::Error {
                request, retryable, ..
            } => {
                assert_eq!(request, theirs);
                assert!(retryable, "nothing was granted; asking again is the fix");
            }
            other => panic!("the claim must be answered: {other:?}"),
        }
        let inner = agents.inner.lock().await;
        assert!(inner.claims.is_empty(), "and not left behind");
        assert!(inner.pending.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_claim_keeps_asking_past_a_coordinator_that_has_no_session() {
        // Three coordinators, the middle one reconnecting, and the third has
        // the board. Taking the middle one off the list and giving up there
        // discarded the third unexamined.
        let agents = Agents::default();
        let (agent, mut out) = connect(&agents).await;
        let (first, middle, last) = (CoordinatorId(0), CoordinatorId(1), CoordinatorId(2));
        with_session(&agents, agent, first, 1).await;
        with_session(&agents, agent, last, 3).await;

        let theirs = RequestId(5);
        // Reversed, as `forward` leaves it, so `pop` walks in configuration
        // order: the middle one first, then the last.
        agents
            .begin_claim(agent, theirs, a_claim(), vec![last, middle])
            .await;
        let ours = sent(&agents, agent, theirs, first).await;

        let retry = agents
            .deliver_reply(
                first,
                refused(ours, "no bench exists matching {soc=esp32s3}"),
            )
            .await
            .expect("the coordinator that has one must still be asked");
        assert_eq!(retry.to, last);
        assert!(out.try_recv().is_err(), "and no failure reported yet");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn closing_a_session_is_answered_once_and_leaves_nothing_behind() {
        // `Ok` is what a coordinator answers a `CloseSession` broadcast with,
        // and it was not counted as a reply: each one was forwarded verbatim,
        // so one request got two answers, and the entry stayed in the map to
        // swallow the next error that happened to bear the same id.
        let agents = Agents::default();
        let (agent, mut out) = connect(&agents).await;
        let (lab, local) = (CoordinatorId(0), CoordinatorId(1));

        let theirs = RequestId(6);
        agents
            .expect_fanout(agent, theirs, 2, FanoutKind::Ack)
            .await;
        let to_lab = sent(&agents, agent, theirs, lab).await;
        let to_local = sent(&agents, agent, theirs, local).await;

        agents
            .deliver_reply(lab, ToClient::Ok { request: to_lab })
            .await;
        assert!(out.try_recv().is_err(), "not until both have answered");
        agents
            .deliver_reply(local, ToClient::Ok { request: to_local })
            .await;

        assert!(matches!(reply(&mut out), ToClient::Ok { request } if request == theirs));
        assert!(out.try_recv().is_err(), "one request, one answer");
        assert!(
            agents.inner.lock().await.fanout.is_empty(),
            "a dead fanout swallows the next error under this id"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_daemons_own_replies_are_not_an_agents_to_swallow() {
        // Zero is a legal request id for an agent to choose, and it was also
        // the daemon's marker for "mine", so an agent with a request
        // outstanding under it absorbed the re-registration reply.
        let agents = Agents::default();
        let (agent, mut out) = connect(&agents).await;
        let local = CoordinatorId(1);

        let theirs = RequestId(0);
        agents
            .expect_fanout(agent, theirs, 1, FanoutKind::Tags)
            .await;
        let _ = sent(&agents, agent, theirs, local).await;

        let mine = agents.track(agent, Origin::Daemon, local, true).await;
        agents.deliver_reply(local, opened(mine, 9)).await;

        assert!(
            agents.inner.lock().await.fanout.contains_key(&(agent, 0)),
            "the agent's request is still outstanding"
        );
        let line = out.try_recv().expect("an event about the reconnection");
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(event["msg"], "reconnected");
        assert!(
            event.get("coordinator").is_none(),
            "naming it would tell the agent how many there are"
        );
        assert!(
            agents.session_on(agent, local).await.is_some(),
            "the session it opened is still recorded"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_coordinator_that_simply_never_answers_is_answered_for() {
        // Nothing else here is driven by time, and a connected coordinator that
        // says nothing is not an event.
        let agents = Agents::default();
        let (agent, mut out) = connect(&agents).await;
        let (lab, local) = (CoordinatorId(0), CoordinatorId(1));

        let theirs = RequestId(7);
        agents
            .expect_fanout(agent, theirs, 2, FanoutKind::Tags)
            .await;
        let to_lab = sent(&agents, agent, theirs, lab).await;
        let quiet = sent(&agents, agent, theirs, local).await;

        agents
            .deliver_reply(
                lab,
                ToClient::Tags {
                    request: to_lab,
                    tags: vec![tag("soc=esp32s3", 3, 2)],
                },
            )
            .await;
        agents.expire(agent, theirs).await;

        match reply(&mut out) {
            ToClient::Error {
                request, retryable, ..
            } => {
                assert_eq!(request, theirs);
                assert!(retryable);
            }
            other => panic!("a half-answered list must not be served whole: {other:?}"),
        }

        // The straggler turning up afterwards must not answer a second time.
        agents
            .deliver_reply(
                local,
                ToClient::Tags {
                    request: quiet,
                    tags: vec![tag("soc=rp2040", 1, 1)],
                },
            )
            .await;
        assert!(out.try_recv().is_err(), "exactly one answer, ever");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_claim_that_goes_quiet_is_answered_too() {
        let agents = Agents::default();
        let (agent, mut out) = connect(&agents).await;
        let lab = CoordinatorId(0);
        with_session(&agents, agent, lab, 1).await;

        let theirs = RequestId(8);
        agents.begin_claim(agent, theirs, a_claim(), vec![]).await;
        let _ = sent(&agents, agent, theirs, lab).await;
        agents.expire(agent, theirs).await;

        assert!(matches!(
            reply(&mut out),
            ToClient::Error { request, .. } if request == theirs
        ));
        assert!(agents.inner.lock().await.claims.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_coordinator_mid_handshake_is_waited_for_rather_than_skipped() {
        // On a reconnect the link is live before the session exists. Treating
        // that as "not a target" is what makes a claim come back unsatisfiable
        // because of a coordinator the agent was never told about.
        let agents = Arc::new(Agents::default());
        let (agent, _out) = connect(&agents).await;
        let local = CoordinatorId(1);
        let ours = agents.track(agent, Origin::Daemon, local, true).await;

        let waiting = {
            let agents = Arc::clone(&agents);
            tokio::spawn(async move {
                agents
                    .session_soon(agent, local, Duration::from_secs(5))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        agents.deliver_reply(local, opened(ours, 4)).await;

        assert!(
            waiting.await.unwrap().is_some(),
            "the handshake landed inside the grace window"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_coordinator_with_no_handshake_in_flight_is_not_waited_for() {
        // The other half: one coordinator that never finishes registering must
        // not hold up every request on the machine.
        let agents = Agents::default();
        let (agent, _out) = connect(&agents).await;

        let started = tokio::time::Instant::now();
        let session = agents
            .session_soon(agent, CoordinatorId(1), Duration::from_secs(30))
            .await;
        assert!(session.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "there is nothing in flight to wait for"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_agent_that_leaves_takes_its_bookkeeping_with_it() {
        let agents = Agents::default();
        let (agent, _out) = connect(&agents).await;
        let lab = CoordinatorId(0);

        let ours = registering(&agents, agent, RequestId(1), lab).await;
        agents
            .expect_fanout(agent, RequestId(1), 1, FanoutKind::Session)
            .await;
        agents.deliver_reply(lab, opened(ours, 2)).await;
        agents
            .begin_claim(agent, RequestId(9), a_claim(), vec![])
            .await;
        let _ = sent(&agents, agent, RequestId(9), lab).await;
        {
            let mut inner = agents.inner.lock().await;
            inner
                .lease_owner
                .insert(LeaseKey::new(lab, benchd_core::lease::LeaseId(1)), agent);
        }

        let sessions = agents.forget_agent(agent).await;
        assert_eq!(sessions.len(), 1, "its session still needs closing");

        let inner = agents.inner.lock().await;
        assert!(inner.owners.is_empty(), "owners");
        assert!(inner.uids.is_empty(), "uids");
        assert!(inner.claims.is_empty(), "claims");
        assert!(inner.fanout.is_empty(), "fanout");
        assert!(inner.pending.is_empty(), "pending");
        assert!(inner.lease_owner.is_empty(), "lease_owner");
        assert!(inner.holds.is_empty(), "holds");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_id_already_in_use_is_refused_rather_than_merged() {
        let agents = Agents::default();
        let (agent, _out) = connect(&agents).await;

        let theirs = RequestId(11);
        assert!(!agents.already_outstanding(agent, theirs).await);
        let _ = sent(&agents, agent, theirs, CoordinatorId(0)).await;
        assert!(agents.already_outstanding(agent, theirs).await);
        assert!(
            !agents.already_outstanding(agent, RequestId(12)).await,
            "only the id that is in use"
        );
    }

    fn tag(name: &str, benches: usize, free: usize) -> TagInfo {
        TagInfo {
            tag: name.into(),
            description: String::new(),
            benches,
            free,
        }
    }

    #[test]
    fn tag_counts_add_up_across_coordinators() {
        let mut merged = BTreeMap::new();
        // The lab has three S3s, two free; I have one of my own, free.
        merge_tags(
            &mut merged,
            &[tag("soc=esp32s3", 3, 2), tag("psram=octal", 1, 1)],
        );
        merge_tags(&mut merged, &[tag("soc=esp32s3", 1, 1)]);

        let s3 = &merged["soc=esp32s3"];
        assert_eq!(
            (s3.benches, s3.free),
            (4, 3),
            "counts must sum, not overwrite"
        );
        // A tag only one coordinator knows about still appears.
        assert_eq!(merged["psram=octal"].benches, 1);
    }

    #[test]
    fn a_retryable_failure_beats_a_permanent_one() {
        // The lab has no such bench, but mine is merely busy: the agent should
        // be told to wait, not that the request is impossible (D14).
        let mut best = None;
        keep_best_error(&mut best, "no bench exists matching {psram=octal}", false);
        keep_best_error(&mut best, "all matching benches are held", true);
        assert!(best.as_ref().unwrap().1, "must stay retryable");
        assert!(best.as_ref().unwrap().0.contains("held"));

        // ...and the winner is not displaced by a later permanent failure.
        keep_best_error(&mut best, "no bench exists", false);
        assert!(best.unwrap().1);
    }

    #[test]
    fn unsatisfiable_only_when_every_coordinator_agrees() {
        let mut best = None;
        keep_best_error(&mut best, "no bench exists matching {soc=rp2040}", false);
        keep_best_error(&mut best, "no bench exists matching {soc=rp2040}", false);
        assert!(!best.unwrap().1, "nobody has it: not worth retrying");
    }
}
