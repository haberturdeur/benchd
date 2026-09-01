//! Behavioural tests for the lease lifecycle.
//!
//! Time is a parameter, so these run instantly and deterministically — there is
//! no sleeping and no clock to mock.

use std::collections::BTreeMap;

use benchd_core::lease::{
    ClaimError, Effect, EndReason, Epoch, LeaseEvent, LeaseManager, LeaseState, RevokeReason,
};
use benchd_core::limits::{LimitError, Limits};
use benchd_core::model::{ClaimRequest, Distinct, Inventory, Requirement};
use benchd_core::LeaseError;

const INVENTORY: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/inventory.toml"
));

fn manager(limits: Limits) -> LeaseManager {
    LeaseManager::new(Inventory::from_toml_str(INVENTORY).unwrap(), limits)
}

fn claim(tags: &[&str], ttl: u64) -> ClaimRequest {
    ClaimRequest {
        slots: [("dut".to_string(), Requirement::parse(tags.iter().copied()).unwrap())]
            .into_iter()
            .collect(),
        distinct: Distinct::All,
        ttl_seconds: ttl,
        reason: "test".into(),
    }
}

fn two_slot_claim(ttl: u64) -> ClaimRequest {
    ClaimRequest {
        slots: [
            ("dut".to_string(), Requirement::parse(["soc=esp32s3"]).unwrap()),
            ("peer".to_string(), Requirement::parse(["family=esp32"]).unwrap()),
        ]
        .into_iter()
        .collect(),
        distinct: Distinct::All,
        ttl_seconds: ttl,
        reason: "mesh".into(),
    }
}

// --- granting --------------------------------------------------------------

#[test]
fn a_claim_exports_on_the_host_before_materialising_on_the_client() {
    // The ordering rule: a client must never hold a node the host thinks is free.
    let mut m = manager(Limits::default());
    let s = m.register("agent-1");
    let g = m.claim(s, &claim(&["soc=esp32s3"], 600), 0).unwrap();

    let kinds: Vec<&str> = g
        .effects
        .iter()
        .map(|e| match e {
            Effect::Export { .. } => "export",
            Effect::Materialize { .. } => "materialize",
            Effect::Unmaterialize { .. } => "unmaterialize",
            Effect::Unexport { .. } => "unexport",
            Effect::Notify { .. } => "notify",
        })
        .collect();
    assert_eq!(kinds, vec!["export", "materialize", "notify"]);
    assert_eq!(g.expires_at, 600);
}

#[test]
fn a_held_bench_is_reported_as_contended_to_the_next_claimant() {
    let mut m = manager(Limits::default());
    let a = m.register("agent-1");
    let b = m.register("agent-2");

    // Take both S3 boards.
    m.claim(a, &claim(&["soc=esp32s3"], 600), 0).unwrap();
    m.claim(a, &claim(&["soc=esp32s3"], 600), 0).unwrap();

    let err = m.claim(b, &claim(&["soc=esp32s3"], 600), 10).unwrap_err();
    match err {
        ClaimError::NoMatch(no) => {
            assert!(!no.unsatisfiable(), "they exist, they're just busy");
            let rendered = no.to_string();
            assert!(rendered.contains("held by agent-1"), "{rendered}");
            assert!(rendered.contains("expires in 590s"), "{rendered}");
        }
        other => panic!("expected NoMatch, got {other:?}"),
    }
}

#[test]
fn each_grant_on_a_bench_gets_a_fresh_higher_epoch() {
    // Fencing: an instruction from a dead lease must be detectably stale.
    let mut m = manager(Limits::default());
    let s = m.register("agent-1");

    let first = m.claim(s, &claim(&["name=esp32s3-a"], 60), 0).unwrap();
    let e1 = *m.lease(first.lease).unwrap().epochs.get("esp32s3-a").unwrap();
    m.release(s, first.lease, 10).unwrap();

    let second = m.claim(s, &claim(&["name=esp32s3-a"], 60), 20).unwrap();
    let e2 = *m.lease(second.lease).unwrap().epochs.get("esp32s3-a").unwrap();

    assert!(e2 > e1, "epoch must increase: {e1:?} -> {e2:?}");
    assert_eq!(e1, Epoch(1));
    assert_eq!(e2, Epoch(2));
}

