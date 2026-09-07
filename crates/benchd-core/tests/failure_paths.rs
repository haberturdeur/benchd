//! Failure-path tests.
//!
//! Written after an adversarial review found that every serious bug in benchd
//! lived on a path no test exercised: what happens when an executor *fails*,
//! when an agent supplies a hostile name, when a component restarts underneath
//! a live lease. The happy path had good coverage and told us nothing.
//!
//! The rule these encode: **a lease means the hardware is yours.** Anything that
//! breaks that — a lease held for hardware that was never set up, a device node
//! that outlives its lease, a name that escapes its directory — is a bug here
//! even if nothing panics.

use std::collections::BTreeMap;

use benchd_core::lease::{Effect, LeaseManager, LeaseState, RevokeReason};
use benchd_core::limits::Limits;
use benchd_core::model::{valid_component, ClaimRequest, Distinct, Inventory, Requirement};

const INVENTORY: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/inventory.toml"
));

fn manager(limits: Limits) -> LeaseManager {
    LeaseManager::new(Inventory::from_toml_str(INVENTORY).unwrap(), limits)
}

fn claim_named(slot: &str, tags: &[&str], ttl: u64) -> ClaimRequest {
    ClaimRequest {
        slots: [(
            slot.to_string(),
            Requirement::parse(tags.iter().copied()).unwrap(),
        )]
        .into_iter()
        .collect(),
        distinct: Distinct::All,
        ttl_seconds: ttl,
        reason: "test".into(),
    }
}

// --- hostile names ---------------------------------------------------------
//
// A slot name is an arbitrary key from an agent's tool call and becomes a path
// component inside a process running as root. These are the inputs that turned
// benchd into an arbitrary-location `mount --bind`.

#[test]
fn a_slot_name_cannot_escape_its_directory() {
    for hostile in [
        "../../../../tmp/pwned",
        "..",
        ".",
        "/dev",
        "/etc/passwd",
        "a/b",
        "a\\b",
        "",
        "with space",
        "semi;colon",
        "new\nline",
        "nul\0byte",
    ] {
        assert!(
            !valid_component(hostile),
            "{hostile:?} must be rejected: it becomes a path component in a root daemon"
        );
        let request = claim_named(hostile, &["soc=esp32s3"], 60);
        assert!(
            request.validate().is_err(),
            "a claim with slot {hostile:?} must be refused"
        );
    }
}

#[test]
fn ordinary_slot_names_still_work() {
    for ok in ["dut", "peer", "node_a", "node-b", "usb.0", "A1"] {
        assert!(valid_component(ok), "{ok:?} should be allowed");
        assert!(claim_named(ok, &["soc=esp32s3"], 60).validate().is_ok());
    }
}

#[test]
fn a_claim_must_name_at_least_one_slot() {
    let request = ClaimRequest {
        slots: BTreeMap::new(),
        distinct: Distinct::All,
        ttl_seconds: 60,
        reason: String::new(),
    };
    assert!(request.validate().is_err());
}

// --- a lease must never outlive its teardown -------------------------------

