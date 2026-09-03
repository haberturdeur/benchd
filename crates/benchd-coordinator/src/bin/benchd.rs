//! `benchd` — the operator CLI.
//!
//! Deliberately a **separate program** from the agent surface, not a privileged
//! mode of it (D17). Agents get five tools and no way to name a bench; you get
//! the whole picture and the ability to take hardware back. Keeping them apart
//! is what stops an agent hardcoding a bench name into a test script.
//!
//! Talks the same JSON-lines protocol as everything else, as an operator
//! connection.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use benchd_core::wire::{OperatorMsg, ToOperator, DEFAULT_PORT};
use clap::{Parser, Subcommand};
use futures::{SinkExt, StreamExt};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

#[derive(Parser)]
#[command(name = "benchd", about = "benchd operator CLI", version)]
struct Args {
    #[arg(long, default_value_t = format!("127.0.0.1:{DEFAULT_PORT}"), env = "BENCHD_COORDINATOR")]
    coordinator: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let socket = tokio::net::TcpStream::connect(&args.coordinator)
        .await
        .with_context(|| format!("failed to reach the coordinator at {}", args.coordinator))?;
    socket.set_nodelay(true).ok();
    let (read, write) = socket.into_split();
    let mut lines = FramedRead::new(read, LinesCodec::new());
    let mut sink = FramedWrite::new(write, LinesCodec::new());

    let request = match &args.command {
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

    match (&args.command, reply) {
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
        println!("no benches registered (is any benchd-host running?)");
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
