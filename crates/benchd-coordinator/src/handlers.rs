//! Per-connection request handling.
//!
//! A connection announces itself with its first message: a `register` makes it
//! a host, anything else makes it a client. That avoids a handshake round trip
//! and keeps the protocol readable — the first line says what you are.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use benchd_core::lease::{ClaimError, LeaseError, SessionId};
use benchd_core::model::{ClaimRequest, Distinct, Requirement};
use benchd_core::wire::{
    BenchView, ChannelHello, ClaimSpec, ClientMsg, HostMsg, LeaseStatus, LeaseView, OperatorMsg, RequestId,
    TagInfo, ToClient, ToHost, ToOperator,
};
use futures::StreamExt;
use tokio::net::TcpStream;
use tokio_util::codec::{FramedRead, LinesCodec};

use crate::conn::{spawn_writer, Outbox};
use crate::state::{ClientConn, HostConn};
use crate::{now, Shared};

pub async fn serve(shared: Arc<Shared>, socket: TcpStream, peer: SocketAddr) -> Result<()> {
    socket.set_nodelay(true).ok();
    let (read, write) = socket.into_split();
    // The writer half is kept separate until we know what this connection is: a
    // data channel needs the raw socket back, not a line-framed sink.
    let mut lines = FramedRead::new(read, LinesCodec::new());

    let Some(first) = lines.next().await else {
        return Ok(());
    };
    let first = first?;

    if let Ok(hello) = serde_json::from_str::<ChannelHello>(&first) {
        let stream = lines
            .into_inner()
            .reunite(write)
            .map_err(|e| anyhow::anyhow!("could not reunite the socket: {e}"))?;
        Arc::clone(&shared.relay).join(hello, stream).await;
        return Ok(());
    }

    let out = spawn_writer(write, peer.to_string());

    if let Ok(HostMsg::Register { bench }) = serde_json::from_str::<HostMsg>(&first) {
        return serve_host(shared, lines, out, peer, bench).await;
    }
    if let Ok(msg) = serde_json::from_str::<OperatorMsg>(&first) {
        return serve_operator(shared, out, msg).await;
    }

    // Not a host and not an operator. It must be a client — but if the first
    // line does not decode as one either, say so and hang up. Falling through
    // silently leaves the caller waiting for a reply that will never come,
    // which is how a version mismatch turns into a mysterious hang instead of
    // an error message.
    if let Err(err) = serde_json::from_str::<ClientMsg>(&first) {
        tracing::warn!(%peer, %err, "unrecognised first message; closing");
        out.send(&ToClient::Error {
            request: RequestId(0),
            error: format!(
                "unrecognised message: {err}. Are the binaries the same build?"
            ),
            retryable: false,
        });
        return Ok(());
    }

    // Replay the first line through the client path so nothing is lost.
    serve_client(shared, lines, out, peer, Some(first)).await
}

// ---------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------

async fn serve_host(
    shared: Arc<Shared>,
    mut lines: FramedRead<tokio::net::tcp::OwnedReadHalf, LinesCodec>,
    out: Outbox,
    peer: SocketAddr,
    spec: benchd_core::wire::BenchSpec,
) -> Result<()> {
    let bench_id = spec.id.clone();

    {
        let mut state = shared.state.lock().await;
        match state.register_bench(&spec) {
            Ok(bench) => {
                state.leases.inventory_mut().benches.insert(bench_id.clone(), bench);
                state.hosts.insert(
                    bench_id.clone(),
                    HostConn { bench: bench_id.clone(), out: out.clone(), peer_ip: peer.ip() },
                );
                out.send(&ToHost::Registered);
                tracing::info!(bench = %bench_id, %peer, "host registered");
            }
            Err(reason) => {
                // A refused registration is terminal: retrying will not help,
                // and a bench that matches nothing is worse than no bench.
                tracing::warn!(bench = %bench_id, %reason, "registration refused");
                out.send(&ToHost::Rejected { reason });
                return Ok(());
            }
        }
    }

    while let Some(line) = lines.next().await {
        let line = line?;
        let msg: HostMsg = match serde_json::from_str(&line) {
            Ok(msg) => msg,
            Err(err) => {
                tracing::warn!(bench = %bench_id, ?err, "undecodable host message");
                continue;
            }
        };
        match msg {
            HostMsg::Heartbeat | HostMsg::Register { .. } => {}
            HostMsg::Done { request, result } => {
                tracing::debug!(bench = %bench_id, ?request, ?result, "host completed");
            }
            HostMsg::DeviceLost { resource, detail } => {
                // Not repaired in place: the bench leaves the inventory and its
                // leases are released, so the matcher routes around it.
                tracing::warn!(bench = %bench_id, %resource, %detail, "device lost");
                let outgoing = {
                    let mut state = shared.state.lock().await;
                    drop_bench(&mut state, &bench_id)
                };
                for msg in outgoing {
                    msg.send();
                }
            }
        }
    }

    tracing::info!(bench = %bench_id, "host disconnected");
    let outgoing = {
        let mut state = shared.state.lock().await;
        state.hosts.remove(&bench_id);
        drop_bench(&mut state, &bench_id)
    };
    for msg in outgoing {
        msg.send();
    }
    Ok(())
}