#[test]
fn the_bench_limit_counts_across_all_of_a_sessions_leases() {
    let mut m = manager(Limits { max_benches: 2, ..Default::default() });
    let s = m.register("agent-1");
    m.claim(s, &claim(&["family=esp32"], 60), 0).unwrap();
    m.claim(s, &claim(&["family=esp32"], 60), 0).unwrap();

    let err = m.claim(s, &claim(&["family=esp32"], 60), 0).unwrap_err();
    assert!(matches!(
        err,
        ClaimError::Limit(LimitError::TooManyBenches { max: 2, held: 2, wanted: 1 })
    ));
}

#[test]
fn a_multi_slot_claim_counts_as_its_number_of_benches() {
    let mut m = manager(Limits { max_benches: 2, ..Default::default() });
    let s = m.register("agent-1");
    let g = m.claim(s, &two_slot_claim(60), 0).unwrap();
    assert_eq!(g.assignment.len(), 2);

    // Budget is now exhausted by a single lease.
    assert!(m.claim(s, &claim(&["family=esp32"], 60), 0).is_err());
}

// --- renewal ---------------------------------------------------------------

#[test]
fn renewal_extends_from_now_and_is_trimmed_by_the_total_hold() {
    let mut m = manager(Limits { max_ttl: 600, max_total_hold: 900, ..Default::default() });
    let s = m.register("agent-1");
    let g = m.claim(s, &claim(&["soc=esp32s3"], 600), 0).unwrap();

    let r = m.renew(s, g.lease, 600, 500).unwrap();
    assert_eq!(r.ttl.granted, 400, "only 400s of the 900s budget remained");
    assert_eq!(r.expires_at, 900);
    assert!(r.ttl.was_clamped());
}

#[test]
fn renewal_is_refused_once_the_hold_budget_is_spent() {
    let mut m = manager(Limits { max_total_hold: 300, ..Default::default() });
    let s = m.register("agent-1");
    let g = m.claim(s, &claim(&["soc=esp32s3"], 300), 0).unwrap();
    assert!(matches!(
        m.renew(s, g.lease, 60, 300),
        Err(LeaseError::Limit(LimitError::HoldExhausted { .. }))
    ));
}

#[test]
fn another_session_cannot_renew_or_release_your_lease() {
    let mut m = manager(Limits::default());
    let a = m.register("agent-1");
    let b = m.register("agent-2");
    let g = m.claim(a, &claim(&["soc=esp32s3"], 600), 0).unwrap();

    assert_eq!(m.renew(b, g.lease, 60, 10), Err(LeaseError::NotYours));
    assert_eq!(m.release(b, g.lease, 10).unwrap_err(), LeaseError::NotYours);
}

// --- expiry ----------------------------------------------------------------

#[test]
fn a_lease_warns_inside_its_ttl_and_frees_the_bench_exactly_when_promised() {
    // An agent that asked for 600s gets 600s: the warning lands at T-grace and
    // teardown at T, so the ETA the matcher quoted stays true.
    let mut m = manager(Limits { grace: 30, ..Default::default() });
    let s = m.register("agent-1");
    let g = m.claim(s, &claim(&["soc=esp32s3"], 600), 0).unwrap();

    assert!(m.tick(500).is_empty(), "nothing due yet");

    let warn = m.tick(570);
    assert_eq!(
        warn,
        vec![Effect::Notify {
            session: s,
            event: LeaseEvent::Revoking {
                lease: g.lease,
                reason: RevokeReason::Expired,
                teardown_at: 600
            }
        }]
    );
    assert!(matches!(m.lease(g.lease).unwrap().state, LeaseState::Revoking { .. }));

    let gone = m.tick(600);
    assert!(m.lease(g.lease).is_none(), "bench is free at exactly T");
    assert!(matches!(gone[0], Effect::Unmaterialize { .. }), "client lets go first");
    assert!(matches!(gone[1], Effect::Unexport { .. }));
    assert!(gone.iter().any(|e| matches!(
        e,
        Effect::Notify { event: LeaseEvent::Ended { reason: EndReason::Expired, .. }, .. }
    )));
}

