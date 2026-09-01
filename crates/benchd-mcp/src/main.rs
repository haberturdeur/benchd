//! benchd MCP shim: the agent-facing tool surface.
//!
//! Five tools, deliberately. Tool surface *is* policy, and with no roles it is
//! the only policy lever left (D17): if a console-read tool existed here, agents
//! would use it instead of `idf.py monitor` and you would get two access paths
//! with split logs. If claim-by-name existed, an agent would hardcode a bench
//! into a test script and reintroduce the contention this system removes. Both
//! live in the operator CLI, which is a different program.
//!
//! Unprivileged. One process per agent, because MCP over stdio is a pipe pair
//! and the harness spawns the server (D8). All privilege lives in the client
//! daemon on the other end of the unix socket.

mod daemon;

use std::collections::BTreeMap;
use std::sync::Arc;

use benchd_core::wire::{ClaimSpec, ClientMsg, RequestId};
use clap::Parser;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ErrorData};
use rmcp::{tool, tool_router, ServiceExt};
use serde::Deserialize;

use crate::daemon::Daemon;

#[derive(Parser)]
#[command(name = "benchd-mcp", about = "benchd MCP server (one per agent)")]
struct Args {
    /// The local client daemon's socket.
    #[arg(long, default_value = "/run/benchd/agent.sock", env = "BENCHD_SOCKET")]
    socket: String,

    /// This agent's name. A diagnostic label, not an authorisation input: it
    /// exists so contention reports can say "held by agent-3" rather than
    /// quoting a UUID (D19).
    #[arg(long, env = "BENCHD_IDENTITY", default_value = "agent")]
    identity: String,
}

#[derive(Clone)]
struct Benchd {
    daemon: Arc<Daemon>,
    // Read by the generated ServerHandler, not by anything we write.
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::tool::ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ClaimArgs {
    /// What each slot needs, as `key=value` capability tags. One slot is the
    /// common case: `{"dut": ["soc=esp32s3"]}`. Ask for two when you need two
    /// boards that can talk to each other, e.g. `{"dut": [...], "peer": [...]}`
    /// — they are granted together or not at all.
    slots: BTreeMap<String, Vec<String>>,
    /// How long you need the hardware, in seconds. Required: there is no
    /// default. Ask for what the work needs; you can always `renew`.
    ttl_seconds: u64,
    /// What you are doing, e.g. "wifi reconnect regression". Shown to whoever
    /// is waiting for the bench.
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct LeaseArgs {
    /// The lease id returned by `claim`.
    lease: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RenewArgs {
    lease: u64,
    /// Extra seconds to add, counted from now.
    extra_seconds: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct NoArgs {}

#[tool_router(server_handler)]
impl Benchd {
    #[tool(
        name = "tag_list",
        description = "List the capability tags benches can be claimed by, with \
                       how many benches have each and how many are free right now. \
                       Call this first: it tells you what exists and what is \
                       available, so you can relax a request before claiming."
    )]
    async fn tag_list(&self, _: Parameters<NoArgs>) -> Result<CallToolResult, ErrorData> {
        let value = self
            .daemon
            .simple(ClientMsg::TagList { request: RequestId(0) })
            .await
            .map_err(internal)?;
        let tags = value.get("tags").cloned().unwrap_or_default();
        Ok(text(&serde_json::to_string_pretty(&tags).unwrap_or_default()))
    }

    #[tool(
        name = "claim",
        description = "Claim hardware by capability for a bounded time. Returns a \
                       lease id and a real device path per resource — use it with \
                       your normal tools (esptool, idf.py monitor, minicom). \
                       Never touch /dev/tty* directly: without a lease it is not \
                       yours, and the path you get is only valid for this lease. \
                       Release when done."
    )]
    async fn claim(
        &self,
        Parameters(args): Parameters<ClaimArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let spec = ClaimSpec {
            slots: args.slots,
            ttl: args.ttl_seconds,
            reason: args.reason,
            distinct: true,
        };
        let value = self.daemon.claim(spec).await.map_err(internal)?;

        // Hand back the paths *and* the env-var names, because agents are
        // reliably good at using $LAB_DUT_CONSOLE and reliably bad at telling
        // two identical-looking device nodes apart.
        let mut lines = Vec::new();
        lines.push(format!(
            "lease {} — expires at {} (unix seconds)",
            value.get("lease").and_then(|v| v.as_u64()).unwrap_or(0),
            value.get("expires_at").and_then(|v| v.as_u64()).unwrap_or(0),
        ));
        if let Some(note) = value.get("note").and_then(|v| v.as_str()) {
            lines.push(format!("note: {note}"));
        }
        if let Some(slots) = value.get("slots").and_then(|v| v.as_object()) {
            for (slot, resources) in slots {
                if let Some(map) = resources.as_object() {
                    for (name, path) in map {
                        lines.push(format!(
                            "  {} = {}",
                            benchd_core::wire::env_var(slot, name),
                            path.as_str().unwrap_or_default()
                        ));
                    }
                }
            }
        }
        lines.push(String::new());
        lines.push(
            "If a device later gives ENOENT or EIO, your lease ended — claim again. \
             The board is not broken; do not power-cycle it."
                .into(),
        );
        Ok(text(&lines.join("\n")))
    }