/// Remove a bench and release everything holding it.
fn drop_bench(state: &mut crate::state::State, bench: &str) -> Vec<crate::state::Outgoing> {
    state.leases.inventory_mut().benches.remove(bench);
    let doomed: Vec<_> = state
        .leases
        .leases()
        .filter(|l| l.benches().any(|b| b == bench))
        .map(|l| l.id)
        .collect();
    let mut effects = Vec::new();
    for lease in doomed {
        effects.extend(state.leases.drop_lease(lease));
    }
    state.dispatch(effects)
}

// ---------------------------------------------------------------------------
// Operator
// ---------------------------------------------------------------------------

async fn serve_operator(shared: Arc<Shared>, out: Outbox, msg: OperatorMsg) -> Result<()> {
    let mut state = shared.state.lock().await;
    let t = now();

    let reply = match msg {
        OperatorMsg::Inspect => {
            let benches = state
                .leases
                .inventory()
                .benches
                .values()
                .map(|b| BenchView {
                    id: b.id.clone(),
                    description: b.description.clone(),
                    tags: b
                        .tags
                        .iter()
                        .filter(|t| t.key != "name")
                        .map(|t| t.to_string())
                        .collect(),
                    resources: b.resource_names().iter().map(|s| s.to_string()).collect(),
                })
                .collect();
            let leases = state.leases.leases().map(|l| lease_view(&state, l, t)).collect();
            ToOperator::State { benches, leases }
        }
        OperatorMsg::ForceRelease { bench, immediate } => {
            if !state.leases.inventory().benches.contains_key(&bench) {
                out.send(&ToOperator::Error {
                    error: format!("no such bench {bench:?}"),
                });
                return Ok(());
            }
            let doomed: Vec<_> = state
                .leases
                .leases()
                .filter(|l| l.benches().any(|b| b == &bench))
                .map(|l| l.id)
                .collect();
            let count = doomed.len();
            let mut effects = Vec::new();
            for lease in doomed {
                // Graced by default: the holder had no warning and may be
                // mid-flash. `--now` is for when you know it is safe.
                effects.extend(if immediate {
                    state.leases.drop_lease(lease)
                } else {
                    state.leases.force_release(lease, t)
                });
            }
            let outgoing = state.dispatch(effects);
            drop(state);
            for msg in outgoing {
                msg.send();
            }
            out.send(&ToOperator::Released { count });
            return Ok(());
        }
    };

    out.send(&reply);
    Ok(())
}

