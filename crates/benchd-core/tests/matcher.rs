//! Behavioural tests for the matcher.
//!
//! These are written to be readable as a specification: if you want to know
//! what the matcher promises, read the test names and assertions rather than
//! `matcher.rs`.

use std::collections::BTreeMap;

use benchd_core::matcher::{fit_cost, BusyInfo, Failure};
use benchd_core::model::{Bench, ClaimRequest, Distinct, Inventory, Requirement};
use benchd_core::tags::{parse_tags, Tag, TagError};
use benchd_core::{allocate_in, format_tags};

const INVENTORY: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/inventory.toml"
));

/// The vocabulary the coordinator actually deploys. Benches live with their
/// hosts, so this file has none, but its tag definitions must classify keys the
/// same way the example inventory does or the lab scores differently from the
/// tests.
const COORDINATOR: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/coordinator.toml"
));

fn inventory() -> Inventory {
    Inventory::from_toml_str(INVENTORY).expect("example inventory should load")
}

fn claim(slots: &[(&str, &[&str])], distinct: Distinct) -> ClaimRequest {
    ClaimRequest {
        grouping: benchd_core::model::Grouping::None,
        slots: slots
            .iter()
            .map(|(name, tags)| {
                (
                    name.to_string(),
                    Requirement::parse(tags.iter().copied()).unwrap(),
                )
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

/// What each candidate would cost for a single-slot claim, with nothing busy.
///
/// Which bench won is only half the story: ties are broken by bench id, so an
/// assertion naming the winner passes just as happily when the metric says the
/// two are indistinguishable. These tests assert the margin as well.
fn costs(inv: &Inventory, req: &ClaimRequest, slot: &str) -> BTreeMap<String, f64> {
    let counts = inv.tag_counts();
    let weights = inv.vocabulary.key_weights();
    inv.enabled_benches()
        .iter()
        .filter(|b| req.slots[slot].matches(b))
        .map(|b| {
            (
                b.id.clone(),
                fit_cost(b, &req.slots[slot], &counts, weights),
            )
        })
        .collect()
}

#[test]
fn among_adequate_benches_the_least_capable_one_wins() {
    // esp32s3-a has PSRAM, esp32s3-b does not. A request that does not ask for
    // PSRAM must not consume the PSRAM board.
    let inv = inventory();
    let req = claim(&[("dut", &["soc=esp32s3"])], Distinct::All);
    let alloc = allocate_in(&inv, &req, &no_one_is_busy()).unwrap();
    assert_eq!(alloc.assignment["dut"], "esp32s3-b");

    let by_bench = costs(&inv, &req, "dut");
    assert!(
        by_bench["esp32s3-a"] > by_bench["esp32s3-b"],
        "the PSRAM board must be the strictly worse fit, not merely the later id: {by_bench:?}"
    );
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

    // On the metric, and by a margin. The bare UART board and the 8 MiB S3 with
    // built-in JTAG once scored exactly equal - the penalty for having less
    // flash cancelling the S3's debug hardware - which left this test passing
    // on nothing but "esp32-cp2102" sorting before "esp32s3-b".
    let by_bench = costs(&inv, &req, "dut");
    let cheap = by_bench["esp32-cp2102"];
    for (id, cost) in &by_bench {
        assert!(
            id == "esp32-cp2102" || *cost > cheap,
            "{id} costs {cost} against the bare board's {cheap}: the id ordering is \
             deciding this, not the fit"
        );
    }
}

#[test]
fn the_least_capable_bench_wins_however_its_id_sorts() {
    // The same inventory, one bench renamed and nothing else. A pure naming
    // change must not move an allocation; when it does, the winner was decided
    // by the tie-break and the scoring was never tested at all.
    let renamed = INVENTORY.replace("esp32-cp2102", "zz-uart-board");
    let inv = Inventory::from_toml_str(&renamed).expect("a rename is not a config change");
    let req = claim(&[("dut", &["family=esp32"])], Distinct::All);
    let alloc = allocate_in(&inv, &req, &no_one_is_busy()).unwrap();
    assert_eq!(alloc.assignment["dut"], "zz-uart-board");
}

#[test]
fn a_descriptive_key_costs_the_same_whether_it_was_asked_for_or_not() {
    // flash values are ordered in the world and not in the matcher: scoring
    // divides by the number of benches carrying that exact value, so 4mb (one
    // board) was charged twice what 8mb (two boards) was, and "least capable
    // wins" ran backwards. Weight 0 is what stops flash being priced at all.
    let inv = inventory();
    assert_eq!(inv.vocabulary.weight("flash"), 0.0);

    let counts = inv.tag_counts();
    let weights = inv.vocabulary.key_weights();
    let bench = &inv.benches["esp32-cp2102"];
    let asked = Requirement::parse(["family=esp32", "flash=4mb"]).unwrap();
    let unasked = Requirement::parse(["family=esp32"]).unwrap();
    assert_eq!(
        fit_cost(bench, &asked, &counts, weights),
        fit_cost(bench, &unasked, &counts, weights),
        "flash describes the board; it is not capability anyone can waste"
    );
}

#[test]
fn no_shipped_tag_value_names_an_absence() {
    // An absent capability is charged exactly like a real one - `psram=none`
    // cost the bare board as much as octal PSRAM cost the S3 - so a board
    // without the hardware omits the key instead. Both shipped vocabularies
    // have to keep to that, and the rule is easier to break than to notice.
    for (file, text) in [
        ("examples/inventory.toml", INVENTORY),
        ("examples/coordinator.toml", COORDINATOR),
    ] {
        let inv = Inventory::from_toml_str(text).expect("shipped config should load");
        for tag in inv.vocabulary.tags() {
            assert!(
                !matches!(tag.value.as_str(), "none" | "no" | "absent" | "unknown"),
                "{file} declares {tag}, which prices not having something"
            );
        }
        for bench in inv.enabled_benches() {
            for tag in &bench.tags {
                assert!(
                    tag.value != "none",
                    "{file}: bench {} declares {tag}",
                    bench.id
                );
            }
        }
    }
}

#[test]
fn the_shipped_vocabularies_agree_on_which_keys_are_capabilities() {
    // The coordinator deploys one of these files and the tests read the other.
    // A key that is descriptive in one and contended in the other scores the
    // lab differently from every test here, silently.
    let inventory = inventory();
    let coordinator = Inventory::from_toml_str(COORDINATOR).expect("coordinator config loads");
    for tag in inventory.vocabulary.tags() {
        assert_eq!(
            inventory.vocabulary.weight(&tag.key),
            coordinator.vocabulary.weight(&tag.key),
            "the two configs disagree about {:?}",
            tag.key
        );
    }
}

#[test]
fn scarcity_is_counted_over_the_benches_a_claim_could_actually_get() {
    // Three boards carry an external probe and two carry GPS. With two probes
    // leased out the lab has exactly one left, and a plain soc=esp32s3 claim
    // must not spend it: counting benches that exist rather than benches that
    // are free priced that last probe at a third of its worth and handed it to
    // a request that never mentioned JTAG.
    let toml = r#"
        [tags.soc]
        weight = 0
        [tags.soc.values.esp32s3]

        [tags.jtag]
        [tags.jtag.values.external]

        [tags.peripheral]
        [tags.peripheral.values.gps]

        [benches."j00"]
        tags = ["soc=esp32s3", "jtag=external"]
        [benches."j01"]
        tags = ["soc=esp32s3", "jtag=external"]
        [benches."j02"]
        tags = ["soc=esp32s3", "jtag=external"]
        [benches."g00"]
        tags = ["soc=esp32s3", "peripheral=gps"]
        [benches."g01"]
        tags = ["soc=esp32s3", "peripheral=gps"]
    "#;
    let inv = Inventory::from_toml_str(toml).unwrap();
    let busy: BTreeMap<String, BusyInfo> = ["j00", "j01"]
        .into_iter()
        .map(|id| {
            (
                id.to_string(),
                BusyInfo {
                    owner: "agent-1".into(),
                    expires_in: Some(60.0),
                    reason: "flashing".into(),
                },
            )
        })
        .collect();

    let req = claim(&[("dut", &["soc=esp32s3"])], Distinct::All);
    let alloc = allocate_in(&inv, &req, &busy).unwrap();
    assert_eq!(
        alloc.assignment["dut"], "g00",
        "the only free probe must be the expensive bench now"
    );
    assert_eq!(alloc.cost, 0.5, "one of two free GPS boards");
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
            BusyInfo {
                owner: "agent-3".into(),
                expires_in: Some(240.0),
                reason: "wifi".into(),
            },
        ),
        (
            "esp32s3-b".to_string(),
            BusyInfo {
                owner: "tom".into(),
                expires_in: None,
                reason: String::new(),
            },
        ),
    ]
    .into();

    let req = claim(&[("dut", &["soc=esp32s3"])], Distinct::All);
    let err = allocate_in(&inv, &req, &busy).unwrap_err();

    assert!(
        !err.unsatisfiable(),
        "agents must know to wait, not to give up"
    );
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

    assert!(
        err.unsatisfiable(),
        "no ESP32-C3 exists; retrying will never help"
    );
    let diag = &err.slots[0];
    assert!(diag.impossible_tags.contains(&Tag::new("soc", "esp32c3")));
    // ...and tells the agent exactly how to relax the request.
    assert_eq!(diag.drop_to_match, parse_tags(["soc=esp32c3"]).unwrap());
    assert_eq!(
        diag.closest_satisfiable,
        parse_tags(["psram=octal"]).unwrap()
    );
}

#[test]
fn an_impossible_combination_of_individually_possible_tags_is_explained() {
    // Every tag exists somewhere, but no single bench has all of them.
    let inv = inventory();
    let req = claim(&[("dut", &["console=uart", "psram=octal"])], Distinct::All);
    let err = allocate_in(&inv, &req, &no_one_is_busy()).unwrap_err();

    let diag = &err.slots[0];
    assert!(err.unsatisfiable());
    assert!(
        diag.impossible_tags.is_empty(),
        "each tag exists on its own"
    );
    assert_eq!(
        diag.drop_to_match.len(),
        1,
        "dropping one tag should suffice"
    );
}

#[test]
fn a_distinctness_conflict_is_not_reported_as_contention() {
    // Both slots only match the CP2102N board, which is free. The failure is
    // structural, not availability, and the message must say so.
    let inv = inventory();
    let req = claim(
        &[("a", &["console=uart"]), ("b", &["soc=esp32"])],
        Distinct::All,
    );
    let err = allocate_in(&inv, &req, &no_one_is_busy()).unwrap_err();

    assert!(err.conflict_only);
    // ...and it must NOT be advertised as worth retrying. Nothing is busy, so
    // waiting can never help. This assertion originally read
    // `assert!(!err.unsatisfiable())`, encoding the bug: an agent following the
    // retryable flag would spin forever against a free bench.
    assert!(
        err.unsatisfiable(),
        "a conflict against free benches cannot be fixed by waiting"
    );
    let rendered = err.to_string();
    assert!(rendered.contains("different benches"), "{rendered}");
    assert!(rendered.contains("Waiting will not help"), "{rendered}");
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

// --- qualified values -------------------------------------------------------

/// Two boards with the same accelerometer, one of which had its part number
/// written down.
const QUALIFIED: &str = r#"
    [tags.peripheral]
    qualified = true
    [tags.peripheral.values.accel]
    description = "Accelerometer"

    [tags.flash]
    [tags.flash.values."4mb"]

    [benches.recorded]
    tags = ["peripheral=accel[mpu6050]", "flash=4mb"]

    [benches.vague]
    tags = ["peripheral=accel", "flash=4mb"]
"#;

#[test]
fn naming_the_part_still_answers_a_request_for_the_category() {
    // The point of qualifiers: recording *which* accelerometer must not hide
    // the board from someone who just needs an accelerometer.
    let inv = Inventory::from_toml_str(QUALIFIED).unwrap();
    assert!(inv.benches["recorded"]
        .tags
        .contains(&Tag::parse("peripheral=accel").unwrap()));

    let req = claim(&[("dut", &["peripheral=accel"])], Distinct::All);
    assert!(allocate_in(&inv, &req, &no_one_is_busy()).is_ok());
}

#[test]
fn asking_for_an_exact_part_will_not_settle_for_the_category() {
    // The asymmetry that makes qualifiers useful: bench tags are expanded,
    // requirements are not. A board that only claims "an accelerometer" cannot
    // satisfy a test that needs an MPU-6050 specifically.
    let inv = Inventory::from_toml_str(QUALIFIED).unwrap();
    let req = claim(&[("dut", &["peripheral=accel[mpu6050]"])], Distinct::All);
    let alloc = allocate_in(&inv, &req, &no_one_is_busy()).unwrap();
    assert_eq!(alloc.assignment["dut"], "recorded");

    let req = claim(&[("dut", &["peripheral=accel[lis3dh]"])], Distinct::All);
    allocate_in(&inv, &req, &no_one_is_busy())
        .expect_err("no bench carries that part, and the category must not stand in for it");
}

#[test]
fn writing_down_a_part_number_does_not_make_a_bench_look_scarcer() {
    // `recorded` carries both peripheral=accel and peripheral=accel[mpu6050].
    // Charging for each would price one chip twice and quietly teach everyone
    // to stop documenting their hardware.
    let inv = Inventory::from_toml_str(QUALIFIED).unwrap();
    let counts = inv.tag_counts();
    let req = Requirement::parse(["flash=4mb"]).unwrap();
    let weights = inv.vocabulary.key_weights();

    let recorded = fit_cost(&inv.benches["recorded"], &req, &counts, weights);
    let vague = fit_cost(&inv.benches["vague"], &req, &counts, weights);
    assert_eq!(
        recorded, vague,
        "the two boards have the same hardware; only the documentation differs"
    );
    assert!(
        recorded > 0.0,
        "both boards must be charged for the accelerometer the request did not \
         ask for, or this equality says nothing"
    );
}

#[test]
fn asking_for_a_part_is_not_also_charged_for_the_category_it_implies() {
    // The mirror case: having asked for the MPU-6050, the `peripheral=accel`
    // the bench also carries is not spare capability going to waste.
    let inv = Inventory::from_toml_str(QUALIFIED).unwrap();
    let counts = inv.tag_counts();
    let weights = inv.vocabulary.key_weights();

    let exact = Requirement::parse(["peripheral=accel[mpu6050]"]).unwrap();
    let category = Requirement::parse(["peripheral=accel"]).unwrap();
    let asked_for_the_part = fit_cost(&inv.benches["recorded"], &exact, &counts, weights);
    assert_eq!(
        asked_for_the_part,
        fit_cost(&inv.benches["recorded"], &category, &counts, weights),
    );
    assert!(
        asked_for_the_part > 0.0,
        "the board's spare flash must still be priced, or this equality is two \
         zeroes agreeing"
    );
}

#[test]
fn a_qualifier_is_rejected_on_a_key_that_did_not_ask_for_one() {
    // Opt-in per key. Otherwise `flash=4mb[whatever]` becomes a legal tag that
    // nobody would ever think to request.
    let toml = r#"
        [tags.flash]
        [tags.flash.values."4mb"]

        [benches.only]
        tags = ["flash=4mb[winbond]"]
    "#;
    let err = Inventory::from_toml_str(toml).expect_err("qualifier must be opted into");
    assert!(
        err.to_string().contains("do not take a [qualifier]"),
        "{err}"
    );
}

#[test]
fn a_qualified_value_with_an_unknown_category_is_rejected_with_suggestions() {
    // The category is still closed vocabulary; only the part inside the
    // brackets is free-form.
    let inv = Inventory::from_toml_str(QUALIFIED).unwrap();
    let err = inv
        .vocabulary
        .check([&Tag::parse("peripheral=accell[mpu6050]").unwrap()])
        .expect_err("unknown category must be rejected");
    match err {
        TagError::Unknown { suggestions, .. } => assert!(
            suggestions.iter().any(|s| s == "peripheral=accel"),
            "expected the category in {suggestions:?}"
        ),
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn the_vocabulary_may_not_enumerate_parts_itself() {
    // Declaring a part centrally is exactly the curation burden qualifiers
    // exist to remove, so the config that tries it fails loudly.
    let toml = r#"
        [tags.peripheral]
        qualified = true
        [tags.peripheral.values."accel[mpu6050]"]
    "#;
    let err = Inventory::from_toml_str(toml).expect_err("vocabulary must not name parts");
    let msg = err.to_string();
    assert!(msg.contains("must not carry a [qualifier]"), "{msg}");
    assert!(
        msg.contains("peripheral=accel"),
        "should say what to do: {msg}"
    );
}

#[test]
fn a_qualifier_with_a_dash_is_rejected_like_any_other_matchable_value() {
    // Qualifiers are matched on, so they rot the same way capability values do:
    // `mpu-6050` and `mpu6050` would be two different chips.
    let toml = r#"
        [tags.peripheral]
        qualified = true
        [tags.peripheral.values.accel]

        [benches.only]
        tags = ["peripheral=accel[mpu-6050]"]
    "#;
    let err = Inventory::from_toml_str(toml).expect_err("dashed qualifier must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("must not contain a dash"), "{msg}");
    assert!(msg.contains("[mpu6050]"), "should suggest the fix: {msg}");
}

#[test]
fn the_vocabulary_may_not_enumerate_parts_through_an_implication_either() {
    // Declaring `peripheral=accel[mpu6050]` is refused, so the way round it was
    // to imply it: every bench declaring board=devkit then carried a part
    // number nobody looked at the board to establish.
    let toml = r#"
        [tags.peripheral]
        qualified = true
        [tags.peripheral.values.accel]

        [tags.board]
        [tags.board.values.devkit]
        implies = ["peripheral=accel[mpu6050]"]
    "#;
    let err = Inventory::from_toml_str(toml).expect_err("an implied part is still a part");
    let msg = err.to_string();
    assert!(msg.contains("[qualifier]"), "{msg}");
    assert!(
        msg.contains("peripheral=accel"),
        "should say what to do: {msg}"
    );
}

#[test]
fn the_vocabulary_may_not_imply_an_identity() {
    // `name=` is an open key because bench ids cannot be enumerated centrally.
    // That is exactly why the vocabulary must not hand one out: a bench would
    // arrive carrying an identity it was never given.
    let toml = r#"
        [tags.board]
        [tags.board.values.devkit]
        implies = ["name=impostor"]
    "#;
    let err = Inventory::from_toml_str(toml).expect_err("identity is not the vocabulary's to give");
    assert!(err.to_string().contains("open key"), "{err}");
}

#[test]
fn a_bench_may_not_declare_an_identity_tag() {
    // Anyone who can reach the coordinator can register a bench (§9), and
    // `name=` is what operator selection matches on. It is injected from the
    // bench id, never accepted from whoever is describing the hardware.
    let toml = r#"
        [tags.soc]
        [tags.soc.values.esp32s3]

        [benches.impostor]
        tags = ["soc=esp32s3", "name=esp32s3-a"]
    "#;
    let err = Inventory::from_toml_str(toml).expect_err("a bench must not name itself");
    let msg = err.to_string();
    assert!(msg.contains("name=esp32s3-a"), "{msg}");
    assert!(msg.contains("assigned from the bench id"), "{msg}");

    // And a claim may still ask for one: the rule is about who declares
    // identity, not about who matches on it.
    let inv = inventory();
    inv.vocabulary
        .check([&Tag::parse("name=esp32s3-a").unwrap()])
        .expect("an operator may still select a bench by name");
}

#[test]
fn an_open_key_does_not_take_a_qualifier_just_because_it_is_open() {
    // Unbounded values are not structured values. `name=esp32s3-a[spare]` is a
    // second spelling of an identity that must have exactly one.
    let inv = inventory();
    let err = inv
        .vocabulary
        .check([&Tag::parse("name=esp32s3-a[spare]").unwrap()])
        .expect_err("name did not opt into qualifiers");
    assert!(
        err.to_string().contains("do not take a [qualifier]"),
        "{err}"
    );
}

#[test]
fn a_weight_that_is_not_a_usable_number_is_rejected() {
    // A NaN weight makes every cost NaN, every comparison false, and the
    // branch-and-bound bound inert - best fit silently degenerates into
    // whichever assignment the search reached last. A negative weight breaks
    // the non-negativity that bound assumes.
    for bad in ["nan", "-1.0", "inf"] {
        let toml = format!(
            r#"
            [tags.psram]
            weight = {bad}
            [tags.psram.values.octal]
        "#
        );
        let err = Inventory::from_toml_str(&toml)
            .err()
            .unwrap_or_else(|| panic!("weight = {bad} must be rejected"));
        assert!(
            err.to_string().contains("finite and non-negative"),
            "weight = {bad}: {err}"
        );
    }
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
fn two_resources_may_name_one_device_and_differ_only_in_the_node() {
    // A USB-SD-Mux is switched through its SCSI node and written through its
    // block node. Both are the same USB device, which is forwarded once — so
    // the two must resolve to a single grouping key or they would be imported
    // twice.
    let toml = r#"
        [tags.sdmux]
        [tags.sdmux.values.usb]

        [benches.muxed]
        description = "A card the host can hand to the DUT"
        tags = ["sdmux=usb"]
        [benches.muxed.resources.console]
        kind = "serial"
        by_id = "/dev/serial/by-id/fake"
        [benches.muxed.resources.switch]
        kind = "scsi"
        busid = "3-1.1"
        [benches.muxed.resources.card]
        kind = "block"
        busid = "3-1.1"
    "#;

    let inv = Inventory::from_toml_str(toml).unwrap();
    let bench = &inv.benches["muxed"];
    let key = |name: &str| bench.resources[name].device_key(name).to_string();
    assert_eq!(key("switch"), key("card"));
    // And the console, which the coordinator cannot resolve to a busid, keeps
    // a key of its own rather than colliding with either.
    assert_ne!(key("console"), key("card"));
}

#[test]
fn a_resource_that_only_says_usb_does_not_say_enough() {
    // `kind = "usb"` used to mean "the whole device" and could not name which
    // node the agent wanted. Such a bench registered happily and then spent ten
    // seconds at materialisation waiting for a tty that would never appear.
    let toml = r#"
        [tags.sdmux]
        [tags.sdmux.values.usb]

        [benches.muxed]
        description = ""
        tags = ["sdmux=usb"]
        [benches.muxed.resources.card]
        kind = "usb"
        busid = "3-1.1"
    "#;

    let err = Inventory::from_toml_str(toml).unwrap_err().to_string();
    assert!(err.contains("block"), "{err}");
    assert!(err.contains("scsi"), "{err}");
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
fn a_bench_disabled_in_a_loaded_inventory_is_invisible_to_the_matcher() {
    // The only way the flag can be set: a file-loaded inventory. A host cannot
    // declare it, because `BenchSpec` has no such field, and the coordinator
    // builds every registered bench enabled - so this covers the matcher's half
    // of a feature whose other half does not exist yet.
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

#[test]
fn a_search_that_ran_out_of_budget_is_not_reported_as_impossible() {
    // Eleven slots over an inventory shaped so the search's first descent is a
    // dead end: the GPS slot is the most constrained, so it goes first and
    // takes the board that ten other slots then need. That branch is a
    // ten-slots-into-nine-benches pigeonhole, and proving it empty costs more
    // than the node budget allows - so the search stops having found nothing
    // and having established nothing.
    //
    // The claim is satisfiable: give the GPS slot the board with no SoC tag and
    // every other slot has a home. Reporting this as a distinctness conflict
    // makes `unsatisfiable()` true, which reaches the agent as
    // `retryable: false` - "change your request", about a request that was fine.
    let mut toml = String::from(
        r#"
        [tags.soc]
        weight = 0
        [tags.soc.values.esp32s3]

        [tags.peripheral]
        [tags.peripheral.values.gps]

        [tags.jtag]
        [tags.jtag.values.external]

        [benches."b09"]
        tags = ["soc=esp32s3", "peripheral=gps"]

        [benches."b10"]
        tags = ["peripheral=gps", "jtag=external"]
        "#,
    );
    for i in 0..9 {
        toml.push_str(&format!("[benches.\"b0{i}\"]\ntags = [\"soc=esp32s3\"]\n"));
    }
    let inv = Inventory::from_toml_str(&toml).unwrap();

    let mut slots: Vec<(String, Vec<&str>)> = (0..10)
        .map(|i| (format!("s{i:02}"), vec!["soc=esp32s3"]))
        .collect();
    slots.push(("gps".to_string(), vec!["peripheral=gps"]));
    let req = ClaimRequest {
        grouping: benchd_core::model::Grouping::None,
        slots: slots
            .iter()
            .map(|(name, tags)| {
                (
                    name.clone(),
                    Requirement::parse(tags.iter().copied()).unwrap(),
                )
            })
            .collect(),
        distinct: Distinct::All,
        ttl_seconds: 900,
        reason: "eleven boards".into(),
    };

    let err = allocate_in(&inv, &req, &no_one_is_busy()).unwrap_err();
    assert!(err.search_exhausted, "the search should have run out here");
    assert!(
        !err.conflict_only,
        "nothing was proved about distinctness; the search never finished"
    );
    assert!(
        !err.unsatisfiable(),
        "an unfinished search must be retryable: this claim can be satisfied"
    );
    assert!(err.to_string().contains("Retry"), "{err}");
}

// --- properties ------------------------------------------------------------
//
// The example tests above pin specific behaviours; these pin the invariants.
// "Allocation succeeds exactly when a valid assignment exists" is a far
// stronger claim than any example, and it is what stops a future optimisation
// from quietly making the matcher return false negatives.

mod properties {
    use super::*;
    use benchd_core::matcher::allocate;
    use benchd_core::tags::TagSet;
    use proptest::prelude::*;

    /// What a bench may declare.
    ///
    /// A qualified value and two values that imply a third are in here because
    /// the shipped vocabulary has both and the scoring treats both specially.
    /// A generator that produces neither tests the search and nothing else.
    const POOL: &[&str] = &[
        "soc=esp32",
        "soc=esp32s3",
        "psram=octal",
        "jtag=builtin",
        "flash=8mb",
        "peripheral=accel[mpu6050]",
        "peripheral=gps",
    ];

    /// Keys the generator prices, weight 0 included — the entire shipped
    /// vocabulary leans on weight 0 and an all-ones generator never reaches it.
    const KEYS: &[&str] = &["soc", "family", "psram", "jtag", "flash", "peripheral"];
    const WEIGHTS: &[f64] = &[0.0, 1.0, 2.5];

    fn base_of(value: &str) -> &str {
        value.split_once('[').map_or(value, |(base, _)| base)
    }

    fn is_qualified(value: &str) -> bool {
        value.contains('[')
    }

    /// The bench-side closure: what a soc implies, plus the bare category
    /// behind a qualified value.
    ///
    /// Written out rather than taken from `Vocabulary`, like everything else in
    /// this module. A reference that shares the implementation's idea of what a
    /// bench carries, what matches, or what anything costs cannot catch the
    /// implementation being wrong about it — and every bug this file has ever
    /// had was in the scoring, which the old reference imported wholesale.
    fn expand(declared: &[&str]) -> TagSet {
        let mut tags = TagSet::new();
        for text in declared {
            let tag = Tag::parse(text).unwrap();
            if tag.key == "soc" {
                tags.insert(Tag::new("family", "esp32"));
            }
            if is_qualified(&tag.value) {
                tags.insert(Tag::new(tag.key.clone(), base_of(&tag.value)));
            }
            tags.insert(tag);
        }
        tags
    }

    /// Build a bench directly, bypassing config parsing.
    fn bench(id: &str, declared: &[&str], enabled: bool) -> Bench {
        let mut tags = expand(declared);
        tags.insert(Tag::new("name", id));
        Bench {
            group: None,
            id: id.to_string(),
            tags,
            resources: Default::default(),
            description: String::new(),
            docs: String::new(),
            enabled,
        }
    }

    /// Superset matching: a bench will do when it carries everything asked for.
    fn satisfies(bench: &Bench, required: &TagSet) -> bool {
        required.iter().all(|tag| bench.tags.contains(tag))
    }

    /// Whether this slot may share its bench with another.
    ///
    /// `Only` names the slots that must be *pairwise* distinct; a slot outside
    /// the set is free to sit on the same bench as one inside it. That reading
    /// is what "these slots must land on different benches" says, and it is
    /// what the matcher does — worth writing down, because a set of one then
    /// constrains nothing at all.
    fn must_be_distinct(distinct: &Distinct, slot: &str) -> bool {
        match distinct {
            Distinct::All => true,
            Distinct::None => false,
            Distinct::Only(only) => only.contains(slot),
        }
    }

    /// The scarcity denominator: benches this claim could actually be given.
    fn free_counts(benches: &[&Bench], busy: &BTreeMap<String, BusyInfo>) -> BTreeMap<Tag, usize> {
        let mut counts: BTreeMap<Tag, usize> = BTreeMap::new();
        for b in benches
            .iter()
            .filter(|b| b.enabled && !busy.contains_key(&b.id))
        {
            for tag in &b.tags {
                *counts.entry(tag.clone()).or_insert(0) += 1;
            }
        }
        counts
    }

    /// The cost rule, transcribed from the specification.
    ///
    /// Each tag the bench carries that the request did not ask for costs
    /// `weight(key) / free benches carrying that exact tag`. Three things are
    /// not spare capability and cost nothing: the identity tag, which every
    /// bench has exactly one of; a qualified value, whose category is charged
    /// instead, so recording a part number cannot make a board look scarcer
    /// than an identical undocumented one; and the category behind a part the
    /// request named, which is the same chip seen from the other side.
    fn spec_cost(
        bench: &Bench,
        required: &TagSet,
        counts: &BTreeMap<Tag, usize>,
        weights: &BTreeMap<String, f64>,
    ) -> f64 {
        let mut total = 0.0;
        for tag in &bench.tags {
            if required.contains(tag) || tag.key == "name" || is_qualified(&tag.value) {
                continue;
            }
            let stands_for_a_requested_part = required.iter().any(|asked| {
                is_qualified(&asked.value)
                    && asked.key == tag.key
                    && base_of(&asked.value) == tag.value
            });
            if stands_for_a_requested_part {
                continue;
            }
            if let Some(&count) = counts.get(tag) {
                if count > 0 {
                    total += weights.get(&tag.key).copied().unwrap_or(1.0) / count as f64;
                }
            }
        }
        total
    }

    /// Cheapest total cost over every assignment, by enumeration. Deliberately
    /// dumb, so it is obviously correct.
    fn cheapest(
        request: &ClaimRequest,
        benches: &[&Bench],
        busy: &BTreeMap<String, BusyInfo>,
        weights: &BTreeMap<String, f64>,
    ) -> Option<f64> {
        let counts = free_counts(benches, busy);
        let free: Vec<&Bench> = benches
            .iter()
            .copied()
            .filter(|b| b.enabled && !busy.contains_key(&b.id))
            .collect();
        let slots: Vec<&String> = request.slots.keys().collect();

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
            let required = &request.slots[slot].tags;
            let exclusive = must_be_distinct(&request.distinct, slot);
            for b in free {
                if !satisfies(b, required) {
                    continue;
                }
                if exclusive && used.contains(&b.id) {
                    continue;
                }
                if exclusive {
                    used.push(b.id.clone());
                }
                let c = spec_cost(b, required, counts, weights);
                go(
                    i + 1,
                    slots,
                    request,
                    free,
                    used,
                    counts,
                    weights,
                    acc + c,
                    best,
                );
                if exclusive {
                    used.pop();
                }
            }
        }

        let mut best = None;
        go(
            0,
            &slots,
            request,
            &free,
            &mut Vec::new(),
            &counts,
            weights,
            0.0,
            &mut best,
        );
        best
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        /// The matcher succeeds exactly when a valid assignment exists; when it
        /// succeeds the assignment is optimal and the cost it reports is the
        /// cost of that assignment; and when it fails it says which kind of
        /// failure it was.
        #[test]
        fn allocation_agrees_with_an_independent_reference(
            bench_masks in prop::collection::vec(0u8..128, 1..6),
            slot_masks in prop::collection::vec(0u8..128, 1..4),
            weight_choices in prop::collection::vec(0usize..3, KEYS.len()),
            distinct_mode in 0u8..3,
            distinct_mask in 0u8..8,
            busy_mask in 0u8..32,
            disabled_mask in 0u8..32,
        ) {
            let benches: Vec<Bench> = bench_masks
                .iter()
                .enumerate()
                .map(|(i, mask)| {
                    let declared: Vec<&str> = POOL
                        .iter()
                        .enumerate()
                        .filter(|(j, _)| mask & (1 << j) != 0)
                        .map(|(_, t)| *t)
                        .collect();
                    bench(&format!("b{i}"), &declared, disabled_mask & (1 << i) == 0)
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
                        .map(|(_, t)| Tag::parse(t).unwrap())
                        .collect();
                    (format!("s{i}"), Requirement::new(tags))
                })
                .collect();

            let distinct = match distinct_mode {
                0 => Distinct::All,
                1 => Distinct::None,
                _ => Distinct::Only(
                    slots
                        .keys()
                        .enumerate()
                        .filter(|(i, _)| distinct_mask & (1 << i) != 0)
                        .map(|(_, slot)| slot.clone())
                        .collect(),
                ),
            };

            let weights: BTreeMap<String, f64> = KEYS
                .iter()
                .zip(&weight_choices)
                .map(|(key, choice)| (key.to_string(), WEIGHTS[*choice]))
                .collect();

            let busy: BTreeMap<String, BusyInfo> = benches
                .iter()
                .enumerate()
                .filter(|(i, _)| busy_mask & (1 << i) != 0)
                .map(|(_, b)| (b.id.clone(), BusyInfo {
                    owner: "someone-else".into(),
                    expires_in: Some(30.0),
                    reason: "held".into(),
                }))
                .collect();

            let request = ClaimRequest {
                grouping: benchd_core::model::Grouping::None,
                slots,
                distinct,
                ttl_seconds: 60,
                reason: String::new(),
            };

            let expected = cheapest(&request, &refs, &busy, &weights);
            let actual = allocate(&request, &refs, &busy, &weights);

            match (expected, actual) {
                (Some(best), Ok(alloc)) => {
                    prop_assert!(
                        (alloc.cost - best).abs() < 1e-9,
                        "matcher returned cost {} but optimum is {best}",
                        alloc.cost
                    );

                    // The assignment must be valid, and the cost it was sold
                    // under must be the cost of the benches it actually names.
                    let counts = free_counts(&refs, &busy);
                    let mut charged = 0.0;
                    let mut used: Vec<&String> = Vec::new();
                    for (slot, bench_id) in &alloc.assignment {
                        let b = benches.iter().find(|b| &b.id == bench_id).unwrap();
                        prop_assert!(b.enabled, "{bench_id} is disabled");
                        prop_assert!(!busy.contains_key(bench_id), "{bench_id} is held");
                        prop_assert!(satisfies(b, &request.slots[slot].tags));
                        if must_be_distinct(&request.distinct, slot) {
                            prop_assert!(!used.contains(&bench_id));
                            used.push(bench_id);
                        }
                        charged += spec_cost(b, &request.slots[slot].tags, &counts, &weights);
                    }
                    prop_assert_eq!(alloc.assignment.len(), request.slots.len());
                    prop_assert!(
                        (alloc.cost - charged).abs() < 1e-9,
                        "reported cost {} is not what this assignment costs ({charged})",
                        alloc.cost
                    );
                }
                (None, Err(err)) => {
                    prop_assert!(
                        !err.search_exhausted,
                        "a handful of benches cannot exhaust the node budget"
                    );
                    // Waiting helps only if the whole request has a structural
                    // assignment, including benches currently held by others.
                    let per_slot_candidates = request.slots.iter().all(|(_, requirement)| {
                        refs.iter().any(|b| b.enabled && satisfies(b, &requirement.tags))
                    }) && cheapest(&request, &refs, &BTreeMap::new(), &weights).is_none();
                    prop_assert_eq!(
                        err.conflict_only,
                        per_slot_candidates,
                        "misclassified failure: {}",
                        err
                    );
                }
                (None, Ok(a)) => prop_assert!(false, "matched {a:?} when nothing should match"),
                (Some(b), Err(e)) => prop_assert!(false, "no match (cost {b} exists): {e}"),
            }
        }
    }
}
