//! Behavioural tests for the matcher.
//!
//! These are written to be readable as a specification: if you want to know
//! what the matcher promises, read the test names and assertions rather than
//! `matcher.rs`.

use std::collections::BTreeMap;

use benchd::matcher::{BusyInfo, Failure};
use benchd::model::{Bench, ClaimRequest, Distinct, Inventory, Requirement};
use benchd::tags::{parse_tags, Tag, TagError};
use benchd::{allocate_in, format_tags};

const INVENTORY: &str = include_str!("../examples/inventory.toml");

fn inventory() -> Inventory {
    Inventory::from_toml_str(INVENTORY).expect("example inventory should load")
}

fn claim(slots: &[(&str, &[&str])], distinct: Distinct) -> ClaimRequest {
    ClaimRequest {
        slots: slots
            .iter()
            .map(|(name, tags)| {
                (name.to_string(), Requirement::parse(tags.iter().copied()).unwrap())
            })
            .collect(),
        distinct,
        ttl_seconds: 900,
        reason: "test".into(),
    }
}

fn no_one_is_busy() -> BTreeMap<String, BusyInfo> {
    BTreeMap::new()
}

// --- matching semantics ----------------------------------------------------

#[test]
fn a_bench_matches_when_its_tags_are_a_superset_of_the_request() {
    let inv = inventory();
    let req = claim(&[("dut", &["soc=esp32s3"])], Distinct::All);
    let alloc = allocate_in(&inv, &req, &no_one_is_busy()).expect("should match");
    assert!(alloc.assignment["dut"].starts_with("esp32s3-"));
}

#[test]
fn implied_tags_match_even_when_not_declared_on_the_bench() {
    // No bench declares `family=esp32`; all three imply it via `soc=`.
    let inv = inventory();
    for bench in inv.enabled_benches() {
        assert!(
            bench.tags.contains(&Tag::new("family", "esp32")),
            "{} should have inherited family=esp32, has: {}",
            bench.id,
            format_tags(&bench.tags)
        );
    }
}

#[test]
fn requests_are_not_expanded_only_benches_are() {
    // Asking for `family=esp32` must stay a one-tag requirement; if requirements
    // were expanded too, this would silently demand arch/net/jtag as well and
    // become harder to satisfy rather than easier.
    let req = Requirement::parse(["family=esp32"]).unwrap();
    assert_eq!(req.tags.len(), 1);
}

// --- best fit --------------------------------------------------------------

#[test]
fn among_adequate_benches_the_least_capable_one_wins() {
    // esp32s3-a has PSRAM, esp32s3-b does not. A request that does not ask for
    // PSRAM must not consume the PSRAM board.
    let inv = inventory();
    let req = claim(&[("dut", &["soc=esp32s3"])], Distinct::All);
    let alloc = allocate_in(&inv, &req, &no_one_is_busy()).unwrap();
    assert_eq!(alloc.assignment["dut"], "esp32s3-b");
}

#[test]
fn identity_tags_do_not_make_a_cheap_bench_look_precious() {
    // Regression test for a real bug: unweighted scarcity scoring made the
    // CP2102N board (the only one of its kind, but the least capable) the most
    // "expensive" to allocate, so `family=esp32` grabbed a PSRAM S3 instead.
    // Identity keys carry weight 0, so the cheap board wins.
    let inv = inventory();
    let req = claim(&[("dut", &["family=esp32"])], Distinct::All);
    let alloc = allocate_in(&inv, &req, &no_one_is_busy()).unwrap();
    assert_eq!(alloc.assignment["dut"], "esp32-cp2102");
}

// --- multi-slot ------------------------------------------------------------

#[test]
fn multi_slot_claims_get_distinct_benches() {
    let inv = inventory();
    let req = claim(
        &[("dut", &["soc=esp32s3"]), ("peer", &["family=esp32"])],
        Distinct::All,
    );
    let alloc = allocate_in(&inv, &req, &no_one_is_busy()).unwrap();
    assert_ne!(alloc.assignment["dut"], alloc.assignment["peer"]);
    assert!(alloc.assignment["dut"].starts_with("esp32s3-"));
}

