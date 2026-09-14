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
//! root because the device node has to appear on *this* machine: creating one
//! needs `CAP_MKNOD`, giving it to the agent needs `CAP_CHOWN`, and importing
//! the device over USB/IP needs root. The agent itself stays unprivileged,
//! which is the whole point (D8).
//!
//! Agents do not talk to this process directly. `benchd mcp` — a thin,
//! unprivileged stdio shim, one per agent — connects over a unix socket. That
//! split is forced rather than chosen: MCP over stdio is one process per
//! client, so a single daemon could not serve several agents.

mod agent;
mod ids;
pub mod lease;
mod materialize;

use std::collections::BTreeMap;
use std::os::unix::fs::DirBuilderExt;
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
pub struct ClientArgs {
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
    /// One queue per lease, so operations on a single lease stay ordered while
    /// different leases proceed independently.
    pub lease_queues: Arc<LeaseQueues>,
    pub materializer: Mutex<Materializer>,
    pub agents: Agents,
    pub root: std::path::PathBuf,
}

/// One piece of work for one lease.
type Job = futures::future::BoxFuture<'static, ()>;

/// A serial queue per lease, fed in the order messages arrived.
///
/// The ordering has to be established by the thread that *reads* the messages,
/// which is why [`LeaseQueues::enqueue`] is not `async`: it takes a lock only
/// long enough to look up a `BTreeMap` and push into an unbounded channel, and
/// never for the work itself. An earlier version spawned a task per message
/// which then took a per-lease lock, so the effective order was whichever task
/// reached `lock()` first — an `Unmaterialize` could and did overtake the
/// `Materialize` it undoes, and the epoch fence cannot catch it, because both
/// messages for one lease carry the same epoch. What that leaves behind is a
/// device imported, materialised and given to a departed agent, with no lease
/// anywhere that could ever tear it down.
#[derive(Default)]
pub struct LeaseQueues {
    /// Deliberately a `std::sync::Mutex`, not tokio's: an async lock would put
    /// an await point between a message being read and its place in its lease's
    /// queue being fixed, which is the whole bug.
    ///
    /// Entries are dropped as soon as a lease's queue drains, so a long-lived
    /// daemon does not accumulate one per lease it has ever seen.
    queues: std::sync::Mutex<BTreeMap<LeaseKey, mpsc::UnboundedSender<Job>>>,
}