#[test]
fn every_way_a_lease_can_end_tears_down_in_the_same_order() {
    // The client must let go before the host does, on *every* path. A path that
    // forgets one leaves a device node live while the bench is marked free —
    // which is two agents on one board.
    let ttl = 600;

    let check = |label: &str, effects: Vec<Effect>| {
        let unmat = effects
            .iter()
            .position(|e| matches!(e, Effect::Unmaterialize { .. }))
            .unwrap_or_else(|| panic!("{label}: no Unmaterialize"));
        let unexp = effects
            .iter()
            .position(|e| matches!(e, Effect::Unexport { .. }))
            .unwrap_or_else(|| panic!("{label}: no Unexport"));
        assert!(unmat < unexp, "{label}: client must let go before the host");
    };

    // 1. voluntary release
    let mut m = manager(Limits::default());
    let s = m.register("agent");
    let g = m
        .claim(s, &claim_named("dut", &["soc=esp32s3"], ttl), 0)
        .unwrap();
    check("release", m.release(s, g.lease, 1).unwrap());

    // 2. the session went away
    let mut m = manager(Limits::default());
    let s = m.register("agent");
    m.claim(s, &claim_named("dut", &["soc=esp32s3"], ttl), 0)
        .unwrap();
    check("end_session", m.end_session(s, 1));

    // 3. expiry
    let mut m = manager(Limits {
        grace: 30,
        ..Default::default()
    });
    let s = m.register("agent");
    m.claim(s, &claim_named("dut", &["soc=esp32s3"], ttl), 0)
        .unwrap();
    m.tick(ttl - 30);
    check("expiry", m.tick(ttl));

    // 4. the hardware disappeared
    let mut m = manager(Limits::default());
    let s = m.register("agent");
    let g = m
        .claim(s, &claim_named("dut", &["soc=esp32s3"], ttl), 0)
        .unwrap();
    check("drop_lease", m.drop_lease(g.lease));

    // 5. an operator took it back
    let mut m = manager(Limits {
        grace: 30,
        ..Default::default()
    });
    let s = m.register("agent");
    let g = m
        .claim(s, &claim_named("dut", &["soc=esp32s3"], ttl), 0)
        .unwrap();
    m.force_release(g.lease, 100);
    check("force_release", m.tick(130));
}

#[test]
fn a_bench_is_free_only_once_its_lease_is_actually_gone() {
    let mut m = manager(Limits {
        grace: 30,
        ..Default::default()
    });
    let s = m.register("agent");
    let g = m
        .claim(s, &claim_named("dut", &["name=esp32s3-a"], 600), 0)
        .unwrap();
    let bench = g.assignment["dut"].clone();

    assert!(m.busy(0).contains_key(&bench));
    m.tick(570); // Revoking: still held, still not claimable by anyone else
    assert!(
        m.busy(570).contains_key(&bench),
        "a revoking lease still holds its bench"
    );
    m.tick(600);
    assert!(
        !m.busy(600).contains_key(&bench),
        "and frees it exactly when it ends"
    );
}

// --- expiry under an unreliable clock --------------------------------------

#[test]
fn a_late_tick_still_ends_the_lease() {
    // The reaper can be delayed by a busy coordinator or a suspended laptop.
    // Skipping the warning is acceptable; failing to end the lease is not.
    let mut m = manager(Limits {
        grace: 30,
        ..Default::default()
    });
    let s = m.register("agent");
    let g = m
        .claim(s, &claim_named("dut", &["soc=esp32s3"], 600), 0)
        .unwrap();

    let effects = m.tick(10_000); // woke up hours late
    assert!(
        m.lease(g.lease).is_none(),
        "an overdue lease must not survive"
    );
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::Unmaterialize { .. })));
    assert!(effects.iter().any(|e| matches!(e, Effect::Unexport { .. })));
}

#[test]
fn a_lease_shorter_than_the_grace_window_is_not_born_revoking() {
    // grace 30s, ttl 10s. Naively warning at expires_at - grace puts the lease
    // in Revoking before it is ever usable.
    let mut m = manager(Limits {
        grace: 30,
        ..Default::default()
    });
    let s = m.register("agent");
    let g = m
        .claim(s, &claim_named("dut", &["soc=esp32s3"], 10), 0)
        .unwrap();

    m.tick(0);
    assert!(
        matches!(m.lease(g.lease).unwrap().state, LeaseState::Held),
        "a fresh lease must be Held, whatever the grace window is"
    );
    m.tick(10);
    assert!(m.lease(g.lease).is_none(), "and it still ends on time");
}

#[test]
fn an_operator_release_never_extends_a_lease() {
    // Taking a bench back must not hand the holder extra time, even if the
    // grace window would reach past the original expiry.
    let mut m = manager(Limits {
        grace: 300,
        ..Default::default()
    });
    let s = m.register("agent");
    let g = m
        .claim(s, &claim_named("dut", &["soc=esp32s3"], 60), 0)
        .unwrap();

    m.force_release(g.lease, 50);
    let LeaseState::Revoking {
        teardown_at,
        reason,
    } = m.lease(g.lease).unwrap().state
    else {
        panic!("expected Revoking");
    };
    assert_eq!(reason, RevokeReason::Forced);
    assert!(
        teardown_at <= 60,
        "teardown at {teardown_at} is past the original expiry of 60"
    );
}

