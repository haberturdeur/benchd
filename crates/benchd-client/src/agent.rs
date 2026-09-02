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

use anyhow::Result;
use benchd_core::lease::{LeaseId, SessionId};
use benchd_core::wire::{ClientMsg, RequestId, SessionToken, ToClient};
use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, Mutex};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

use crate::materialize::owner_dir;
use crate::Shared;

/// One connected MCP shim.
struct Agent {
    name: String,
    session: Option<SessionToken>,
    internal: Option<SessionId>,
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

#[derive(Default)]
struct Inner {
    agents: BTreeMap<u64, Agent>,
    /// Our request id -> (agent, the id the agent used).
    pending: BTreeMap<u64, (u64, RequestId)>,
    /// Which agent holds a lease, so events reach the right one.
    lease_owner: BTreeMap<LeaseId, u64>,
    /// Internal session id -> directory component, for materialisation paths.
    owners: BTreeMap<SessionId, String>,
    /// Internal session id -> the uid that should own its device nodes.
    uids: BTreeMap<SessionId, u32>,
    next_agent: u64,
    next_request: u64,
}

impl Agents {
    pub async fn owner_for(&self, session: SessionId) -> String {
        let inner = self.inner.lock().await;
        inner.owners.get(&session).cloned().unwrap_or_else(|| session.to_string())
    }

    /// Which uid should be able to open this session's devices.
    pub async fn uid_for(&self, session: SessionId) -> Option<u32> {
        self.inner.lock().await.uids.get(&session).copied()
    }

    /// Rewrite an agent's request into our id space and remember the mapping.
    async fn track(&self, agent: u64, theirs: RequestId) -> RequestId {
        let mut inner = self.inner.lock().await;
        inner.next_request += 1;
        let ours = RequestId(inner.next_request);
        inner.pending.insert(ours.0, (agent, theirs));
        ours
    }

    /// Route a coordinator reply back to the agent that asked, restoring its
    /// own request id.
    pub async fn deliver_reply(&self, msg: ToClient) {
        let ours = match &msg {
            ToClient::SessionOpened { request, .. }
            | ToClient::Ok { request }
            | ToClient::Error { request, .. }
            | ToClient::Granted { request, .. }
            | ToClient::Renewed { request, .. }
            | ToClient::Status { request, .. }
            | ToClient::Tags { request, .. } => request.0,
            _ => return,
        };

        let mut inner = self.inner.lock().await;
        let Some((agent_id, theirs)) = inner.pending.remove(&ours) else {
            tracing::debug!(ours, "reply for an unknown request");
            return;
        };

        // Remember the session and lease ownership as they are handed out, so
        // later events and materialisations can be routed without asking.
        match &msg {
            ToClient::SessionOpened { session, id, .. } => {
                let name = if let Some(agent) = inner.agents.get_mut(&agent_id) {
                    agent.session = Some(session.clone());
                    agent.internal = Some(*id);
                    agent.name.clone()
                } else {
                    String::new()
                };
                // Remember where this session's device nodes will live, so a
                // later Materialize can be placed without another round trip.
                inner.owners.insert(*id, owner_dir(*id, &name));
                if let Some(uid) = inner.agents.get(&agent_id).map(|a| a.uid) {
                    inner.uids.insert(*id, uid);
                }
            }
            ToClient::Granted { lease, .. } => {
                inner.lease_owner.insert(*lease, agent_id);
            }
            _ => {}
        }

        let Some(agent) = inner.agents.get(&agent_id) else { return };
        let msg = with_request(msg, theirs);
        if let Ok(line) = serde_json::to_string(&msg) {
            let _ = agent.out.send(line);
        }
    }

    /// A claim is only useful once the device nodes exist, so the shim is told
    /// where they are as a separate message after materialisation succeeds.
    pub async fn deliver_paths(
        &self,
        lease: LeaseId,
        paths: BTreeMap<String, BTreeMap<String, String>>,
    ) {
        let inner = self.inner.lock().await;
        let Some(agent_id) = inner.lease_owner.get(&lease) else { return };
        let Some(agent) = inner.agents.get(agent_id) else { return };
        let msg = serde_json::json!({ "msg": "paths", "lease": lease, "slots": paths });
        let _ = agent.out.send(msg.to_string());
    }

    pub async fn notify_revoking(&self, lease: LeaseId, reason: &str, teardown_at: u64) {
        self.notify(lease, serde_json::json!({
            "msg": "revoking", "lease": lease, "reason": reason, "teardown_at": teardown_at
        }))
        .await;
    }

    /// A lease was withdrawn before it ever worked. Unlike `ended`, the agent
    /// is probably still blocked waiting for device paths, so this must reach it.
    pub async fn notify_failed(&self, lease: LeaseId, detail: &str) {
        // Deliberately does NOT forget the lease owner: the teardown for this
        // lease runs afterwards and still needs it to find the right directory.
        // It is cleaned up by notify_ended, or when the agent disconnects.
        self.notify(lease, serde_json::json!({
            "msg": "failed", "lease": lease, "detail": detail
        }))
        .await;
    }

    /// The internal session id that owns a lease, for path construction.
    pub async fn session_for_lease(&self, lease: LeaseId) -> SessionId {
        let inner = self.inner.lock().await;
        inner
            .lease_owner
            .get(&lease)
            .and_then(|agent| inner.agents.get(agent))
            .and_then(|a| a.internal)
            .unwrap_or(SessionId(0))
    }

