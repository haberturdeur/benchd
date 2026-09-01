//! benchd coordinator: the only component that listens (D5), and the only
//! writer of lease state (D6).
//!
//! Stateless across restarts by design. There is no database and no
//! reconciliation: on startup it knows nothing, hosts re-register, and any
//! lease that existed before is gone. That closes the orphaned-mount hole by
//! construction rather than by careful code.

mod conn;
mod handlers;
mod state;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use benchd_core::tags::Vocabulary;
use benchd_core::wire::DEFAULT_PORT;
use benchd_core::Limits;
use clap::Parser;
use tokio::sync::Mutex;

use crate::state::State;

#[derive(Parser)]
#[command(name = "benchd-coordinator", about = "benchd coordinator daemon")]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value_t = format!("0.0.0.0:{DEFAULT_PORT}"))]
    listen: String,

    /// Vocabulary and limits. Benches are *not* configured here: each host
    /// declares its own (D9).
    #[arg(long, default_value = "/etc/benchd/coordinator.toml")]
    config: String,

    /// How often to check for expiring leases.
    #[arg(long, default_value_t = 1)]
    tick_seconds: u64,
}

pub struct Shared {
    pub state: Mutex<State>,
    pub vocabulary: Vocabulary,
}

/// Coarse wall-clock seconds. The lease machine takes time as a parameter, so
/// this is the only place a clock is read.
pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "benchd_coordinator=info".into()),
        )
        .init();

    let args = Args::parse();

    let text = std::fs::read_to_string(&args.config)
        .with_context(|| format!("failed to read {}", args.config))?;
    let raw: toml::Value = toml::from_str(&text)
        .with_context(|| format!("failed to parse {}", args.config))?;

    let limits = raw
        .get("limits")
        .cloned()
        .map(|v| v.try_into::<Limits>())
        .transpose()
        .context("invalid [limits]")?
        .unwrap_or_default();

    // The vocabulary is loaded through the same path the inventory parser uses,
    // so a typo in the central config fails identically to a typo in a bench.
    let vocabulary = benchd_core::model::Inventory::from_toml_str(&text)
        .context("invalid vocabulary")?
        .vocabulary;

    tracing::info!(
        max_ttl = limits.max_ttl,
        max_total_hold = limits.max_total_hold,
        max_benches = limits.max_benches,
        grace = limits.grace,
        "limits"
    );

    let shared = Arc::new(Shared { state: Mutex::new(State::new(limits)), vocabulary });

    // The reaper. Effects are collected under the lock and dispatched after it
    // is released, so a slow peer can never stall expiry.
    {
        let shared = Arc::clone(&shared);
        let period = std::time::Duration::from_secs(args.tick_seconds.max(1));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            loop {
                ticker.tick().await;
                let outgoing = {
                    let mut state = shared.state.lock().await;
                    let effects = state.leases.tick(now());
                    state.dispatch(effects)
                };
                for msg in outgoing {
                    msg.send();
                }
            }
        });
    }

    let listener = conn::listen(&args.listen).await?;
    tracing::info!("ready");

    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!(?err, "accept failed");
                continue;
            }
        };
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            if let Err(err) = handlers::serve(shared, socket, peer).await {
                tracing::debug!(%peer, ?err, "connection ended");
            }
        });
    }
}
