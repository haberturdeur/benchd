use benchd_core::lease::{ClaimError, LeaseManager};
use benchd_core::limits::Limits;
use benchd_core::model::{ClaimRequest, Distinct, Grouping, Inventory, Requirement};
use std::collections::BTreeMap;

const LAB: &str = r#"
open_keys = ["name", "device"]
[benches.a1]
group = "a"
tags = ["device=esp32", "host=one"]
[benches.a2]
group = "a"
tags = ["device=esp32", "host=two"]
[benches.aw]
group = "a"
tags = ["device=wifi"]
[benches.ab]
group = "a"
tags = ["device=bt"]
[benches.b1]
group = "b"
tags = ["device=esp32"]
[benches.b2]
group = "b"
tags = ["device=esp32"]
[benches.bw]
group = "b"
tags = ["device=wifi"]
[benches.bb]
group = "b"
tags = ["device=bt"]
[benches.ungrouped]
tags = ["device=esp32"]
"#;
fn manager() -> LeaseManager {
    LeaseManager::new(
        Inventory::from_toml_str(LAB).unwrap(),
        Limits {
            max_benches: 16,
            ..Limits::default()
        },
    )
}
fn request(mode: Grouping, tags: &[&[&str]]) -> ClaimRequest {
    ClaimRequest {
        grouping: mode,
        slots: tags
            .iter()
            .enumerate()
            .map(|(i, t)| {
                (
                    format!("slot{i}"),
                    Requirement::parse(t.iter().copied()).unwrap(),
                )
            })
            .collect::<BTreeMap<_, _>>(),
        distinct: Distinct::All,
        ttl_seconds: 60,
        reason: "group regression".into(),
    }
}
fn no_match(error: ClaimError) -> benchd_core::matcher::NoMatch {
    match error {
        ClaimError::NoMatch(e) => e,
        other => panic!("{other}"),
    }
}
#[test]
fn membership_is_optional_and_validated() {
    let inv = Inventory::from_toml_str(LAB).unwrap();
    assert_eq!(inv.benches["a1"].group.as_deref(), Some("a"));
    assert!(inv.benches["ungrouped"].group.is_none());
    assert!(Inventory::from_toml_str(&LAB.replace("group = \"a\"", "group = \"../a\"")).is_err());
}
#[test]
fn four_roles_are_selected_from_one_group_across_hosts() {
    let mut m = manager();
    let s = m.register("test");
    let grant = m
        .claim(
            s,
            &request(
                Grouping::Same,
                &[
                    &["device=esp32"],
                    &["device=esp32"],
                    &["device=wifi"],
                    &["device=bt"],
                ],
            ),
            0,
        )
        .unwrap();
    let groups: std::collections::BTreeSet<_> = grant
        .assignment
        .values()
        .map(|id| m.inventory().benches[id].group.clone())
        .collect();
    assert_eq!(groups.len(), 1);
    assert!(!groups.contains(&None));
    assert_eq!(
        grant
            .assignment
            .values()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        4
    );
}
#[test]
fn cross_group_only_assignment_is_impossible_and_reserves_nothing() {
    let mut m = manager();
    let s = m.register("test");
    let error = m
        .claim(
            s,
            &request(Grouping::Same, &[&["name=a1"], &["name=bw"]]),
            0,
        )
        .unwrap_err();
    assert!(no_match(error).unsatisfiable());
    assert_eq!(m.leases().count(), 0);
}
#[test]
fn normal_claims_can_share_a_group_but_exclusive_claims_block_unused_members() {
    let mut m = manager();
    let a = m.register("a");
    let b = m.register("b");
    let first = m
        .claim(a, &request(Grouping::Same, &[&["name=a1"]]), 0)
        .unwrap();
    let second = m
        .claim(b, &request(Grouping::None, &[&["name=aw"]]), 0)
        .unwrap();
    assert!(!no_match(
        m.claim(a, &request(Grouping::Exclusive, &[&["name=a2"]]), 0)
            .unwrap_err()
    )
    .unsatisfiable());
    m.release(a, first.lease, 1).unwrap();
    m.release(b, second.lease, 1).unwrap();
    let exclusive = m
        .claim(a, &request(Grouping::Exclusive, &[&["name=a1"]]), 2)
        .unwrap();
    assert_eq!(exclusive.assignment.len(), 1);
    for mode in [Grouping::None, Grouping::Same, Grouping::Exclusive] {
        assert!(
            !no_match(m.claim(b, &request(mode, &[&["name=aw"]]), 3).unwrap_err()).unsatisfiable()
        );
    }
    assert!(m
        .claim(b, &request(Grouping::None, &[&["name=bw"]]), 3)
        .is_ok());
    m.release(a, exclusive.lease, 4).unwrap();
    assert!(m
        .claim(b, &request(Grouping::None, &[&["name=aw"]]), 5)
        .is_ok());
}
#[test]
fn busy_group_is_skipped_and_ungrouped_bench_is_not_a_group() {
    let mut m = manager();
    let a = m.register("a");
    let b = m.register("b");
    m.claim(a, &request(Grouping::None, &[&["name=ab"]]), 0)
        .unwrap();
    let grant = m
        .claim(
            b,
            &request(Grouping::Exclusive, &[&["device=esp32"], &["device=wifi"]]),
            0,
        )
        .unwrap();
    assert!(grant
        .assignment
        .values()
        .all(|id| m.inventory().benches[id].group.as_deref() == Some("b")));
    assert!(no_match(
        m.claim(b, &request(Grouping::Same, &[&["name=ungrouped"]]), 0)
            .unwrap_err()
    )
    .unsatisfiable());
}

