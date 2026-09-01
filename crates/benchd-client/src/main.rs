//! benchd client daemon: one privileged process per agent machine.
//!
//! Holds the coordinator connection and does all materialisation. It runs as
//! root because the device node has to appear on *this* machine and
//! bind-mounting an inode needs `CAP_SYS_ADMIN` — the agent itself stays
//! unprivileged, which is the whole point (D8).
//!
//! Agents do not talk to this process directly. `benchd-mcp` — a thin,
//! unprivileged stdio shim, one per agent — connects over a unix socket. That
//! split is forced rather than chosen: MCP over stdio is one process per
//! client, so a single daemon could not serve several agents.

mod agent;
mod materialize;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use benchd_core::wire::{ClientMsg, ToClient, DEFAULT_PORT};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, Mutex};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

use crate::agent::Agents;
use crate::materialize::Materializer;

#[derive(Parser)]
#[command(name = "benchd-clientd", about = "benchd client daemon (one per machine)")]
struct Args {
    /// Coordinator to dial. Only the coordinator listens (D5).
    #[arg(long, default_value_t = format!("127.0.0.1:{DEFAULT_PORT}"))]
    coordinator: String,

    /// Where device nodes are materialised. Bind-mount `<root>/<owner>` into an
    /// agent's sandbox and its leases appear and disappear live.
    #[arg(long, default_value = "/run/benchd")]
    root: String,

    /// Unix socket the per-agent MCP shims connect to.
    #[arg(long, default_value = "/run/benchd/agent.sock")]
    socket: String,

    #[arg(long, default_value_t = 10)]
    heartbeat_seconds: u64,
}

pub struct Shared {
    /// One lock per lease, so operations on a single lease stay ordered while
    /// different leases proceed independently.
    pub lease_locks: Mutex<BTreeMap<benchd_core::lease::LeaseId, Arc<Mutex<()>>>>,
    pub materializer: Mutex<Materializer>,
    pub agents: Agents,
    /// Outbound queue to the coordinator. Unbounded and non-blocking, so a
    /// stalled coordinator link can never deadlock a materialisation.
    pub to_coordinator: Mutex<Option<mpsc::UnboundedSender<String>>>,
    pub root: std::path::PathBuf,
}

impl Shared {
    async fn lease_queue(&self, lease: benchd_core::lease::LeaseId) -> Arc<Mutex<()>> {
        let mut locks = self.lease_locks.lock().await;
        Arc::clone(locks.entry(lease).or_insert_with(|| Arc::new(Mutex::new(()))))
    }

    pub async fn send(&self, msg: &ClientMsg) {
        let line = match serde_json::to_string(msg) {
            Ok(line) => line,
            Err(err) => {
                tracing::error!(?err, "failed to encode message");
                return;
            }
        };
        // The sender is cloned out under a short lock and the channel itself is
        // unbounded, so sending never blocks. An earlier version used try_lock
        // and dropped the message when merely contended, reporting it as "link
        // is down" — which was both a silent loss and a misleading log line.
        let tx = self.to_coordinator.lock().await.clone();
        match tx {
            Some(tx) => {
                let _ = tx.send(line);
            }
            None => tracing::warn!("coordinator link is down; message dropped"),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "benchd_client=info".into()),
        )
        .init();

    let args = Args::parse();
    let root = std::path::PathBuf::from(&args.root);
    std::fs::create_dir_all(&root)
        .with_context(|| format!("failed to create {}", root.display()))?;

    let mut materializer = Materializer::new(root.clone(), args.coordinator.clone());
    // Nothing we mounted survives us in any meaningful sense: the coordinator
    // holds all lease state and has forgotten everything (D6).
    materializer.clear_stale().await;

    let shared = Arc::new(Shared {
        lease_locks: Mutex::new(BTreeMap::new()),
        materializer: Mutex::new(materializer),
        agents: Agents::default(),
        to_coordinator: Mutex::new(None),
        root,
    });

    // Local agent socket.
    {
        let shared = Arc::clone(&shared);
        let path = args.socket.clone();
        tokio::spawn(async move {
            if let Err(err) = agent::serve(shared, &path).await {
                tracing::error!(?err, "agent socket failed");
            }
        });
    }

    loop {
        if let Err(err) = run(&args, Arc::clone(&shared)).await {
            tracing::warn!(?err, "coordinator link failed");
        }
        // Every lease was granted by a coordinator we can no longer reach, so
        // none of them are valid any more (D6).
        {
            let mut m = shared.materializer.lock().await;
            m.clear_stale().await;
        }
        shared.agents.invalidate_all().await;
        *shared.to_coordinator.lock().await = None;
        tokio::time::sleep(Duration::from_secs(3)).await;
        tracing::info!("reconnecting");
    }
}