    #[tool(
        name = "renew",
        description = "Extend a lease you hold. Renewal is explicit on purpose: it \
                       proves you are still working, so hardware is never held by \
                       an agent that has moved on. Call it when you get a revoking \
                       notice and still need the board."
    )]
    async fn renew(
        &self,
        Parameters(args): Parameters<RenewArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let value = self
            .daemon
            .simple(ClientMsg::Renew {
                request: RequestId(0),
                session: benchd_core::wire::SessionToken(String::new()),
                lease: benchd_core::lease::LeaseId(args.lease),
                extra: args.extra_seconds,
            })
            .await
            .map_err(internal)?;
        Ok(text(&format!(
            "lease {} now expires at {}",
            args.lease,
            value.get("expires_at").and_then(|v| v.as_u64()).unwrap_or(0)
        )))
    }

    #[tool(
        name = "release",
        description = "Give a lease back as soon as you are done. Someone may be \
                       waiting, and releasing early is free."
    )]
    async fn release(
        &self,
        Parameters(args): Parameters<LeaseArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.daemon
            .simple(ClientMsg::Release {
                request: RequestId(0),
                session: benchd_core::wire::SessionToken(String::new()),
                lease: benchd_core::lease::LeaseId(args.lease),
            })
            .await
            .map_err(internal)?;
        Ok(text(&format!("released lease {}", args.lease)))
    }

    #[tool(
        name = "lease_status",
        description = "Show the leases you hold and how long each has left. Use it \
                       to check remaining time before starting something slow."
    )]
    async fn lease_status(&self, _: Parameters<NoArgs>) -> Result<CallToolResult, ErrorData> {
        let value = self
            .daemon
            .simple(ClientMsg::Status {
                request: RequestId(0),
                session: benchd_core::wire::SessionToken(String::new()),
            })
            .await
            .map_err(internal)?;
        let leases = value.get("leases").cloned().unwrap_or_default();
        if leases.as_array().map(|a| a.is_empty()).unwrap_or(true) {
            return Ok(text("no leases held"));
        }
        Ok(text(&serde_json::to_string_pretty(&leases).unwrap_or_default()))
    }
}

fn text(body: &str) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(body)])
}

fn internal(err: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(err.to_string(), None)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stderr, never stdout: stdout is the MCP transport.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "benchd_mcp=info".into()),
        )
        .init();

    let args = Args::parse();
    let daemon = Daemon::connect(&args.socket, &args.identity).await?;
    tracing::info!(identity = %args.identity, "registered");

    let service = Benchd { daemon, tool_router: Benchd::tool_router() }
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