// --- renewal cannot be used to hold hardware forever ------------------------

#[test]
fn repeated_renewal_cannot_exceed_the_total_hold_budget() {
    let limits = Limits {
        max_ttl: 60,
        max_total_hold: 200,
        grace: 5,
        ..Default::default()
    };
    let mut m = manager(limits);
    let s = m.register("agent");
    let g = m
        .claim(s, &claim_named("dut", &["soc=esp32s3"], 60), 0)
        .unwrap();

    // Renew as aggressively as the rules allow.
    let mut now = 0;
    for _ in 0..100 {
        now += 10;
        if m.renew(s, g.lease, 60, now).is_err() {
            break;
        }
    }
    let expires = m.lease(g.lease).map(|l| l.expires_at).unwrap_or(0);
    assert!(
        expires <= limits.max_total_hold,
        "lease runs to {expires}s, past the {}s total-hold budget",
        limits.max_total_hold
    );
}

// --- fencing ---------------------------------------------------------------

#[test]
fn epochs_are_per_bench_not_per_lease() {
    // A multi-slot claim can hold a well-used bench and a fresh one at the same
    // time. Collapsing their epochs into one made the two ends of a relayed
    // claim derive different channel keys, so the relay could never pair them.
    let mut m = manager(Limits {
        max_benches: 4,
        ..Default::default()
    });
    let s = m.register("agent");

    // Use up esp32s3-a a few times so its counter runs ahead.
    for _ in 0..3 {
        let g = m
            .claim(s, &claim_named("dut", &["psram=octal"], 60), 0)
            .unwrap();
        m.release(s, g.lease, 1).unwrap();
    }

    let request = ClaimRequest {
        slots: [
            (
                "dut".to_string(),
                Requirement::parse(["psram=octal"]).unwrap(),
            ),
            (
                "peer".to_string(),
                Requirement::parse(["console=uart"]).unwrap(),
            ),
        ]
        .into_iter()
        .collect(),
        distinct: Distinct::All,
        ttl_seconds: 60,
        reason: "mesh".into(),
    };
    let g = m.claim(s, &request, 2).unwrap();
    let lease = m.lease(g.lease).unwrap();

    let epochs: Vec<u64> = lease.epochs.values().map(|e| e.0).collect();
    assert_eq!(epochs.len(), 2, "one epoch per bench");
    assert_ne!(
        epochs[0], epochs[1],
        "these benches have different histories, so their epochs must differ: {epochs:?}"
    );
}

#[test]
fn an_epoch_never_goes_backwards_for_a_bench() {
    let mut m = manager(Limits::default());
    let s = m.register("agent");
    let mut last = 0;
    for _ in 0..5 {
        let g = m
            .claim(s, &claim_named("dut", &["psram=octal"], 60), 0)
            .unwrap();
        let epoch = m.lease(g.lease).unwrap().epochs.values().next().unwrap().0;
        assert!(epoch > last, "epoch went {last} -> {epoch}");
        last = epoch;
        m.release(s, g.lease, 1).unwrap();
    }
}