async fn run(args: &Args, shared: Arc<Shared>) -> Result<()> {
    let socket = tokio::net::TcpStream::connect(&args.coordinator)
        .await
        .with_context(|| format!("failed to dial {}", args.coordinator))?;
    socket.set_nodelay(true).ok();
    tracing::info!(coordinator = %args.coordinator, "connected");

    let (read, write) = socket.into_split();
    let mut lines = FramedRead::new(read, LinesCodec::new());
    let mut sink = FramedWrite::new(write, LinesCodec::new());

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    *shared.to_coordinator.lock().await = Some(tx.clone());

    // Agents that were connected across the outage get fresh sessions without
    // having to notice anything: their stdio shims are still running and their
    // unix sockets never closed.
    shared.agents.reregister_all(&shared).await;

    let heartbeat = {
        let tx = tx.clone();
        let period = Duration::from_secs(args.heartbeat_seconds.max(1));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let Ok(line) = serde_json::to_string(&ClientMsg::Heartbeat) else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        })
    };

    let result = loop {
        tokio::select! {
            outbound = rx.recv() => {
                let Some(line) = outbound else { break Ok(()) };
                sink.send(line).await?;
            }
            inbound = lines.next() => {
                let Some(line) = inbound else { break Ok(()) };
                let line = line?;
                let msg: ToClient = match serde_json::from_str(&line) {
                    Ok(msg) => msg,
                    Err(err) => {
                        tracing::warn!(?err, %line, "undecodable message");
                        continue;
                    }
                };
                // Dispatched, not awaited: materialisation can take tens of
                // seconds (a USB/IP import waits for the kernel to enumerate),
                // and awaiting it here blocks the read loop for every other
                // lease and every other agent on this machine. A revocation
                // notice that arrives 30s late is a device yanked without
                // warning.
                dispatch(&shared, msg);
            }
        }
    };

    heartbeat.abort();
    result
}

/// Route one coordinator message.
///
/// Per-lease work is queued so operations on one lease stay in order — an
/// `Unmaterialize` must never overtake the `Materialize` it undoes — while work
/// on different leases proceeds in parallel and cheap notifications are never
/// stuck behind either.
fn dispatch(shared: &Arc<Shared>, msg: ToClient) {
    let lease = match &msg {
        ToClient::Materialize { lease, .. }
        | ToClient::Unmaterialize { lease, .. }
        | ToClient::Failed { lease, .. } => Some(*lease),
        _ => None,
    };

    // A failed claim is told to the agent *immediately*, ahead of any queued
    // work for that lease. The teardown below still has to wait its turn behind
    // the materialisation it undoes, but the agent must not: waiting would mean
    // it learns the real reason only after its own 30s timeout has already
    // reported something vaguer.
    if let ToClient::Failed { lease, detail } = &msg {
        let (shared, lease, detail) = (Arc::clone(shared), *lease, detail.clone());
        tokio::spawn(async move { shared.agents.notify_failed(lease, &detail).await });
    }

    let Some(lease) = lease else {
        // Cheap and order-independent: handle inline.
        let shared = Arc::clone(shared);
        tokio::spawn(async move { handle(&shared, msg).await });
        return;
    };

    let shared = Arc::clone(shared);
    tokio::spawn(async move {
        let queue = shared.lease_queue(lease).await;
        let _guard = queue.lock().await;
        handle(&shared, msg).await;
    });
}

async fn handle(shared: &Arc<Shared>, msg: ToClient) {
    match msg {
        // Instructions we execute.
        ToClient::Materialize { request, lease, epoch, session, slots } => {
            let owner = shared.agents.owner_for(session).await;
            let outcome = {
                let mut m = shared.materializer.lock().await;
                m.materialize(lease, epoch, &owner, &slots).await
            };
            if let benchd_core::wire::Outcome::Ok = outcome {
                let paths = paths_for(&shared.root, &owner, lease, &slots);
                shared.agents.deliver_paths(lease, paths).await;
            } else {
                tracing::error!(%lease, ?outcome, "materialisation failed");
            }
            shared.send(&ClientMsg::Done { request, result: outcome }).await;
        }
        ToClient::Unmaterialize { request, lease, epoch, session } => {
            let owner = shared.agents.owner_for(session).await;
            let outcome = {
                let mut m = shared.materializer.lock().await;
                m.unmaterialize(lease, epoch, &owner).await
            };
            shared.send(&ClientMsg::Done { request, result: outcome }).await;
        }

        // Unsolicited lease events: forward to whichever agent holds it.
        ToClient::Revoking { lease, reason, teardown_at } => {
            shared.agents.notify_revoking(lease, &reason, teardown_at).await;
        }
        ToClient::Ended { lease, reason } => {
            shared.agents.notify_ended(lease, &reason).await;
        }
        ToClient::Failed { lease, detail: _ } => {
            // The agent has already been told (see `dispatch`); this is the
            // teardown, which had to wait for any in-flight materialisation of
            // the same lease so it cannot undo work that has not happened yet.
            let owner = shared.agents.owner_for(lease_session(shared, lease).await).await;
            let mut m = shared.materializer.lock().await;
            m.unmaterialize_now(lease, &owner).await;
        }

        // Replies to agent requests.
        other => shared.agents.deliver_reply(other).await,
    }
}

/// Which session a lease belongs to, as far as this daemon knows.
async fn lease_session(
    shared: &Arc<Shared>,
    lease: benchd_core::lease::LeaseId,
) -> benchd_core::lease::SessionId {
    shared.agents.session_for_lease(lease).await
}

fn paths_for(
    root: &std::path::Path,
    owner: &str,
    lease: benchd_core::lease::LeaseId,
    slots: &BTreeMap<String, BTreeMap<String, benchd_core::wire::ResourceHandle>>,
) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (slot, resources) in slots {
        let mut paths = BTreeMap::new();
        for name in resources.keys() {
            paths.insert(
                name.clone(),
                materialize::resource_path(root, owner, lease, slot, name)
                    .display()
                    .to_string(),
            );
        }
        out.insert(slot.clone(), paths);
    }
    out
}