#[test]
fn distinctness_can_be_waived_per_claim() {
    let inv = inventory();
    let req = claim(
        &[("a", &["soc=esp32s3"]), ("b", &["soc=esp32s3"])],
        Distinct::None,
    );
    let alloc = allocate_in(&inv, &req, &no_one_is_busy()).unwrap();
    assert_eq!(alloc.assignment["a"], alloc.assignment["b"]);
}

#[test]
fn a_claim_is_all_or_nothing() {
    // One satisfiable slot, one impossible slot -> the whole claim fails.
    let inv = inventory();
    let req = claim(
        &[("ok", &["soc=esp32s3"]), ("nope", &["soc=esp32c3"])],
        Distinct::All,
    );
    let err = allocate_in(&inv, &req, &no_one_is_busy()).unwrap_err();
    assert!(err.unsatisfiable());
}

// --- diagnosis -------------------------------------------------------------

#[test]
fn all_benches_busy_is_reported_as_contended_not_unsatisfiable() {
    let inv = inventory();
    let busy: BTreeMap<String, BusyInfo> = [
        (
            "esp32s3-a".to_string(),
            BusyInfo { owner: "agent-3".into(), expires_in: Some(240.0), reason: "wifi".into() },
        ),
        (
            "esp32s3-b".to_string(),
            BusyInfo { owner: "tom".into(), expires_in: None, reason: String::new() },
        ),
    ]
    .into();

    let req = claim(&[("dut", &["soc=esp32s3"])], Distinct::All);
    let err = allocate_in(&inv, &req, &busy).unwrap_err();

    assert!(!err.unsatisfiable(), "agents must know to wait, not to give up");
    assert_eq!(err.slots[0].failure, Failure::Contended);
    assert_eq!(err.slots[0].matching.len(), 2);
    assert_eq!(err.slots[0].earliest_free(), Some(240.0));
    let rendered = err.to_string();
    assert!(rendered.contains("held by agent-3"), "{rendered}");
    assert!(rendered.contains("expires in 240s"), "{rendered}");
}

#[test]
fn an_impossible_request_names_the_tag_that_makes_it_impossible() {
    let inv = inventory();
    let req = claim(&[("dut", &["soc=esp32c3", "psram=octal"])], Distinct::All);
    let err = allocate_in(&inv, &req, &no_one_is_busy()).unwrap_err();

    assert!(err.unsatisfiable(), "no ESP32-C3 exists; retrying will never help");
    let diag = &err.slots[0];
    assert!(diag.impossible_tags.contains(&Tag::new("soc", "esp32c3")));
    // ...and tells the agent exactly how to relax the request.
    assert_eq!(diag.drop_to_match, parse_tags(["soc=esp32c3"]).unwrap());
    assert_eq!(diag.closest_satisfiable, parse_tags(["psram=octal"]).unwrap());
}

#[test]
fn an_impossible_combination_of_individually_possible_tags_is_explained() {
    // Every tag exists somewhere, but no single bench has all of them.
    let inv = inventory();
    let req = claim(&[("dut", &["usb=cp2102n", "psram=octal"])], Distinct::All);
    let err = allocate_in(&inv, &req, &no_one_is_busy()).unwrap_err();

    let diag = &err.slots[0];
    assert!(err.unsatisfiable());
    assert!(diag.impossible_tags.is_empty(), "each tag exists on its own");
    assert_eq!(diag.drop_to_match.len(), 1, "dropping one tag should suffice");
}

