//! benchd client daemon: one privileged process per agent machine.
//!
//! Holds the coordinator connections and does all materialisation.
//!
//! **Several coordinators at once, deliberately.** The usual setup is a shared
//! lab server plus a local coordinator owning the operator's own boards, bound
//! to loopback so it is private without any authentication. Each is an
//! independent authority (D6), so they fail independently: the lab server
//! restarting must not disturb a lease on a board plugged into this machine.
//! The agent sees one merged view and never learns there is more than one. It runs as
//! root because the device node has to appear on *this* machine and
//! bind-mounting an inode needs `CAP_SYS_ADMIN` — the agent itself stays
//! unprivileged, which is the whole point (D8).
//!
//! Agents do not talk to this process directly. `benchd-mcp` — a thin,
//! unprivileged stdio shim, one per agent — connects over a unix socket. That
//! split is forced rather than chosen: MCP over stdio is one process per
//! client, so a single daemon could not serve several agents.

mod agent;
mod ids;
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
use crate::ids::{CoordinatorId, LeaseKey, SessionKey};
use crate::materialize::Materializer;

#[derive(Parser, Clone)]
#[command(
    name = "benchd-clientd",
    about = "benchd client daemon (one per machine)"
)]
struct Args {
    /// A coordinator to dial, as `name=address` or just `address`.
    ///
    /// Repeat for each one. The usual setup is a shared lab server plus a local
    /// coordinator owning this machine's own boards, and order matters: a claim
    /// is offered to them in the order given, so listing the local one first
    /// prefers your own hardware over the shared pool.
    #[arg(
        long = "coordinator",
        action = clap::ArgAction::Append,
        default_values_t = [format!("local=127.0.0.1:{DEFAULT_PORT}")]
    )]
    coordinators: Vec<String>,

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

/// One configured coordinator.
#[derive(Clone, Debug)]
pub struct Coordinator {
    pub id: CoordinatorId,
    /// Shown to operators and used to disambiguate bench names.
    pub name: String,
    pub address: String,
}

impl Coordinator {
    /// Parse `name=address`, or `address` with the host part as the name.
    fn parse(index: u32, spec: &str) -> Coordinator {
        let (name, address) = match spec.split_once('=') {
            Some((name, address)) => (name.to_string(), address.to_string()),
            None => (
                spec.split(':').next().unwrap_or(spec).to_string(),
                spec.to_string(),
            ),
        };
        Coordinator {
            id: CoordinatorId(index),
            name,
            address,
        }
    }
}

pub struct Shared {
    pub coordinators: Vec<Coordinator>,
    /// Outbound queue per coordinator; absent while that link is down.
    pub links: Mutex<BTreeMap<CoordinatorId, mpsc::UnboundedSender<String>>>,
    /// One lock per lease, so operations on a single lease stay ordered while
    /// different leases proceed independently.
    pub lease_locks: Mutex<BTreeMap<LeaseKey, Arc<Mutex<()>>>>,
    pub materializer: Mutex<Materializer>,
    pub agents: Agents,
    pub root: std::path::PathBuf,
}

