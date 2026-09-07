//! The local agent socket: where per-agent MCP shims connect.
//!
//! Each shim owns one session. The daemon multiplexes them all onto its single
//! coordinator connection, correlating replies by request id and routing
//! unsolicited lease events to whichever agent holds the lease.
//!
//! Requests are forwarded almost verbatim — the daemon rewrites request ids into
//! its own space and substitutes the session token, but does not interpret
//! claims. Policy lives in the coordinator; this is plumbing.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::ids::{CoordinatorId, LeaseKey, SessionKey};
use anyhow::Result;
use benchd_core::lease::SessionId;
use benchd_core::wire::{ClientMsg, RequestId, SessionToken, ToClient};
use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, Mutex};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

use crate::materialize::owner_dir;
use crate::Shared;

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

/// A request the daemon asked of several coordinators at once.
#[derive(Default)]
struct Fanout {
    outstanding: usize,
    tags: BTreeMap<String, benchd_core::wire::TagInfo>,
    leases: Vec<benchd_core::wire::LeaseStatus>,
    /// The most useful failure so far. A request no coordinator can ever
    /// satisfy is only unsatisfiable if *every* one says so; if any says it is
    /// merely busy, the agent should wait rather than give up (D14).
    error: Option<(String, bool)>,
    answered: bool,
}

#[derive(Default)]
struct Inner {
    agents: BTreeMap<u64, Agent>,
    /// Our request id -> (agent, the id the agent used, which coordinator).
    pending: BTreeMap<u64, (u64, RequestId, CoordinatorId)>,
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
    /// Session -> directory component, for materialisation paths.
    owners: BTreeMap<SessionKey, String>,
    /// Session -> the uid that should own its device nodes.
    uids: BTreeMap<SessionKey, u32>,
    next_agent: u64,
    next_request: u64,
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
    async fn expect_fanout(&self, agent_id: u64, theirs: RequestId, n: usize) {
        let mut inner = self.inner.lock().await;
        let entry = inner
            .fanout
            .entry((agent_id, theirs.0))
            .or_insert_with(Fanout::default);
        entry.outstanding += n;
    }

    /// This agent's session token on one coordinator.
    pub async fn session_on(&self, agent_id: u64, to: CoordinatorId) -> Option<SessionToken> {
        let inner = self.inner.lock().await;
        inner.agents.get(&agent_id)?.sessions.get(&to).cloned()
    }

    pub async fn uid_for(&self, session: SessionKey) -> Option<u32> {
        self.inner.lock().await.uids.get(&session).copied()
    }

    /// Rewrite an agent's request into our id space and remember the mapping.
    async fn track(&self, agent: u64, theirs: RequestId, to: CoordinatorId) -> RequestId {
        let mut inner = self.inner.lock().await;
        inner.next_request += 1;
        let ours = RequestId(inner.next_request);
        inner.pending.insert(ours.0, (agent, theirs, to));
        ours
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
            | ToClient::Tags { request, .. } => request.0,
            _ => return None,
        };