#[test]
fn noticing_the_warning_and_renewing_rescues_the_lease() {
    let mut m = manager(Limits { grace: 30, ..Default::default() });
    let s = m.register("agent-1");
    let g = m.claim(s, &claim(&["soc=esp32s3"], 600), 0).unwrap();

    m.tick(570);
    m.renew(s, g.lease, 300, 575).unwrap();

    assert!(matches!(m.lease(g.lease).unwrap().state, LeaseState::Held));
    assert!(m.tick(600).is_empty(), "no longer due");
    assert!(m.lease(g.lease).is_some());
}

// --- ending ----------------------------------------------------------------

#[test]
fn losing_the_session_releases_immediately_rather_than_waiting_out_the_ttl() {
    // Nobody is left to warn, so a grace period would only idle the hardware.
    let mut m = manager(Limits::default());
    let s = m.register("agent-1");
    let g = m.claim(s, &claim(&["soc=esp32s3"], 3600), 0).unwrap();

    let effects = m.end_session(s, 5);
    assert!(m.lease(g.lease).is_none());
    assert!(matches!(effects[0], Effect::Unmaterialize { .. }));
    assert!(matches!(effects[1], Effect::Unexport { .. }));

    // And the bench is immediately claimable by someone else.
    let other = m.register("agent-2");
    assert!(m.claim(other, &claim(&["name=esp32s3-b"], 60), 6).is_ok());
}

#[test]
fn an_operator_forced_release_is_graced_but_cannot_be_renewed_away() {
    let mut m = manager(Limits { grace: 30, ..Default::default() });
    let s = m.register("agent-1");
    let g = m.claim(s, &claim(&["soc=esp32s3"], 3600), 0).unwrap();

    let notice = m.force_release(g.lease, 100);
    assert_eq!(
        notice,
        vec![Effect::Notify {
            session: s,
            event: LeaseEvent::Revoking {
                lease: g.lease,
                reason: RevokeReason::Forced,
                teardown_at: 130
            }
        }]
    );

    // The holder gets a window to park the board, but cannot cling on.
    assert_eq!(m.renew(s, g.lease, 600, 105), Err(LeaseError::ForciblyRevoked));
    assert!(m.lease(g.lease).is_some(), "still held during grace");

    let gone = m.tick(130);
    assert!(m.lease(g.lease).is_none());
    assert!(gone.iter().any(|e| matches!(
        e,
        Effect::Notify { event: LeaseEvent::Ended { reason: EndReason::Forced, .. }, .. }
    )));
}

#[test]
fn releasing_is_immediate_because_the_holder_asked() {
    let mut m = manager(Limits::default());
    let s = m.register("agent-1");
    let g = m.claim(s, &claim(&["soc=esp32s3"], 600), 0).unwrap();

    let effects = m.release(s, g.lease, 10).unwrap();
    assert!(m.lease(g.lease).is_none());
    assert!(effects.iter().any(|e| matches!(
        e,
        Effect::Notify { event: LeaseEvent::Ended { reason: EndReason::Released, .. }, .. }
    )));
    assert!(m.busy(10).is_empty());
}

#[test]
fn busy_reports_the_holders_name_and_time_remaining() {
    let mut m = manager(Limits::default());
    let s = m.register("agent-7");
    m.claim(s, &claim(&["name=esp32s3-a"], 600), 100).unwrap();

    let busy: BTreeMap<_, _> = m.busy(400);
    let info = &busy["esp32s3-a"];
    assert_eq!(info.owner, "agent-7");
    assert_eq!(info.expires_in, Some(300.0));
    assert_eq!(info.reason, "test");
}

// --- regressions -----------------------------------------------------------

#[test]
fn a_bench_can_leave_the_inventory_and_its_lease_goes_with_it() {
    // A host disconnecting or losing a device is not repaired in place: the
    // bench leaves and the matcher routes around it.
    let mut m = manager(Limits::default());
    let s = m.register("agent-1");
    let g = m.claim(s, &claim(&["name=esp32s3-a"], 600), 0).unwrap();

    let effects = m.drop_lease(g.lease);
    assert!(m.lease(g.lease).is_none());
    assert!(matches!(effects[0], Effect::Unmaterialize { .. }));
    assert!(m.busy(1).is_empty(), "the bench is free for whoever is left");
}