fn lease_view(
    state: &crate::state::State,
    lease: &benchd_core::lease::Lease,
    _now: u64,
) -> LeaseView {
    LeaseView {
        id: lease.id.0,
        owner: state
            .leases
            .session(lease.session)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| lease.session.to_string()),
        slots: lease.slots.clone(),
        expires_at: lease.expires_at,
        state: match lease.state {
            benchd_core::lease::LeaseState::Held => "held".into(),
            benchd_core::lease::LeaseState::Revoking { .. } => "revoking".into(),
        },
        reason: lease.reason.clone(),
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

async fn serve_client(
    shared: Arc<Shared>,
    mut lines: FramedRead<tokio::net::tcp::OwnedReadHalf, LinesCodec>,
    out: Outbox,
    peer: SocketAddr,
    replay: Option<String>,
) -> Result<()> {
    let conn_id = {
        let mut state = shared.state.lock().await;
        let id = state.next_conn();
        state.clients.insert(id, ClientConn { out: out.clone(), peer_ip: peer.ip() });
        id
    };
    tracing::info!(%peer, conn_id, "client connected");

    let mut pending = replay;
    loop {
        let line = match pending.take() {
            Some(line) => line,
            None => match lines.next().await {
                Some(line) => line?,
                None => break,
            },
        };

        let msg: ClientMsg = match serde_json::from_str(&line) {
            Ok(msg) => msg,
            Err(err) => {
                tracing::warn!(conn_id, ?err, "undecodable client message");
                continue;
            }
        };

        let outgoing = handle_client(&shared, conn_id, &out, msg).await;
        for msg in outgoing {
            msg.send();
        }
    }

    // The client daemon is gone, so every agent behind it is gone too. Release
    // immediately: there is nobody left to warn, and gracing would only idle
    // the hardware.
    tracing::info!(conn_id, "client disconnected");
    let outgoing = {
        let mut state = shared.state.lock().await;
        state.clients.remove(&conn_id);
        let sessions: Vec<SessionId> = state
            .session_conn
            .iter()
            .filter(|(_, c)| **c == conn_id)
            .map(|(s, _)| *s)
            .collect();
        let mut effects = Vec::new();
        for session in sessions {
            state.session_conn.remove(&session);
            state.tokens.retain(|_, id| *id != session);
            effects.extend(state.leases.end_session(session, now()));
        }
        state.dispatch(effects)
    };
    for msg in outgoing {
        msg.send();
    }
    Ok(())
}

async fn handle_client(
    shared: &Arc<Shared>,
    conn_id: u64,
    out: &Outbox,
    msg: ClientMsg,
) -> Vec<crate::state::Outgoing> {
    let mut state = shared.state.lock().await;

    match msg {
        ClientMsg::Heartbeat => Vec::new(),
        ClientMsg::Done { request, result } => {
            tracing::debug!(?request, ?result, "client completed");
            Vec::new()
        }

        ClientMsg::OpenSession { request, name } => {
            let id = state.leases.register(&name);
            let token = state.mint_token(id);
            state.session_conn.insert(id, conn_id);
            tracing::info!(%name, session = %id, "session opened");
            out.send(&ToClient::SessionOpened { request, session: token, id });
            Vec::new()
        }

        ClientMsg::CloseSession { request, session } => {
            let Some(id) = state.resolve(&session) else {
                out.send(&unknown_session(request));
                return Vec::new();
            };
            state.session_conn.remove(&id);
            state.tokens.remove(&session);
            let effects = state.leases.end_session(id, now());
            out.send(&ToClient::Ok { request });
            state.dispatch(effects)
        }

        ClientMsg::Claim { request, session, claim } => {
            let Some(id) = state.resolve(&session) else {
                out.send(&unknown_session(request));
                return Vec::new();
            };
            let req = match to_claim_request(&claim, &state.leases.inventory().vocabulary) {
                Ok(req) => req,
                Err(error) => {
                    // A malformed tag is the agent's mistake and is fixable in
                    // one turn, so it is reported as not-retryable with the
                    // vocabulary's own did-you-mean text.
                    out.send(&ToClient::Error { request, error, retryable: false });
                    return Vec::new();
                }
            };
            match state.leases.claim(id, &req, now()) {
                Ok(granted) => {
                    // Granted first, then the Materialize that follows from
                    // dispatch: the client correlates them by lease id and
                    // answers the agent once the nodes actually exist.
                    out.send(&ToClient::Granted {
                        request,
                        lease: granted.lease,
                        slots: granted.assignment.clone(),
                        expires_at: granted.expires_at,
                        note: granted.ttl.note(),
                    });
                    state.dispatch(granted.effects)
                }
                Err(err) => {
                    let retryable = match &err {
                        // Contended: wait. Unsatisfiable: never retry as-is.
                        ClaimError::NoMatch(no) => !no.unsatisfiable(),
                        ClaimError::Limit(_) => false,
                        ClaimError::UnknownSession => false,
                    };
                    out.send(&ToClient::Error {
                        request,
                        error: err.to_string(),
                        retryable,
                    });
                    Vec::new()
                }
            }
        }

        ClientMsg::Renew { request, session, lease, extra } => {
            let Some(id) = state.resolve(&session) else {
                out.send(&unknown_session(request));
                return Vec::new();
            };
            match state.leases.renew(id, lease, extra, now()) {
                Ok(renewed) => {
                    out.send(&ToClient::Renewed { request, expires_at: renewed.expires_at });
                }
                Err(err) => out.send(&ToClient::Error {
                    request,
                    error: err.to_string(),
                    retryable: matches!(err, LeaseError::Limit(_)),
                }),
            }
            Vec::new()
        }

        ClientMsg::Release { request, session, lease } => {
            let Some(id) = state.resolve(&session) else {
                out.send(&unknown_session(request));
                return Vec::new();
            };
            match state.leases.release(id, lease, now()) {
                Ok(effects) => {
                    out.send(&ToClient::Ok { request });
                    state.dispatch(effects)
                }
                Err(err) => {
                    out.send(&ToClient::Error {
                        request,
                        error: err.to_string(),
                        retryable: false,
                    });
                    Vec::new()
                }
            }
        }

        ClientMsg::Status { request, session } => {
            let Some(id) = state.resolve(&session) else {
                out.send(&unknown_session(request));
                return Vec::new();
            };
            let t = now();
            let leases: Vec<LeaseStatus> = state
                .leases
                .leases()
                .filter(|l| l.session == id)
                .map(|l| LeaseStatus {
                    lease: l.id,
                    slots: l.slots.clone(),
                    expires_at: l.expires_at,
                    remaining: l.remaining(t),
                    state: match l.state {
                        benchd_core::lease::LeaseState::Held => "held".into(),
                        benchd_core::lease::LeaseState::Revoking { .. } => "revoking".into(),
                    },
                })
                .collect();
            out.send(&ToClient::Status { request, leases });
            Vec::new()
        }

        ClientMsg::TagList { request } => {
            let t = now();
            let busy = state.leases.busy(t);
            let inventory = state.leases.inventory();
            let counts = inventory.tag_counts();

            let mut tags: Vec<TagInfo> = Vec::new();
            for (tag, benches) in &counts {
                if tag.key == "name" {
                    continue;
                }
                let free = inventory
                    .enabled_benches()
                    .iter()
                    .filter(|b| b.tags.contains(tag) && !busy.contains_key(&b.id))
                    .count();
                tags.push(TagInfo {
                    tag: tag.to_string(),
                    description: inventory
                        .vocabulary
                        .describe(tag)
                        .unwrap_or_default()
                        .to_string(),
                    benches: *benches,
                    free,
                });
            }
            out.send(&ToClient::Tags { request, tags });
            Vec::new()
        }
    }
}

fn unknown_session(request: RequestId) -> ToClient {
    ToClient::Error {
        request,
        error: "unknown session; register again".into(),
        retryable: false,
    }
}

fn to_claim_request(
    spec: &ClaimSpec,
    vocabulary: &benchd_core::tags::Vocabulary,
) -> Result<ClaimRequest, String> {
    let mut slots = std::collections::BTreeMap::new();
    for (name, tags) in &spec.slots {
        let requirement = Requirement::parse(tags.iter().map(String::as_str))
            .map_err(|e| e.to_string())?;
        // Check against the vocabulary *before* matching, so `soc=esp32s4`
        // comes back as "did you mean soc=esp32s3?" rather than the far less
        // useful "no bench matches" (D10). A typo is then fixable in one turn.
        vocabulary.check(&requirement.tags).map_err(|e| e.to_string())?;
        slots.insert(name.clone(), requirement);
    }
    if slots.is_empty() {
        return Err("a claim must request at least one slot".into());
    }
    Ok(ClaimRequest {
        slots,
        distinct: if spec.distinct { Distinct::All } else { Distinct::None },
        ttl_seconds: spec.ttl,
        reason: spec.reason.clone(),
    })
}
