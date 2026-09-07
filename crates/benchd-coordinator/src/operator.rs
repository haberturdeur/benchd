//! The operator subcommands: `benches`, `leases`, `release`.
//!
//! A **different surface** from the one agents get, not a privileged mode of it
//! (D17). Agents get five MCP tools and no way to name a bench; an operator
//! sees the whole picture and can take hardware back. That the two now ship in
//! one executable changes nothing: the boundary is the tool list an agent is
//! handed, never which file it lives in (D25).
//!
//! Talks the same JSON-lines protocol as everything else, as an operator
//! connection.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use benchd_core::wire::{OperatorMsg, ToOperator};
use clap::Subcommand;
use futures::{SinkExt, StreamExt};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

#[derive(Subcommand)]
pub enum Command {
    /// Show every bench, its tags, and who holds it.
    Benches,
    /// Show every live lease.
    Leases,
    /// Take a bench back from whoever is holding it.
    ///
    /// The holder gets a grace window to park the board before the device
    /// disappears — yanking a device mid-flash can leave it in bootloader.
    Release {
        /// Bench id, as shown by `benchd benches`.
        #[arg(long)]
        bench: String,
        /// Skip the grace window. Only when you know nothing is mid-write.
        #[arg(long)]
        now: bool,
    },
}

pub async fn run(coordinator: &str, command: Command) -> Result<()> {
    let socket = tokio::net::TcpStream::connect(coordinator)
        .await
        .with_context(|| format!("failed to reach the coordinator at {coordinator}"))?;
    socket.set_nodelay(true).ok();
    let (read, write) = socket.into_split();
    let mut lines = FramedRead::new(read, LinesCodec::new());
    let mut sink = FramedWrite::new(write, LinesCodec::new());

    let request = match &command {
        Command::Benches | Command::Leases => OperatorMsg::Inspect,
        Command::Release { bench, now } => OperatorMsg::ForceRelease {
            bench: bench.clone(),
            immediate: *now,
        },
    };
    sink.send(serde_json::to_string(&request)?).await?;

    let line = tokio::time::timeout(std::time::Duration::from_secs(10), lines.next())
        .await
        .map_err(|_| anyhow!("the coordinator did not reply within 10s"))?
        .ok_or_else(|| anyhow!("the coordinator closed the connection"))??;
    let reply: ToOperator = serde_json::from_str(&line)
        .with_context(|| format!("could not understand the coordinator's reply: {line}"))?;

    match (&command, reply) {
        (Command::Benches, ToOperator::State { benches, leases }) => {
            print_benches(&benches, &leases)
        }
        (Command::Leases, ToOperator::State { leases, .. }) => print_leases(&leases),
        (Command::Release { bench, .. }, ToOperator::Released { count }) => {
            if count == 0 {
                println!("{bench}: nothing was holding it");
            } else {
                println!("{bench}: released ({count} lease(s))");
            }
        }
        (_, ToOperator::Error { error }) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
        (_, other) => eprintln!("unexpected reply: {other:?}"),
    }
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn print_benches(
    benches: &[benchd_core::wire::BenchView],
    leases: &[benchd_core::wire::LeaseView],
) {
    if benches.is_empty() {
        println!("no benches registered (is any `benchd host` running?)");
        return;
    }
    let held: BTreeMap<&str, &benchd_core::wire::LeaseView> = leases
        .iter()
        .flat_map(|l| l.slots.values().map(move |b| (b.as_str(), l)))
        .collect();

    let width = benches.iter().map(|b| b.id.len()).max().unwrap_or(0);
    for bench in benches {
        let status = match held.get(bench.id.as_str()) {
            Some(lease) => format!(
                "held by {} — {} ({}s left)",
                lease.owner,
                if lease.reason.is_empty() {
                    "no reason given"
                } else {
                    &lease.reason
                },
                lease.expires_at.saturating_sub(now())
            ),
            None => "free".into(),
        };
        println!("{:<width$}  {}", bench.id, status, width = width);
        // Tags second, indented: when you are looking for a free board the
        // status is what you are scanning for.
        println!("{:<width$}  {}", "", bench.tags.join(" "), width = width);
        if !bench.description.is_empty() || !bench.has_docs {
            let description = if bench.description.is_empty() {
                "(no description)"
            } else {
                &bench.description
            };
            // Flagging the *absence* of docs, because a bench nobody documented
            // is the one an agent will waste a lease guessing at.
            let docs = if bench.has_docs { "" } else { "  [no docs]" };
            println!("{:<width$}  {description}{docs}", "", width = width);
        }
    }
}

fn print_leases(leases: &[benchd_core::wire::LeaseView]) {
    if leases.is_empty() {
        println!("no live leases");
        return;
    }
    let t = now();
    for lease in leases {
        let slots = lease
            .slots
            .iter()
            .map(|(slot, bench)| format!("{slot}={bench}"))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "l{}  {}  {}  {}s left  {}",
            lease.id,
            lease.owner,
            lease.state,
            lease.expires_at.saturating_sub(t),
            slots
        );
        if !lease.reason.is_empty() {
            println!("      {}", lease.reason);
        }
    }
}
