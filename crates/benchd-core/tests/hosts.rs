use std::collections::BTreeMap;

use benchd_core::lease::{ClaimError, LeaseManager};
use benchd_core::limits::Limits;
use benchd_core::matcher::fit_cost;
use benchd_core::model::{ClaimRequest, Distinct, Inventory, Requirement};
use benchd_core::tags::{Tag, Vocabulary};

const INVENTORY: &str = r#"
open_keys = ["name"]
[tags.soc.values.esp32s3]
[benches.a]
tags = ["soc=esp32s3", "host=lab-a.example.com"]
[benches.b]
tags = ["soc=esp32s3", "host=lab-b"]
"#;

fn request(hosts: &[&str]) -> ClaimRequest {
    ClaimRequest {
        slots: hosts
            .iter()
            .enumerate()
            .map(|(i, host)| {
                (
                    format!("slot{i}"),
                    Requirement::parse(["soc=esp32s3", host]).unwrap(),
                )
            })
            .collect(),
        distinct: Distinct::All,
        ttl_seconds: 60,
        reason: "multi-host test".into(),
    }
}

#[test]
fn host_is_builtin_even_with_an_existing_open_keys_setting() {
    let tag = Tag::parse("host=lab-a.example.com").unwrap();
    assert!(Vocabulary::default().check([&tag]).is_ok());
    let inventory = Inventory::from_toml_str(INVENTORY).unwrap();
    assert!(inventory.vocabulary.check([&tag]).is_ok());
    assert!(Requirement::parse(["host=lab-a.example.com"])
        .unwrap()
        .matches(&inventory.benches["a"]));
}

#[test]
fn host_identity_does_not_make_a_bench_more_expensive() {
    let inventory = Inventory::from_toml_str(INVENTORY).unwrap();
    let requirement = Requirement::parse(["soc=esp32s3"]).unwrap();
    assert_eq!(
        fit_cost(
            &inventory.benches["a"],
            &requirement,
            &inventory.tag_counts(),
            &BTreeMap::new()
        ),
        0.0
    );
}

#[test]
fn multi_host_claim_reserves_every_bench_under_one_lease() {
    let mut manager = LeaseManager::new(
        Inventory::from_toml_str(INVENTORY).unwrap(),
        Limits::default(),
    );
    let session = manager.register("requester");
    let grant = manager
        .claim(
            session,
            &request(&["host=lab-a.example.com", "host=lab-b"]),
            0,
        )
        .unwrap();
    assert_eq!(grant.assignment["slot0"], "a");
    assert_eq!(grant.assignment["slot1"], "b");
    assert_eq!(manager.leases().count(), 1);
    assert_eq!(manager.busy(0).len(), 2);
    manager.release(session, grant.lease, 1).unwrap();
    assert!(manager.busy(1).is_empty());
}

#[test]
fn unavailable_slot_does_not_reserve_the_other_host() {
    let mut manager = LeaseManager::new(
        Inventory::from_toml_str(INVENTORY).unwrap(),
        Limits::default(),
    );
    let holder = manager.register("holder");
    let requester = manager.register("requester");
    let held = manager.claim(holder, &request(&["host=lab-b"]), 0).unwrap();
    for unavailable in ["host=lab-b", "host=missing", "host=lab-a.example.com"] {
        assert!(matches!(
            manager.claim(
                requester,
                &request(&["host=lab-a.example.com", unavailable]),
                1
            ),
            Err(ClaimError::NoMatch(_))
        ));
        assert_eq!(manager.leases().count(), 1);
        assert!(!manager.busy(1).contains_key("a"));
        assert!(manager.lease(held.lease).is_some());
    }
    assert!(manager
        .claim(requester, &request(&["host=lab-a.example.com"]), 2)
        .is_ok());
}