        let mut inner = self.inner.lock().await;
        let Some((agent_id, theirs, _to)) = inner.pending.remove(&ours) else {
            tracing::debug!(ours, "reply for an unknown request");
            return None;
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
                    let attempt = inner.claims.get_mut(&claim_key).expect("checked");
                    keep_best_error(&mut attempt.best_error, error, *retryable);
                    if let Some(next) = attempt.remaining.pop() {
                        let claim = attempt.claim.clone();
                        let session = inner
                            .agents
                            .get(&agent_id)
                            .and_then(|a| a.sessions.get(&next).cloned());
                        if let Some(session) = session {
                            inner.next_request += 1;
                            let request = RequestId(inner.next_request);
                            inner.pending.insert(request.0, (agent_id, theirs, next));
                            return Some(ClaimRetry {
                                to: next,
                                request,
                                session,
                                claim,
                            });
                        }
                    }
                    // Nowhere left to ask: report the most actionable reason.
                    let attempt = inner.claims.remove(&claim_key).expect("checked");
                    let (error, retryable) = attempt
                        .best_error
                        .unwrap_or_else(|| ("no coordinator could satisfy this".into(), false));
                    if let Some(agent) = inner.agents.get(&agent_id) {
                        let reply = ToClient::Error {
                            request: theirs,
                            error,
                            retryable,
                        };
                        if let Ok(line) = serde_json::to_string(&reply) {
                            let _ = agent.out.send(line);
                        }
                    }
                    return None;
                }
                _ => {}
            }
        }

        // Remember the session and lease ownership as they are handed out, so
        // later events and materialisations can be routed without asking.
        match &msg {
            ToClient::SessionOpened { session, id, .. } => {
                let name = if let Some(agent) = inner.agents.get_mut(&agent_id) {
                    agent.sessions.insert(from, session.clone());
                    agent.internal.insert(from, *id);
                    agent.name.clone()
                } else {
                    String::new()
                };
                // Remember where this session's device nodes will live, so a
                // later Materialize can be placed without another round trip.
                let key = SessionKey::new(from, *id);
                inner.owners.insert(key, owner_dir(*id, &name));
                if let Some(uid) = inner.agents.get(&agent_id).map(|a| a.uid) {
                    inner.uids.insert(key, uid);
                }
            }
            ToClient::Granted { lease, .. } => {
                inner
                    .lease_owner
                    .insert(LeaseKey::new(from, *lease), agent_id);
            }
            _ => {}
        }

        // Fanned-out requests are merged rather than raced: `tag_list` and
        // `lease_status` must describe every coordinator, and a request that
        // failed everywhere should report the most actionable reason.
        let fanout_key = (agent_id, theirs.0);
        let is_fanout = matches!(msg, ToClient::Tags { .. } | ToClient::Status { .. })
            || (matches!(msg, ToClient::Error { .. }) && inner.fanout.contains_key(&fanout_key));

        if is_fanout {
            let entry = inner
                .fanout
                .entry(fanout_key)
                .or_insert_with(Fanout::default);
            entry.outstanding = entry.outstanding.saturating_sub(1);
            match &msg {
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
                    keep_best_error(&mut entry.error, error, *retryable);
                }
                _ => {}
            }

            if entry.outstanding > 0 || entry.answered {
                return None;
            }
            entry.answered = true;
            let merged = match &msg {
                ToClient::Tags { .. } => ToClient::Tags {
                    request: theirs,
                    tags: entry.tags.values().cloned().collect(),
                },
                ToClient::Status { .. } => ToClient::Status {
                    request: theirs,
                    leases: entry.leases.clone(),
                },
                _ => match &entry.error {
                    Some((error, retryable)) => ToClient::Error {
                        request: theirs,
                        error: error.clone(),
                        retryable: *retryable,
                    },
                    None => ToClient::Ok { request: theirs },
                },
            };
            inner.fanout.remove(&fanout_key);
            if let Some(agent) = inner.agents.get(&agent_id) {
                if let Ok(line) = serde_json::to_string(&merged) {
                    let _ = agent.out.send(line);
                }
            }
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
                expires_at,
                note,
            } => ToClient::Granted {
                request,
                lease: benchd_core::lease::LeaseId(LeaseKey::new(from, lease).to_public()),
                slots,
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
        self.inner.lock().await.lease_owner.remove(&lease);
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
    /// Forget everything belonging to ONE coordinator.
    ///
    /// Per coordinator, not global: a lab server restarting must not invalidate
    /// sessions held against the local coordinator that owns this machine's own
    /// boards. They are independent authorities.
    pub async fn invalidate(&self, coordinator: CoordinatorId) {
        let mut inner = self.inner.lock().await;
        inner.pending.retain(|_, (_, _, to)| *to != coordinator);
        inner
            .lease_owner
            .retain(|k, _| k.coordinator != coordinator);
        inner.owners.retain(|k, _| k.coordinator != coordinator);
        inner.uids.retain(|k, _| k.coordinator != coordinator);
        for agent in inner.agents.values_mut() {
            if agent.sessions.remove(&coordinator).is_some() {
                agent.internal.remove(&coordinator);
                let msg = serde_json::json!({
                    "msg": "disconnected",
                    "coordinator": coordinator.to_string(),
                    "detail": "a coordinator restarted; its leases are void. \
                               Your session there is being re-registered."
                });
                let _ = agent.out.send(msg.to_string());
            }
        }
    }

    /// Re-register every connected agent after the link is restored.
    ///
    /// Names are remembered per agent precisely so this can happen without the
    /// shim doing anything.
    pub async fn reregister_all(&self, shared: &Shared, to: CoordinatorId) {
        let agents: Vec<(u64, String)> = {
            let inner = self.inner.lock().await;
            inner
                .agents
                .iter()
                .filter(|(_, a)| !a.sessions.contains_key(&to) && !a.name.is_empty())
                .map(|(id, a)| (*id, a.name.clone()))
                .collect()
        };
        for (agent_id, name) in agents {
            let request = self.track(agent_id, RequestId(0), to).await;
            tracing::info!(agent_id, %name, coordinator = %to, "re-registering");
            shared
                .send(to, &ClientMsg::OpenSession { request, name })
                .await;
        }
    }
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
            expires_at,
            note,
            ..
        } => Granted {
            request,
            lease,
            slots,
            expires_at,
            note,
        },
        Renewed { expires_at, .. } => Renewed {
            request,
            expires_at,
        },
        Status { leases, .. } => Status { request, leases },
        Tags { tags, .. } => Tags { request, tags },
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