    pub async fn notify_ended(&self, lease: LeaseId, reason: &str) {
        self.notify(lease, serde_json::json!({
            "msg": "ended", "lease": lease, "reason": reason
        }))
        .await;
        self.inner.lock().await.lease_owner.remove(&lease);
    }

    async fn notify(&self, lease: LeaseId, msg: serde_json::Value) {
        let inner = self.inner.lock().await;
        let Some(agent_id) = inner.lease_owner.get(&lease) else { return };
        let Some(agent) = inner.agents.get(agent_id) else { return };
        let _ = agent.out.send(msg.to_string());
    }

    /// The coordinator link dropped, so every session token we hold is void.
    ///
    /// The agents' unix sockets are still open — only the upstream link broke —
    /// so they are re-registered automatically when it comes back. An earlier
    /// version cleared the session and left the shim to notice, but a shim only
    /// registers once at startup and its socket never closed, so a one-second
    /// coordinator restart wedged every agent on the machine permanently.
    pub async fn invalidate_all(&self) {
        let mut inner = self.inner.lock().await;
        inner.pending.clear();
        inner.lease_owner.clear();
        for agent in inner.agents.values_mut() {
            agent.session = None;
            agent.internal = None;
            let msg = serde_json::json!({
                "msg": "disconnected",
                "detail": "the coordinator restarted; all leases are void. \
                           Your session is being re-registered; re-claim anything you need."
            });
            let _ = agent.out.send(msg.to_string());
        }
    }

    /// Re-register every connected agent after the link is restored.
    ///
    /// Names are remembered per agent precisely so this can happen without the
    /// shim doing anything.
    pub async fn reregister_all(&self, shared: &Shared) {
        let agents: Vec<(u64, String)> = {
            let inner = self.inner.lock().await;
            inner
                .agents
                .iter()
                .filter(|(_, a)| a.session.is_none() && !a.name.is_empty())
                .map(|(id, a)| (*id, a.name.clone()))
                .collect()
        };
        for (agent_id, name) in agents {
            let request = self.track(agent_id, RequestId(0)).await;
            tracing::info!(agent_id, %name, "re-registering after reconnect");
            shared.send(&ClientMsg::OpenSession { request, name }).await;
        }
    }
}

fn with_request(msg: ToClient, request: RequestId) -> ToClient {
    use ToClient::*;
    match msg {
        SessionOpened { session, id, .. } => SessionOpened { request, session, id },
        Ok { .. } => Ok { request },
        Error { error, retryable, .. } => Error { request, error, retryable },
        Granted { lease, slots, expires_at, note, .. } => {
            Granted { request, lease, slots, expires_at, note }
        }
        Renewed { expires_at, .. } => Renewed { request, expires_at },
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
            Agent { name: String::new(), session: None, internal: None, uid, out: tx.clone() },
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
    let session = {
        let mut inner = shared.agents.inner.lock().await;
        inner.agents.remove(&agent_id).and_then(|a| a.session)
    };
    if let Some(session) = session {
        let request = shared.agents.track(agent_id, RequestId(0)).await;
        shared.send(&ClientMsg::CloseSession { request, session }).await;
    }
    tracing::info!(agent_id, "agent disconnected");
    Ok(())
}

async fn forward(shared: &Arc<Shared>, agent_id: u64, msg: ClientMsg) {
    // Substitute the session token the daemon holds, so a shim never needs to
    // handle one and cannot present another agent's.
    let session = {
        let inner = shared.agents.inner.lock().await;
        inner.agents.get(&agent_id).and_then(|a| a.session.clone())
    };

    // Handled locally: the coordinator has no idea where this machine puts
    // device nodes, and the caller is a sandbox launcher rather than an agent.
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
        let inner = shared.agents.inner.lock().await;
        if let Some(agent) = inner.agents.get(&agent_id) {
            if let Ok(line) = serde_json::to_string(&reply) {
                let _ = agent.out.send(line);
            }
        }
        return;
    }

    let out = match msg {
        ClientMsg::OpenSession { request, name } => {
            {
                let mut inner = shared.agents.inner.lock().await;
                if let Some(agent) = inner.agents.get_mut(&agent_id) {
                    agent.name = name.clone();
                }
            }
            let ours = shared.agents.track(agent_id, request).await;
            ClientMsg::OpenSession { request: ours, name }
        }
        other => {
            let Some(session) = session else {
                reply_error(shared, agent_id, request_of(&other), "not registered yet").await;
                return;
            };
            let ours = shared.agents.track(agent_id, request_of(&other)).await;
            match other {
                ClientMsg::Claim { claim, .. } => {
                    ClientMsg::Claim { request: ours, session, claim }
                }
                ClientMsg::Renew { lease, extra, .. } => {
                    ClientMsg::Renew { request: ours, session, lease, extra }
                }
                ClientMsg::Release { lease, .. } => {
                    ClientMsg::Release { request: ours, session, lease }
                }
                ClientMsg::Status { .. } => ClientMsg::Status { request: ours, session },
                ClientMsg::TagList { .. } => ClientMsg::TagList { request: ours },
                ClientMsg::CloseSession { .. } => {
                    ClientMsg::CloseSession { request: ours, session }
                }
                _ => return,
            }
        }
    };

    shared.send(&out).await;
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
    let Some(agent) = inner.agents.get(&agent_id) else { return };
    let msg = ToClient::Error { request, error: error.into(), retryable: false };
    if let Ok(line) = serde_json::to_string(&msg) {
        let _ = agent.out.send(line);
    }
}