#[test]
fn teardown_carries_the_epoch_it_was_created_at() {
    // Teardown effects are produced after the lease is removed, so anything
    // that looks the epoch up afterwards sees 0 — which an executor fencing on
    // epoch rejects as stale, leaving the device node live for a lease that no
    // longer exists. The epoch has to travel with the effect.
    fn unmaterialize_epoch(effects: &[Effect], label: &str) -> benchd_core::lease::Epoch {
        effects
            .iter()
            .find_map(|e| match e {
                Effect::Unmaterialize { epoch, .. } => Some(*epoch),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{label}: no Unmaterialize"))
    }

    for (label, end) in [
        ("release", 0usize),
        ("drop_lease", 1),
        ("end_session", 2),
        ("expiry", 3),
    ] {
        let mut m = manager(Limits {
            grace: 5,
            ..Default::default()
        });
        let s = m.register("agent");
        let g = m
            .claim(s, &claim_named("dut", &["psram=octal"], 60), 0)
            .unwrap();
        let granted = *m.lease(g.lease).unwrap().epochs.values().next().unwrap();

        let effects = match end {
            0 => m.release(s, g.lease, 1).unwrap(),
            1 => m.drop_lease(g.lease),
            2 => m.end_session(s, 1),
            _ => {
                m.tick(55);
                m.tick(60)
            }
        };
        assert_eq!(
            unmaterialize_epoch(&effects, label),
            granted,
            "{label}: teardown must carry the lease's epoch, not 0"
        );
    }
}

// --- what agents are told to do about a failure ----------------------------

#[test]
fn a_distinctness_conflict_is_never_reported_as_worth_retrying() {
    // Two slots, one matching bench, and it is FREE. Nothing is busy, so no
    // amount of waiting can satisfy this. Reporting it as contended made agents
    // retry-spin at full speed - the exact failure D14 exists to prevent.
    let inv = Inventory::from_toml_str(INVENTORY).unwrap();
    let request = ClaimRequest {
        slots: [
            (
                "dut".to_string(),
                Requirement::parse(["psram=octal"]).unwrap(),
            ),
            (
                "peer".to_string(),
                Requirement::parse(["psram=octal"]).unwrap(),
            ),
        ]
        .into_iter()
        .collect(),
        distinct: Distinct::All,
        ttl_seconds: 60,
        reason: String::new(),
    };
    let err = benchd_core::allocate_in(&inv, &request, &Default::default()).unwrap_err();

    assert!(err.conflict_only, "only one bench has psram=octal");
    assert!(
        err.unsatisfiable(),
        "a distinctness conflict against a FREE bench must not be advertised as retryable"
    );
    let rendered = err.to_string();
    assert!(rendered.contains("Waiting will not help"), "{rendered}");
}

#[test]
fn sharing_one_bench_between_slots_grants_one_epoch_and_one_export() {
    // With distinct=false two slots may land on the same bench. Iterating the
    // assignment directly bumped that bench's epoch twice and emitted two
    // Exports; the host treats the second as already active, so it serves the
    // first channel key while the client is handed the second, and a relayed
    // claim can never pair.
    let mut m = manager(Limits::default());
    let s = m.register("agent");
    let request = ClaimRequest {
        slots: [
            (
                "a".to_string(),
                Requirement::parse(["psram=octal"]).unwrap(),
            ),
            (
                "b".to_string(),
                Requirement::parse(["psram=octal"]).unwrap(),
            ),
        ]
        .into_iter()
        .collect(),
        distinct: Distinct::None,
        ttl_seconds: 60,
        reason: String::new(),
    };
    let g = m.claim(s, &request, 0).unwrap();

    assert_eq!(
        g.assignment["a"], g.assignment["b"],
        "both slots share the bench"
    );
    let exports = g
        .effects
        .iter()
        .filter(|e| matches!(e, Effect::Export { .. }))
        .count();
    assert_eq!(exports, 1, "one bench in one claim must be exported once");
    assert_eq!(
        m.lease(g.lease).unwrap().epochs.len(),
        1,
        "and must hold exactly one epoch"
    );
    assert_eq!(
        m.lease(g.lease).unwrap().epochs.values().next().unwrap().0,
        1,
        "which must not have been bumped twice"
    );
}

#[test]
fn a_revoked_lease_reports_when_the_bench_actually_frees() {
    // An operator taking a one-hour lease back frees the bench in `grace`
    // seconds, not an hour. Quoting the nominal expiry told the next claimant
    // to wait a hundred times too long.
    let mut m = manager(Limits {
        grace: 30,
        ..Default::default()
    });
    let s = m.register("agent-1");
    let g = m
        .claim(s, &claim_named("dut", &["psram=octal"], 3600), 0)
        .unwrap();
    let bench = g.assignment["dut"].clone();

    m.force_release(g.lease, 100);
    let busy = m.busy(101);
    let eta = busy[&bench].expires_in.unwrap();
    assert!(eta <= 30.0, "bench frees at t=130 but the ETA says {eta}s");
}