impl LeaseQueues {
    /// Queue work for one lease. Returns immediately; the work runs on that
    /// lease's own task, so a 30s import blocks neither the read loop nor any
    /// other lease.
    pub fn enqueue(self: &Arc<Self>, lease: LeaseKey, job: Job) {
        let mut queues = self.lock();
        if let Some(queue) = queues.get(&lease) {
            match queue.send(job) {
                Ok(()) => return,
                // Only reachable if the worker died with the entry still in
                // place, which means a job panicked. Starting a fresh worker
                // beats silently dropping every later message for that lease.
                Err(mpsc::error::SendError(returned)) => {
                    tracing::error!(%lease, "the queue for this lease died; starting another");
                    queues.remove(&lease);
                    drop(queues);
                    return self.enqueue(lease, returned);
                }
            }
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(job);
        queues.insert(lease, tx);
        drop(queues);

        let queues = Arc::clone(self);
        tokio::spawn(async move { queues.serve(lease, rx).await });
    }

    /// Run one lease's queue until it is empty, then retire it.
    async fn serve(self: Arc<Self>, lease: LeaseKey, mut rx: mpsc::UnboundedReceiver<Job>) {
        loop {
            let next = {
                // Held across the emptiness check and the removal, and it is
                // the same lock `enqueue` takes to push: a message arriving at
                // this instant therefore either lands in this queue or starts a
                // new one, and cannot fall between the two.
                let mut queues = self.lock();
                match rx.try_recv() {
                    Ok(job) => Some(job),
                    Err(_) => {
                        queues.remove(&lease);
                        None
                    }
                }
            };
            let Some(job) = next else { return };
            job.await;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<LeaseKey, mpsc::UnboundedSender<Job>>> {
        // Nothing held across an await and nothing that can panic while it is
        // held, so a poisoned lock would mean a job panicked mid-`send`, which
        // it cannot.
        self.queues.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.lock().len()
    }
}

impl Shared {
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

pub async fn run(args: ClientArgs) -> Result<()> {
    let root = std::path::PathBuf::from(&args.root);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(&root)
        .with_context(|| format!("failed to create {}", root.display()))?;
    // Root-owned and not writable by agents, and explicitly so rather than by
    // whatever the umask happens to be. Sandboxes ask this daemon to create
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
    // Nothing we materialised survives us in any meaningful sense: every
    // coordinator holds its own lease state and all of them have forgotten
    // everything (D6).
    materializer.clear_stale().await;

    let shared = Arc::new(Shared {
        coordinators: coordinators.clone(),
        links: Mutex::new(BTreeMap::new()),
        lease_queues: Arc::new(LeaseQueues::default()),
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
                if let Err(err) = link(&args, Arc::clone(&shared), &coordinator).await {
                    tracing::warn!(
                        coordinator = %coordinator.name, ?err, "link failed"
                    );
                }
                // Retire the link first. Tearing down its leases below unmounts
                // and detaches per resource, which takes seconds, and until the
                // sender is gone `live_links` still offers this coordinator as
                // a target — so every request routed during the teardown would
                // be written into a channel whose receiver died with `link`,
                // and answered by nobody.
                shared.links.lock().await.remove(&coordinator.id);
                // Only this coordinator's leases are void.
                {
                    let mut m = shared.materializer.lock().await;
                    m.clear_coordinator(coordinator.id).await;
                }
                shared.agents.invalidate(coordinator.id).await;
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

/// One connection to one coordinator, from dial until it drops.
async fn link(args: &ClientArgs, shared: Arc<Shared>, coordinator: &Coordinator) -> Result<()> {
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

    // Queued here, synchronously, so the order is the order these arrived in.
    // Spawning first and taking a per-lease lock inside the task ordered them
    // by whichever task got scheduled first instead, which is no order at all.
    let queued = Arc::clone(shared);
    shared.lease_queues.enqueue(
        lease,
        Box::pin(async move { handle(&queued, from, msg).await }),
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use benchd_core::lease::LeaseId;

    fn lease(n: u64) -> LeaseKey {
        LeaseKey::new(CoordinatorId(0), LeaseId(n))
    }

    /// The property the per-lease queue exists for.
    ///
    /// An `Unmaterialize` that overtakes the `Materialize` it undoes finds
    /// nothing, returns `Ok`, and then the materialisation runs — leaving a
    /// device imported and handed to an agent that has gone, with no lease left
    /// anywhere to tear it down. It is a race either way round, so this repeats:
    /// the shape it replaces reorders these four in about 40% of rounds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn work_for_one_lease_runs_in_the_order_it_arrived() {
        for round in 0..200 {
            let queues = Arc::new(LeaseQueues::default());
            let order = Arc::new(std::sync::Mutex::new(Vec::new()));
            let (done, mut finished) = mpsc::unbounded_channel();

            for step in 0..4 {
                let (order, done) = (Arc::clone(&order), done.clone());
                queues.enqueue(
                    lease(1),
                    Box::pin(async move {
                        // The first job gives the runtime every chance to run a
                        // later one ahead of it, which is what used to happen.
                        for _ in 0..step.max(1) {
                            tokio::task::yield_now().await;
                        }
                        order.lock().unwrap().push(step);
                        let _ = done.send(());
                    }),
                );
            }
            drop(done);
            while finished.recv().await.is_some() {}

            assert_eq!(
                *order.lock().unwrap(),
                vec![0, 1, 2, 3],
                "reordered on round {round}"
            );
        }
    }

    /// The reason the work is not simply awaited in the read loop: a claim on
    /// one bench must not wait behind a 30s USB/IP import on another.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn one_lease_does_not_hold_up_another() {
        let queues = Arc::new(LeaseQueues::default());
        let (unblock, mut blocked) = mpsc::unbounded_channel();
        let (report, mut ran) = mpsc::unbounded_channel();

        // Deadlocks rather than merely slows down if these two share a queue.
        let finish = report.clone();
        queues.enqueue(
            lease(1),
            Box::pin(async move {
                let _ = blocked.recv().await;
                let _ = finish.send("slow");
            }),
        );
        queues.enqueue(
            lease(2),
            Box::pin(async move {
                let _ = unblock.send(());
                let _ = report.send("quick");
            }),
        );

        let first = tokio::time::timeout(Duration::from_secs(5), ran.recv())
            .await
            .expect("a second lease must not wait behind the first");
        assert_eq!(first, Some("quick"));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), ran.recv())
                .await
                .expect("and the first still runs"),
            Some("slow")
        );
    }

    /// A daemon that runs for months must not keep a queue per lease it has
    /// ever served.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_drained_queue_is_retired() {
        let queues = Arc::new(LeaseQueues::default());
        let (done, mut finished) = mpsc::unbounded_channel();
        for n in 1..20 {
            let done = done.clone();
            queues.enqueue(
                lease(n),
                Box::pin(async move {
                    let _ = done.send(());
                }),
            );
        }
        drop(done);
        while finished.recv().await.is_some() {}

        // The worker retires its queue just after the last job returns, so give
        // it a moment rather than racing it.
        for _ in 0..100 {
            if queues.tracked() == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{} queues left behind", queues.tracked());
    }
}