#[test]
fn matching_backtracks_within_a_group_and_busy_feasible_groups_are_retryable() {
    let mut m = manager();
    let s = m.register("test");
    // The unconstrained slot must leave a1 for the constrained slot.
    let grant = m
        .claim(
            s,
            &request(Grouping::Same, &[&["device=esp32"], &["name=a1"]]),
            0,
        )
        .unwrap();
    assert_eq!(grant.assignment["slot0"], "a2");
    m.release(s, grant.lease, 0).unwrap();
    m.claim(s, &request(Grouping::None, &[&["name=a2"]]), 0)
        .unwrap();
    let error = m
        .claim(
            s,
            &request(Grouping::Same, &[&["device=esp32"], &["name=a1"]]),
            0,
        )
        .unwrap_err();
    assert!(!no_match(error).unsatisfiable());
}

#[test]
fn exclusive_reserves_new_members_but_only_exports_and_counts_selected_benches() {
    let mut m = LeaseManager::new(
        Inventory::from_toml_str(LAB).unwrap(),
        Limits {
            max_benches: 1,
            ..Limits::default()
        },
    );
    let s = m.register("test");
    let grant = m
        .claim(s, &request(Grouping::Exclusive, &[&["name=a1"]]), 0)
        .unwrap();
    assert_eq!(grant.group.as_deref(), Some("a"));
    assert_eq!(grant.grouping, Grouping::Exclusive);
    assert_eq!(
        grant
            .effects
            .iter()
            .filter(|e| matches!(e, benchd_core::Effect::Export { .. }))
            .count(),
        1
    );
    let mut new = m.inventory().benches["aw"].clone();
    new.id = "new".into();
    m.inventory_mut().benches.insert(new.id.clone(), new);
    assert!(m.busy(0).contains_key("new"));
    let effects = m.end_session(s, 1);
    assert!(effects.iter().any(|e| matches!(e, benchd_core::Effect::Unmaterialize { exclusive_group: Some(g), .. } if g == "a")));
    assert!(!m.group_reserved("a"));
}

#[test]
fn expiry_force_and_setup_failure_release_the_same_exclusive_reservation() {
    for end in ["expiry", "force", "failure"] {
        let mut m = manager();
        let s = m.register("test");
        let g = m
            .claim(s, &request(Grouping::Exclusive, &[&["name=a1"]]), 0)
            .unwrap();
        let effects = match end {
            "expiry" => m.tick(60),
            "force" => {
                m.force_release(g.lease, 0);
                m.tick(60)
            }
            _ => m.drop_lease(g.lease),
        };
        assert!(!m.group_reserved("a"));
        assert!(effects.iter().any(|e| matches!(e, benchd_core::Effect::Unmaterialize { exclusive_group: Some(g), .. } if g == "a")));
        assert!(m
            .claim(s, &request(Grouping::None, &[&["name=aw"]]), 61)
            .is_ok());
    }
}

#[test]
fn wire_defaults_are_compatible_and_unknown_modes_are_rejected() {
    use benchd_core::wire::ClaimSpec;
    let old = r#"{"slots":{"dut":["device=esp32"]},"ttl":60}"#;
    let spec: ClaimSpec = serde_json::from_str(old).unwrap();
    assert_eq!(spec.grouping, Grouping::None);
    for mode in [Grouping::None, Grouping::Same, Grouping::Exclusive] {
        let mut spec = spec.clone();
        spec.grouping = mode;
        let value = serde_json::to_value(&spec).unwrap();
        let back: ClaimSpec = serde_json::from_value(value).unwrap();
        assert_eq!(back.grouping, mode);
    }
    assert!(serde_json::from_str::<ClaimSpec>(
        &old.replace("{\"slots\"", "{\"grouping\":\"typo\",\"slots\"")
    )
    .is_err());
}

#[test]
fn disabled_unselected_members_prevent_exclusive_admission_through_inventory_api() {
    let mut inventory = Inventory::from_toml_str(LAB).unwrap();
    inventory.benches.get_mut("ab").unwrap().enabled = false;
    let req = request(Grouping::Exclusive, &[&["name=a1"]]);
    let error = benchd_core::allocate_in(&inventory, &req, &BTreeMap::new()).unwrap_err();
    assert!(!error.unsatisfiable());
    assert!(benchd_core::allocate_in(
        &inventory,
        &request(Grouping::Same, &[&["name=a1"]]),
        &BTreeMap::new()
    )
    .is_ok());
}