async fn serve_agent(shared: Arc<Shared>, socket: tokio::net::UnixStream) -> Result<()> {
    // Ask the kernel who is on the other end rather than trusting anything the
    // peer says: this decides who gets to open a device node.
    let uid = socket.peer_cred().map(|c| c.uid()).unwrap_or(0);
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

    // The MCP process exited, so its agent is gone. Closing the session makes
    // the coordinator release its leases immediately rather than waiting out
    // their TTLs — nobody is left to warn, so gracing would only idle hardware.
    // Close the agent's session on every coordinator it had one with, so each
    // releases that agent's leases immediately rather than waiting out a TTL.
    let sessions = {
        let mut inner = shared.agents.inner.lock().await;
        inner
            .agents
            .remove(&agent_id)
            .map(|a| a.sessions)
            .unwrap_or_default()
    };
    for (to, session) in sessions {
        let request = shared.agents.track(agent_id, RequestId(0), to).await;
        shared
            .send(to, &ClientMsg::CloseSession { request, session })
            .await;
    }
    tracing::info!(agent_id, "agent disconnected");
    Ok(())
}

async fn forward(shared: &Arc<Shared>, agent_id: u64, msg: ClientMsg) {
    // Handled locally: no coordinator knows where this machine puts device
    // nodes, and the caller is a sandbox launcher rather than an agent.
    if let ClientMsg::PrepareOwner { request, name } = &msg {
        let dir = shared.root.join(owner_dir(SessionId(0), name));
        let reply = match std::fs::create_dir_all(&dir).and_then(|_| {
            // Root-owned and not writable by the agent: the whole point is that
            // the agent cannot plant symlinks where root will later create
            // lease directories.
            std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        }) {
            Ok(()) => ToClient::OwnerReady {
                request: *request,
                path: dir.display().to_string(),
            },
            Err(err) => ToClient::Error {
                request: *request,
                error: format!("could not prepare {}: {err}", dir.display()),
                retryable: false,
            },
        };
        send_to_agent(shared, agent_id, &reply).await;
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
            reply_error(shared, agent_id, *request, "no coordinator is reachable").await;
            return;
        }
        // The agent is told it has a session as soon as the first one answers;
        // the rest arrive behind it. `deliver_reply` only forwards the first.
        for to in live {
            let ours = shared.agents.track(agent_id, *request, to).await;
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
        return;
    }

    // A lease-bearing request goes to the coordinator that granted it. Anything
    // else is asked of every coordinator and the answers merged.
    let theirs = request_of(&msg);
    match msg {
        ClientMsg::Claim { claim, .. } => {
            // Offered to coordinators one at a time, in configuration order:
            // broadcasting would let two of them satisfy it at once and hold
            // hardware the agent never asked for. When one says no, the reply
            // path moves on to the next (see `deliver_reply`).
            let mut targets = Vec::new();
            for to in shared.live_links().await {
                if shared.agents.session_on(agent_id, to).await.is_some() {
                    targets.push(to);
                }
            }
            if targets.is_empty() {
                reply_error(shared, agent_id, theirs, "no coordinator is reachable").await;
                return;
            }
            let first = targets.remove(0);
            // Reversed so `pop` walks them in configuration order.
            targets.reverse();
            shared
                .agents
                .begin_claim(agent_id, theirs, claim.clone(), targets)
                .await;
            let session = shared
                .agents
                .session_on(agent_id, first)
                .await
                .expect("checked");
            let ours = shared.agents.track(agent_id, theirs, first).await;
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
            broadcast(shared, agent_id, theirs, |request, session| {
                ClientMsg::Status { request, session }
            })
            .await;
        }
        ClientMsg::TagList { .. } => {
            broadcast(shared, agent_id, theirs, |request, _| ClientMsg::TagList {
                request,
            })
            .await;
        }
        ClientMsg::CloseSession { .. } => {
            broadcast(shared, agent_id, theirs, |request, session| {
                ClientMsg::CloseSession { request, session }
            })
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
    let Some(session) = shared.agents.session_on(agent_id, key.coordinator).await else {
        reply_error(
            shared,
            agent_id,
            theirs,
            "that lease belongs to a coordinator this machine is not connected to",
        )
        .await;
        return;
    };
    let ours = shared.agents.track(agent_id, theirs, key.coordinator).await;
    shared
        .send(key.coordinator, &build(ours, session, key.lease))
        .await;
}

/// Ask every connected coordinator; `deliver_reply` merges the answers.
async fn broadcast(
    shared: &Arc<Shared>,
    agent_id: u64,
    theirs: RequestId,
    build: impl Fn(RequestId, SessionToken) -> ClientMsg,
) {
    let live = shared.live_links().await;
    if live.is_empty() {
        reply_error(shared, agent_id, theirs, "no coordinator is reachable").await;
        return;
    }

    let mut targets = Vec::new();
    for to in live {
        if let Some(session) = shared.agents.session_on(agent_id, to).await {
            targets.push((to, session));
        }
    }
    if targets.is_empty() {
        reply_error(
            shared,
            agent_id,
            theirs,
            "not registered with any coordinator",
        )
        .await;
        return;
    }

    // Counted before any is sent, or a fast reply could complete the merge
    // while later coordinators are still being asked.
    shared
        .agents
        .expect_fanout(agent_id, theirs, targets.len())
        .await;
    for (to, session) in targets {
        let ours = shared.agents.track(agent_id, theirs, to).await;
        shared.send(to, &build(ours, session)).await;
    }
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
        | ClientMsg::PrepareOwner { request, .. }
        | ClientMsg::Done { request, .. } => *request,
        ClientMsg::Heartbeat => RequestId(0),
    }
}

async fn reply_error(shared: &Arc<Shared>, agent_id: u64, request: RequestId, error: &str) {
    let inner = shared.agents.inner.lock().await;
    let Some(agent) = inner.agents.get(&agent_id) else {
        return;
    };
    let msg = ToClient::Error {
        request,
        error: error.into(),
        retryable: false,
    };
    if let Ok(line) = serde_json::to_string(&msg) {
        let _ = agent.out.send(line);
    }
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