#[test]
fn a_distinctness_conflict_is_not_reported_as_contention() {
    // Both slots only match the CP2102N board, which is free. The failure is
    // structural, not availability, and the message must say so.
    let inv = inventory();
    let req = claim(
        &[("a", &["usb=cp2102n"]), ("b", &["soc=esp32"])],
        Distinct::All,
    );
    let err = allocate_in(&inv, &req, &no_one_is_busy()).unwrap_err();

    assert!(err.conflict_only);
    assert!(!err.unsatisfiable());
    let rendered = err.to_string();
    assert!(rendered.contains("must be distinct"), "{rendered}");
    assert!(rendered.contains("free"), "{rendered}");
}

// --- vocabulary ------------------------------------------------------------

#[test]
fn a_typo_is_rejected_with_suggestions_rather_than_matching_nothing() {
    let inv = inventory();
    let err = inv
        .vocabulary
        .check([&Tag::new("soc", "esp32s4")])
        .expect_err("unknown tag must be rejected");
    match err {
        TagError::Unknown { suggestions, .. } => {
            assert!(
                suggestions.iter().any(|s| s == "soc=esp32s3"),
                "expected soc=esp32s3 in {suggestions:?}"
            );
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn a_capability_value_with_a_dash_is_rejected_when_the_vocabulary_declares_it() {
    // The `esp32-s3` vs `esp32s3` split is the failure mode that makes tag
    // vocabularies rot, so it is caught where the vocabulary is written.
    let toml = r#"
        [tags.soc]
        [tags.soc.values."esp32-s3"]
    "#;
    let err = Inventory::from_toml_str(toml).expect_err("dashed capability value must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("must not contain a dash"), "{msg}");
    assert!(msg.contains("soc=esp32s3"), "should suggest the fix: {msg}");
}

#[test]
fn identity_values_may_contain_dashes_so_bench_ids_survive_round_tripping() {
    // `name=` is an identity tag, not a capability, so `esp32s3-a` must parse
    // and match rather than being silently mangled into `esp32s3_a`.
    let inv = inventory();
    let tag = Tag::parse("name=esp32s3-a").expect("identity values may contain dashes");
    assert!(inv.benches["esp32s3-a"].tags.contains(&tag));

    let req = claim(&[("dut", &["name=esp32s3-a"])], Distinct::All);
    let alloc = allocate_in(&inv, &req, &no_one_is_busy()).unwrap();
    assert_eq!(alloc.assignment["dut"], "esp32s3-a");
}

#[test]
fn single_character_tag_keys_are_allowed() {
    let toml = r#"
        [tags.v]
        [tags.v.values.one]

        [benches.only]
        tags = ["v=one"]
    "#;
    let inv = Inventory::from_toml_str(toml).expect("single-char keys are fine");
    let req = claim(&[("dut", &["v=one"])], Distinct::All);
    assert!(allocate_in(&inv, &req, &no_one_is_busy()).is_ok());
}

#[test]
fn a_bench_may_hold_several_boards_and_they_are_claimed_together() {
    // D11: a bench is a *physical grouping*, not a single board. A mesh rig whose
    // three nodes share a carrier and a power rail is one bench; handing out one
    // node while another agent drives the other two is meaningless.
    let toml = r#"
        [tags.soc]
        weight = 0
        [tags.soc.values.esp32c3]

        [tags.topology]
        [tags.topology.values.mesh]

        [tags.nodes]
        [tags.nodes.values."3"]

        [benches."mesh-rig"]
        description = "Three C3s on one carrier, shared power rail"
        tags = ["soc=esp32c3", "topology=mesh", "nodes=3"]
        [benches."mesh-rig".resources.node_a]
        kind = "serial"
        by_id = "/dev/serial/by-id/fake-a"
        [benches."mesh-rig".resources.node_b]
        kind = "serial"
        by_id = "/dev/serial/by-id/fake-b"
        [benches."mesh-rig".resources.node_c]
        kind = "serial"
        by_id = "/dev/serial/by-id/fake-c"
    "#;

    let inv = Inventory::from_toml_str(toml).unwrap();
    let rig = &inv.benches["mesh-rig"];
    assert_eq!(rig.resource_names(), vec!["node_a", "node_b", "node_c"]);

    // One slot, one bench, three device nodes for the holder.
    let req = claim(&[("dut", &["topology=mesh"])], Distinct::All);
    let alloc = allocate_in(&inv, &req, &no_one_is_busy()).unwrap();
    assert_eq!(alloc.assignment["dut"], "mesh-rig");
    assert_eq!(inv.benches[&alloc.assignment["dut"]].resources.len(), 3);
}

#[test]
fn tags_describe_the_whole_bench_not_individual_boards() {
    // Corollary of D11: there is no way to ask for "the C3 inside the mesh rig".
    // If boards within a bench differ in ways an agent must select on, that is
    // evidence they should have been separate benches.
    let inv = inventory();
    for bench in inv.enabled_benches() {
        let names: Vec<&str> = bench.tags.iter().map(|t| t.key.as_str()).collect();
        assert!(
            !names.contains(&"resource"),
            "tags must not address individual resources"
        );
    }
}

#[test]
fn implication_cycles_are_rejected_at_load_time() {
    let toml = r#"
        [tags.a]
        [tags.a.values.one]
        implies = ["b=two"]
        [tags.b]
        [tags.b.values.two]
        implies = ["a=one"]
    "#;
    let err = Inventory::from_toml_str(toml).expect_err("cycle must be rejected");
    assert!(err.to_string().contains("cycle"), "{err}");
}

#[test]
fn a_disabled_bench_is_invisible_to_the_matcher() {
    let toml = r#"
        [tags.soc]
        [tags.soc.values.esp32]

        [benches.only]
        tags = ["soc=esp32"]
        enabled = false
    "#;
    let inv = Inventory::from_toml_str(toml).unwrap();
    let req = claim(&[("dut", &["soc=esp32"])], Distinct::All);
    let err = allocate_in(&inv, &req, &no_one_is_busy()).unwrap_err();
    assert!(err.unsatisfiable());
}

// --- properties ------------------------------------------------------------
//
// The example tests above pin specific behaviours; these pin the invariants.
// "Allocation succeeds exactly when a valid assignment exists" is a far
// stronger claim than any example, and it is what stops a future optimisation
// from quietly making the matcher return false negatives.

mod properties {
    use super::*;
    use benchd::matcher::allocate;
    use benchd::tags::TagSet;
    use proptest::prelude::*;

    /// Build a bench directly, bypassing config parsing.
    fn bench(id: &str, tags: &[(&str, &str)]) -> Bench {
        let mut tagset: TagSet = tags.iter().map(|(k, v)| Tag::new(*k, *v)).collect();
        tagset.insert(Tag::new("name", id));
        Bench {
            id: id.to_string(),
            tags: tagset,
            resources: Default::default(),
            description: String::new(),
            enabled: true,
        }
    }

    const POOL: &[(&str, &str)] = &[
        ("soc", "esp32"),
        ("soc", "esp32s3"),
        ("psram", "octal"),
        ("jtag", "builtin"),
        ("flash", "8mb"),
    ];

    /// Brute-force reference: does *any* valid assignment exist, and what is the
    /// cheapest total cost? Deliberately dumb, so it is obviously correct.
    fn reference(
        request: &ClaimRequest,
        benches: &[&Bench],
        busy: &BTreeMap<String, BusyInfo>,
        counts: &BTreeMap<Tag, usize>,
        weights: &BTreeMap<String, f64>,
    ) -> Option<f64> {
        let slots: Vec<&String> = request.slots.keys().collect();
        let free: Vec<&Bench> = benches
            .iter()
            .copied()
            .filter(|b| b.enabled && !busy.contains_key(&b.id))
            .collect();

        #[allow(clippy::too_many_arguments)] // deliberately dumb, so it is obviously correct
        fn go(
            i: usize,
            slots: &[&String],
            request: &ClaimRequest,
            free: &[&Bench],
            used: &mut Vec<String>,
            counts: &BTreeMap<Tag, usize>,
            weights: &BTreeMap<String, f64>,
            acc: f64,
            best: &mut Option<f64>,
        ) {
            if i == slots.len() {
                *best = Some(best.map_or(acc, |b: f64| b.min(acc)));
                return;
            }
            let slot = slots[i];
            let req = &request.slots[slot];
            for b in free {
                if !req.matches(b) {
                    continue;
                }
                if request.distinct.applies_to(slot) && used.contains(&b.id) {
                    continue;
                }
                used.push(b.id.clone());
                let c = benchd::fit_cost(b, req, counts, weights);
                go(i + 1, slots, request, free, used, counts, weights, acc + c, best);
                used.pop();
            }
        }

        let mut best = None;
        go(0, &slots, request, &free, &mut Vec::new(), counts, weights, 0.0, &mut best);
        best
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        /// The matcher succeeds exactly when a valid assignment exists, and when
        /// it succeeds the assignment it returns is optimal.
        #[test]
        fn allocation_agrees_with_brute_force(
            bench_masks in prop::collection::vec(0u8..32, 1..5),
            slot_masks in prop::collection::vec(0u8..32, 1..4),
            distinct_all in any::<bool>(),
        ) {
            let benches: Vec<Bench> = bench_masks
                .iter()
                .enumerate()
                .map(|(i, mask)| {
                    let tags: Vec<(&str, &str)> = POOL
                        .iter()
                        .enumerate()
                        .filter(|(j, _)| mask & (1 << j) != 0)
                        .map(|(_, t)| *t)
                        .collect();
                    bench(&format!("b{i}"), &tags)
                })
                .collect();
            let refs: Vec<&Bench> = benches.iter().collect();

            let slots: BTreeMap<String, Requirement> = slot_masks
                .iter()
                .enumerate()
                .map(|(i, mask)| {
                    let tags: TagSet = POOL
                        .iter()
                        .enumerate()
                        .filter(|(j, _)| mask & (1 << j) != 0)
                        .map(|(_, (k, v))| Tag::new(*k, *v))
                        .collect();
                    (format!("s{i}"), Requirement::new(tags))
                })
                .collect();

            let request = ClaimRequest {
                slots,
                distinct: if distinct_all { Distinct::All } else { Distinct::None },
                ttl_seconds: 60,
                reason: String::new(),
            };

            let mut counts: BTreeMap<Tag, usize> = BTreeMap::new();
            for b in &benches {
                for t in &b.tags {
                    *counts.entry(t.clone()).or_insert(0) += 1;
                }
            }
            let weights = BTreeMap::new();
            let busy = BTreeMap::new();

            let expected = reference(&request, &refs, &busy, &counts, &weights);
            let actual = allocate(&request, &refs, &busy, &counts, &weights);

            match (expected, actual) {
                (None, Err(_)) => {}
                (Some(best), Ok(alloc)) => {
                    prop_assert!(
                        (alloc.cost - best).abs() < 1e-9,
                        "matcher returned cost {} but optimum is {best}",
                        alloc.cost
                    );
                    // The returned assignment must actually be valid.
                    let mut seen: Vec<&String> = Vec::new();
                    for (slot, bench_id) in &alloc.assignment {
                        let b = benches.iter().find(|b| &b.id == bench_id).unwrap();
                        prop_assert!(request.slots[slot].matches(b));
                        if request.distinct.applies_to(slot) {
                            prop_assert!(!seen.contains(&bench_id));
                            seen.push(bench_id);
                        }
                    }
                    prop_assert_eq!(alloc.assignment.len(), request.slots.len());
                }
                (None, Ok(a)) => prop_assert!(false, "matched {a:?} when nothing should match"),
                (Some(b), Err(e)) => prop_assert!(false, "no match (cost {b} exists): {e}"),
            }
        }
    }
}
