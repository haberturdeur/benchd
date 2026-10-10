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
use benchd_core::wire::{
    ClientMsg, CoordinatorInventory, OperatorMsg, RequestId, ToClient, ToOperator, DEFAULT_PORT,
};
use clap::Subcommand;
use futures::{SinkExt, StreamExt};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

#[derive(Subcommand)]
pub enum Command {
    /// Show every bench, its tags, and who holds it.
    Benches {
        /// Show only benches carrying every given tag, e.g. host=lab-a soc=esp32s3.
        #[arg(value_name = "TAG")]
        tags: Vec<String>,
        /// Show benches on this host (shorthand for host=NAME).
        #[arg(long, value_name = "NAME")]
        host: Option<String>,
        /// The local client daemon whose connected coordinators should be shown.
        #[arg(long, default_value = "/run/benchd/agent.sock", env = "BENCHD_SOCKET")]
        socket: String,
    },
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

pub async fn run(coordinator: Option<&str>, command: Command) -> Result<()> {
    let filters = match &command {
        Command::Benches { tags, host, .. } => tags
            .iter()
            .cloned()
            .chain(host.iter().map(|host| format!("host={host}")))
            .map(|tag| benchd_core::tags::Tag::parse(&tag).map(|tag| tag.to_string()))
            .collect::<Result<Vec<_>, _>>()?,
        _ => Vec::new(),
    };
    if let (None, Command::Benches { socket, .. }) = (coordinator, &command) {
        return run_all(socket, &filters).await;
    }

    let default = format!("127.0.0.1:{DEFAULT_PORT}");
    let coordinator = coordinator.unwrap_or(&default);
    let mut socket = tokio::net::TcpStream::connect(coordinator)
        .await
        .with_context(|| format!("failed to reach the coordinator at {coordinator}"))?;
    socket.set_nodelay(true).ok();
    benchd_core::protocol::connect(&mut socket).await?;
    let (read, write) = socket.into_split();
    let mut lines = FramedRead::new(read, LinesCodec::new());
    let mut sink = FramedWrite::new(write, LinesCodec::new());

    let request = match &command {
        Command::Benches { .. } | Command::Leases => OperatorMsg::Inspect,
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
        (Command::Benches { .. }, ToOperator::State { benches, leases }) => print_benches(
            vec![CoordinatorInventory {
                name: coordinator.into(),
                local: false,
                benches,
                leases,
            }],
            &filters,
        ),
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

/// Ask the local client daemon for the complete view it already presents to
/// agents. It is the source of truth for which coordinators are configured and
/// currently connected; duplicating that list in an operator command is how
/// `benchd benches` used to silently show only the local authority.
async fn run_all(socket: &str, filters: &[String]) -> Result<()> {
    let mut socket = tokio::net::UnixStream::connect(socket)
        .await
        .with_context(|| {
            format!("could not reach the client daemon at {socket} — is `benchd client` running?")
        })?;
    benchd_core::protocol::connect(&mut socket).await?;
    let (read, write) = socket.into_split();
    let mut lines = FramedRead::new(read, LinesCodec::new());
    let mut sink = FramedWrite::new(write, LinesCodec::new());
    sink.send(serde_json::to_string(&ClientMsg::Inspect {
        request: RequestId(1),
    })?)
    .await?;

    let line = tokio::time::timeout(std::time::Duration::from_secs(15), lines.next())
        .await
        .map_err(|_| anyhow!("the client daemon did not reply within 15s"))?
        .ok_or_else(|| anyhow!("the client daemon closed the connection"))??;
    let reply: ToClient = serde_json::from_str(&line)
        .with_context(|| format!("could not understand the client daemon's reply: {line}"))?;
    match reply {
        ToClient::Inventory { coordinators, .. } => print_benches(coordinators, filters),
        ToClient::Error { error, .. } => return Err(anyhow!(error)),
        other => return Err(anyhow!("unexpected reply: {other:?}")),
    }
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn filter_benches(inventories: &mut [CoordinatorInventory], filters: &[String]) {
    for inventory in inventories {
        inventory
            .benches
            .retain(|bench| filters.iter().all(|tag| bench.tags.contains(tag)));
    }
}

fn print_benches(mut inventories: Vec<CoordinatorInventory>, filters: &[String]) {
    filter_benches(&mut inventories, filters);
    if inventories
        .iter()
        .all(|inventory| inventory.benches.is_empty())
    {
        if filters.is_empty() {
            println!("no benches registered (is any `benchd host` running?)");
        } else {
            println!("no benches match {}", filters.join(" "));
        }
        return;
    }

    let width = inventories
        .iter()
        .flat_map(|inventory| {
            inventory
                .benches
                .iter()
                .map(|bench| bench.id.len() + if inventory.local { " [local]".len() } else { 0 })
        })
        .max()
        .unwrap_or(0);
    for inventory in inventories {
        let held: BTreeMap<&str, &benchd_core::wire::LeaseView> = inventory
            .leases
            .iter()
            .flat_map(|lease| {
                lease
                    .slots
                    .values()
                    .map(move |bench| (bench.as_str(), lease))
            })
            .collect();
        for bench in &inventory.benches {
            let label = if inventory.local {
                format!("{} [local]", bench.id)
            } else {
                bench.id.clone()
            };
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
            println!("{label:<width$}  {status}");
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: Command,
    }

    #[test]
    fn benches_accepts_host_and_capability_filters() {
        assert!(
            Cli::try_parse_from(["benchd", "benches", "--host", "lab-a", "soc=esp32s3"]).is_ok()
        );
        assert!(Cli::try_parse_from(["benchd", "benches", "host=lab-a"]).is_ok());
    }

    #[test]
    fn filters_require_every_tag_and_apply_across_coordinators() {
        use benchd_core::wire::BenchView;
        let inventory = |name: &str| CoordinatorInventory {
            name: name.into(),
            local: false,
            benches: [
                ("match", vec!["host=lab-a", "soc=esp32s3"]),
                ("other-host", vec!["host=lab-ab", "soc=esp32s3"]),
                ("other-chip", vec!["host=lab-a", "soc=esp32c3"]),
                ("legacy", vec!["soc=esp32s3"]),
            ]
            .into_iter()
            .map(|(id, tags)| BenchView {
                id: id.into(),
                description: String::new(),
                has_docs: false,
                tags: tags.into_iter().map(String::from).collect(),
                resources: vec![],
            })
            .collect(),
            leases: vec![],
        };
        let mut inventories = vec![inventory("local"), inventory("remote")];
        filter_benches(&mut inventories, &[]);
        assert!(inventories.iter().all(|i| i.benches.len() == 4));
        filter_benches(
            &mut inventories,
            &["host=lab-a".into(), "soc=esp32s3".into()],
        );
        for inventory in &inventories {
            assert_eq!(inventory.benches.len(), 1);
            assert_eq!(inventory.benches[0].id, "match");
        }
        filter_benches(&mut inventories, &["host=missing".into()]);
        assert!(inventories.iter().all(|i| i.benches.is_empty()));
    }
}