impl Shared {
    async fn lease_queue(&self, lease: LeaseKey) -> Arc<Mutex<()>> {
        let mut locks = self.lease_locks.lock().await;
        Arc::clone(
            locks
                .entry(lease)
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    pub fn name_of(&self, id: CoordinatorId) -> &str {
        self.coordinators
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.name.as_str())
            .unwrap_or("?")
    }

    /// Send to one coordinator.
    ///
    /// The sender is cloned out under a short lock and the channel is
    /// unbounded, so this never blocks. An earlier version used `try_lock` and
    /// dropped the message when merely contended, reporting it as "link is
    /// down" — a silent loss with a misleading log line.
    pub async fn send(&self, to: CoordinatorId, msg: &ClientMsg) {
        let line = match serde_json::to_string(msg) {
            Ok(line) => line,
            Err(err) => {
                tracing::error!(?err, "failed to encode message");
                return;
            }
        };
        let tx = self.links.lock().await.get(&to).cloned();
        match tx {
            Some(tx) => {
                let _ = tx.send(line);
            }
            None => tracing::warn!(
                coordinator = %self.name_of(to),
                "link is down; message dropped"
            ),
        }
    }

    /// Every coordinator currently connected.
    pub async fn live_links(&self) -> Vec<CoordinatorId> {
        self.links.lock().await.keys().copied().collect()
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
    // Root-owned and not writable by agents. Sandboxes ask this daemon to create
    // their lease directory (`PrepareOwner`) rather than creating it themselves:
    // an agent that can write its own lease directory can plant a symlink where
    // root will later create the next lease, and root follows it.
    std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .with_context(|| format!("failed to set permissions on {}", root.display()))?;

    let coordinators: Vec<Coordinator> = args
        .coordinators
        .iter()
        .enumerate()
        .map(|(i, spec)| Coordinator::parse(i as u32, spec))
        .collect();
    for c in &coordinators {
        tracing::info!(name = %c.name, address = %c.address, id = %c.id, "coordinator");
    }

    let mut materializer = Materializer::new(root.clone(), coordinators.clone());
    // Nothing we mounted survives us in any meaningful sense: every coordinator
    // holds its own lease state and all of them have forgotten everything (D6).
    materializer.clear_stale().await;

    let shared = Arc::new(Shared {
        coordinators: coordinators.clone(),
        links: Mutex::new(BTreeMap::new()),
        lease_locks: Mutex::new(BTreeMap::new()),
        materializer: Mutex::new(materializer),
        agents: Agents::default(),
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

    // One reconnect loop per coordinator. They are independent authorities, so
    // one being unreachable must not disturb the others: a lab server restart
    // cannot be allowed to tear down a lease on a board plugged into this
    // machine.
    let mut tasks = Vec::new();
    for coordinator in coordinators {
        let shared = Arc::clone(&shared);
        let args = args.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                if let Err(err) = run(&args, Arc::clone(&shared), &coordinator).await {
                    tracing::warn!(
                        coordinator = %coordinator.name, ?err, "link failed"
                    );
                }
                // Only this coordinator's leases are void.
                {
                    let mut m = shared.materializer.lock().await;
                    m.clear_coordinator(coordinator.id).await;
                }
                shared.agents.invalidate(coordinator.id).await;
                shared.links.lock().await.remove(&coordinator.id);
                tokio::time::sleep(Duration::from_secs(3)).await;
                tracing::info!(coordinator = %coordinator.name, "reconnecting");
            }
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
    Ok(())
}

async fn run(args: &Args, shared: Arc<Shared>, coordinator: &Coordinator) -> Result<()> {
    let socket = tokio::net::TcpStream::connect(&coordinator.address)
        .await
        .with_context(|| format!("failed to dial {}", coordinator.address))?;
    socket.set_nodelay(true).ok();
    tracing::info!(
        coordinator = %coordinator.name, address = %coordinator.address, "connected"
    );

    let (read, write) = socket.into_split();
    let mut lines = FramedRead::new(read, LinesCodec::new());
    let mut sink = FramedWrite::new(write, LinesCodec::new());

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    shared.links.lock().await.insert(coordinator.id, tx.clone());

    // Agents that were connected across the outage get fresh sessions without
    // having to notice anything: their stdio shims are still running and their
    // unix sockets never closed.
    shared.agents.reregister_all(&shared, coordinator.id).await;

    let heartbeat = {
        let tx = tx.clone();
        let period = Duration::from_secs(args.heartbeat_seconds.max(1));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let Ok(line) = serde_json::to_string(&ClientMsg::Heartbeat) else {
                    break;
                };
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
                dispatch(&shared, coordinator.id, msg);
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
fn dispatch(shared: &Arc<Shared>, from: CoordinatorId, msg: ToClient) {
    let lease = match &msg {
        ToClient::Materialize { lease, .. }
        | ToClient::Unmaterialize { lease, .. }
        | ToClient::Failed { lease, .. } => Some(LeaseKey::new(from, *lease)),
        _ => None,
    };

    // A failed claim is told to the agent *immediately*, ahead of any queued
    // work for that lease. The teardown below still has to wait its turn behind
    // the materialisation it undoes, but the agent must not: waiting would mean
    // it learns the real reason only after its own 30s timeout has already
    // reported something vaguer.
    if let ToClient::Failed { lease, detail } = &msg {
        let (shared, key, detail) = (
            Arc::clone(shared),
            LeaseKey::new(from, *lease),
            detail.clone(),
        );
        tokio::spawn(async move { shared.agents.notify_failed(key, &detail).await });
    }

    let Some(lease) = lease else {
        // Cheap and order-independent: handle inline.
        let shared = Arc::clone(shared);
        tokio::spawn(async move { handle(&shared, from, msg).await });
        return;
    };

    let shared = Arc::clone(shared);
    tokio::spawn(async move {
        let queue = shared.lease_queue(lease).await;
        let _guard = queue.lock().await;
        handle(&shared, from, msg).await;
    });
}

async fn handle(shared: &Arc<Shared>, from: CoordinatorId, msg: ToClient) {
    match msg {
        // Instructions we execute.
        ToClient::Materialize {
            request,
            lease,
            epoch,
            session,
            slots,
        } => {
            let key = LeaseKey::new(from, lease);
            let session = SessionKey::new(from, session);
            let owner = shared.agents.owner_for(session).await;
            let uid = shared.agents.uid_for(session).await;
            let outcome = {
                let mut m = shared.materializer.lock().await;
                m.materialize(key, epoch, &owner, uid, &slots).await
            };
            if let benchd_core::wire::Outcome::Ok = outcome {
                let paths = paths_for(&shared.root, &owner, key, &slots);
                shared.agents.deliver_paths(key, paths).await;
            } else {
                tracing::error!(%key, ?outcome, "materialisation failed");
            }
            shared
                .send(
                    from,
                    &ClientMsg::Done {
                        request,
                        result: outcome,
                    },
                )
                .await;
        }
        ToClient::Unmaterialize {
            request,
            lease,
            epoch,
            session,
        } => {
            let key = LeaseKey::new(from, lease);
            let owner = shared
                .agents
                .owner_for(SessionKey::new(from, session))
                .await;
            let outcome = {
                let mut m = shared.materializer.lock().await;
                m.unmaterialize(key, epoch, &owner).await
            };
            shared
                .send(
                    from,
                    &ClientMsg::Done {
                        request,
                        result: outcome,
                    },
                )
                .await;
        }

        // Unsolicited lease events: forward to whichever agent holds it.
        ToClient::Revoking {
            lease,
            reason,
            teardown_at,
        } => {
            shared
                .agents
                .notify_revoking(LeaseKey::new(from, lease), &reason, teardown_at)
                .await;
        }
        ToClient::Ended { lease, reason } => {
            shared
                .agents
                .notify_ended(LeaseKey::new(from, lease), &reason)
                .await;
        }
        ToClient::Failed { lease, detail: _ } => {
            // The agent has already been told (see `dispatch`); this is the
            // teardown, which had to wait for any in-flight materialisation of
            // the same lease so it cannot undo work that has not happened yet.
            let owner = shared
                .agents
                .owner_for(lease_session(shared, LeaseKey::new(from, lease)).await)
                .await;
            let mut m = shared.materializer.lock().await;
            m.unmaterialize_now(LeaseKey::new(from, lease), &owner)
                .await;
        }

        // Replies to agent requests.
        other => {
            // A claim the previous coordinator could not satisfy is offered to
            // the next one.
            if let Some(retry) = shared.agents.deliver_reply(from, other).await {
                shared
                    .send(
                        retry.to,
                        &ClientMsg::Claim {
                            request: retry.request,
                            session: retry.session,
                            claim: retry.claim,
                        },
                    )
                    .await;
            }
        }
    }
}

/// Which session a lease belongs to, as far as this daemon knows.
async fn lease_session(shared: &Arc<Shared>, lease: LeaseKey) -> SessionKey {
    shared.agents.session_for_lease(lease).await
}

fn paths_for(
    root: &std::path::Path,
    owner: &str,
    lease: LeaseKey,
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
