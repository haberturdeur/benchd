//! `benchd` — every component, behind one command.
//!
//! The coordinator, the host, the client daemon, the MCP shim, the hand-held
//! lease and the operator commands are one executable with six entry points.
//! They were six binaries; a lab is several machines running different subsets
//! of them, and keeping six versions in step across those machines is the part
//! that goes wrong. One artefact cannot be half-upgraded.
//!
//! It does *not* merge the surfaces. An agent is still handed five MCP tools
//! and no way to name a bench (D17). That boundary was never the executable —
//! nothing ever stopped an agent from running the operator CLI — so putting
//! them in one file costs nothing that was actually being defended (D25).
//!
//! This file owns the two things the subcommands must not each decide for
//! themselves: the tokio runtime, and where logs go.

use std::process::ExitCode;

use benchd_core::wire::DEFAULT_PORT;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "benchd",
    version,
    about = "Lease-based hardware bench broker",
    subcommand_required = true,
    arg_required_else_help = true
)]
struct Args {
    /// Which coordinator the operator commands should ask.
    ///
    /// Only `benches`, `leases` and `release` read this. The daemons take their
    /// own `--coordinator`, after the subcommand, because a host dialling one
    /// and an operator inspecting one are different questions that happen to
    /// share a word.
    #[arg(long, default_value_t = format!("127.0.0.1:{DEFAULT_PORT}"), env = "BENCHD_COORDINATOR")]
    coordinator: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the coordinator: the only component that listens.
    Coordinator(benchd_coordinator::CoordinatorArgs),

    /// Run a host, which owns the hardware of exactly one bench.
    Host(benchd_host::HostArgs),

    /// Run the client daemon, which materialises device nodes on this machine.
    Client(benchd_client::ClientArgs),

    /// Serve MCP on stdio, for one agent.
    Mcp(benchd_mcp::McpArgs),

    /// Hold a bench by hand.
    Lease(benchd_client::lease::LeaseArgs),

    /// `benches`, `leases`, `release` — flattened in, so they read as the
    /// top-level commands they have always been.
    #[command(flatten)]
    Operator(benchd_coordinator::operator::Command),
}

/// Send logs to stderr, always.
///
/// `benchd mcp` has no choice — stdout carries JSON-RPC and a stray log line
/// ends the session — and under systemd both streams land in the journal
/// anyway, so there is nothing to gain by treating the daemons differently.
///
/// The default filter names the crate the subcommand lives in, which is what it
/// named before the merge: `RUST_LOG=benchd_host=debug` still selects the host.
fn logging(default: &str) {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default.into()),
        )
        .init();
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    // The one-shot commands stay quiet. They print an answer and exit, and a
    // log line in front of it is noise on a terminal rather than context.
    match &args.command {
        Command::Coordinator(_) => logging("benchd_coordinator=info"),
        Command::Host(_) => logging("benchd_host=info"),
        Command::Client(_) => logging("benchd_client=info"),
        Command::Mcp(_) => logging("benchd_mcp=info"),
        Command::Lease(_) | Command::Operator(_) => {}
    }

    let result = match args.command {
        Command::Coordinator(args) => benchd_coordinator::run(args).await,
        Command::Host(args) => benchd_host::run(args).await,
        Command::Client(args) => benchd_client::run(args).await,
        Command::Mcp(args) => benchd_mcp::run(args).await,
        Command::Operator(command) => {
            benchd_coordinator::operator::run(&args.coordinator, command).await
        }

        // The only subcommand with an exit status of its own: it reports
        // whether the hold ended the way you asked, and wraps a command whose
        // status has to survive.
        Command::Lease(args) => return benchd_client::lease::run(args).await,
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err:#}");
            ExitCode::FAILURE
        }
    }
}
