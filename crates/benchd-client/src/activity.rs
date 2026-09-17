//! Extend a lease when its imported device is actually transferring.
//!
//! A client-daemon heartbeat would keep a forgotten claim alive for as long as
//! the MCP shim stayed connected. USB traffic is a different signal: something
//! on this machine is talking to the board. The coordinator still sees an
//! ordinary `Renew`, so `max_total_hold` and operator revoke apply unchanged
//! (D15).

use std::sync::Arc;
use std::time::Duration;

use benchd_core::wire::ClientMsg;

use crate::Shared;

const POLL: Duration = Duration::from_secs(5);

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether this poll should send a `Renew`.
///
/// Half the last grant remaining is the DHCP-style threshold: continuous use
/// slides the window forward; a claim that went quiet after the first minute
/// still dies at the original expiry.
pub fn should_extend(
    now: u64,
    expires_at: u64,
    extra: u64,
    urbs_grew: bool,
    in_flight: bool,
) -> bool {
    if !urbs_grew || in_flight || extra == 0 || now >= expires_at {
        return false;
    }
    let remaining = expires_at - now;
    remaining <= extra.div_ceil(2)
}

pub async fn watch(shared: Arc<Shared>) {
    let mut ticker = tokio::time::interval(POLL);
    loop {
        ticker.tick().await;
        tick(&shared).await;
    }
}

async fn tick(shared: &Arc<Shared>) {
    let urbs = {
        let materializer = shared.materializer.lock().await;
        materializer.urb_totals().await
    };
    let due = shared.agents.due_extensions(&urbs, unix_now()).await;
    for (lease, session, extra) in due {
        let Some(agent_id) = shared.agents.holder_of(lease).await else {
            continue;
        };
        let request = shared.agents.track_auto_renew(agent_id, lease).await;
        tracing::info!(%lease, extra, "extending a lease whose device is in use");
        shared
            .send(
                lease.coordinator,
                &ClientMsg::Renew {
                    request,
                    session,
                    lease: lease.lease,
                    extra,
                },
            )
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::should_extend;

    #[test]
    fn idle_traffic_does_not_extend() {
        assert!(!should_extend(800, 1000, 400, false, false));
    }

    #[test]
    fn use_in_the_first_half_leaves_the_original_expiry() {
        assert!(!should_extend(100, 1000, 900, true, false));
    }

    #[test]
    fn use_in_the_second_half_extends() {
        assert!(should_extend(600, 1000, 900, true, false));
    }

    #[test]
    fn an_in_flight_renew_is_not_stacked() {
        assert!(!should_extend(600, 1000, 900, true, true));
    }

    #[test]
    fn a_spent_budget_stops_trying() {
        assert!(!should_extend(600, 1000, 0, true, false));
    }

    #[test]
    fn a_lease_that_has_already_ended_is_not_rescued() {
        assert!(!should_extend(1000, 1000, 900, true, false));
    }
}
