//! Matching benches to requirements.
//!
//! Three properties matter here, in order:
//!
//! 1. **Superset matching.** A bench matches when its tags are a superset of the
//!    requirement's. That is the whole contract agents see.
//! 2. **Best fit, not first fit.** Among adequate benches, prefer the *least
//!    capable* one, scored by how scarce the capabilities it would waste are.
//!    Handing the lab's only JTAG bench to an agent that asked for a blinking
//!    LED is how a lab deadlocks at 3am.
//! 3. **Atomic multi-slot allocation.** Either every slot is satisfied or none
//!    is. Greedy per-slot assignment can fail to find an assignment that exists,
//!    so we solve it exactly.
//!
//! When allocation fails, the *diagnosis* matters more than the failure: an
//! agent must be able to tell "no such bench will ever exist" (change your
//! request) from "they're all busy" (wait). Conflating those makes agents
//! retry-spin forever on impossible requests.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::model::{Bench, ClaimRequest, Requirement};
use crate::tags::{format_tags, Tag, TagSet};

/// Hard cap on the assignment search. Slots are single digits and benches are
/// tens in any realistic lab; this exists only so a pathological inventory
/// degrades to an error rather than hanging the broker.
const SEARCH_NODE_LIMIT: u64 = 200_000;

/// Requirements with more tags than this skip the "largest satisfiable subset"
/// analysis. 2^12 masks is instant; beyond that the request is absurd anyway.
const MAX_SUBSET_ANALYSIS_TAGS: usize = 12;

/// Why a bench is unavailable, for near-miss reporting.
#[derive(Clone, Debug, PartialEq)]
pub struct BusyInfo {
    pub owner: String,
    /// Seconds until the current lease expires, if it has a deadline.
    pub expires_in: Option<f64>,
    pub reason: String,
}

/// A satisfiable assignment of slots to benches.
#[derive(Clone, Debug, PartialEq)]
pub struct Allocation {
    pub assignment: BTreeMap<String, String>,
    pub cost: f64,
}

/// Whether a slot can never be satisfied, or merely is not satisfiable now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// No such bench exists, ever. The agent must change its request.
    Unsatisfiable,
    /// Benches exist but are all held. The agent should wait.
    Contended,
}

/// Why one slot could not be filled.
#[derive(Clone, Debug)]
pub struct SlotDiagnosis {
    pub slot: String,
    pub failure: Failure,
    pub required: TagSet,
    /// Benches matching the requirement regardless of availability.
    pub matching: Vec<String>,
    /// Of those, the ones currently free.
    pub free: Vec<String>,
    /// Required tags that no enabled bench carries at all.
    pub impossible_tags: TagSet,
    /// Largest subset of the requirement that *does* match something...
    pub closest_satisfiable: TagSet,
    /// ...and the tags you would have to drop to get there.
    pub drop_to_match: TagSet,
    pub holders: BTreeMap<String, BusyInfo>,
}

impl SlotDiagnosis {
    /// Soonest a matching bench frees up, if any holder has a deadline.
    pub fn earliest_free(&self) -> Option<f64> {
        self.holders
            .values()
            .filter_map(|info| info.expires_in)
            .min_by(f64::total_cmp)
    }
}

impl fmt::Display for SlotDiagnosis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let want = if self.required.is_empty() {
            "(no tags)".to_string()
        } else {
            format_tags(&self.required)
        };

        match self.failure {
            Failure::Unsatisfiable => {
                write!(
                    f,
                    "slot {:?}: no bench exists matching {{{want}}}",
                    self.slot
                )?;
                if !self.impossible_tags.is_empty() {
                    write!(
                        f,
                        "\n  no bench has: {}",
                        format_tags(&self.impossible_tags)
                    )?;
                }
                if !self.drop_to_match.is_empty() {
                    write!(
                        f,
                        "\n  drop {} -> matches {{{}}}",
                        format_tags(&self.drop_to_match),
                        format_tags(&self.closest_satisfiable)
                    )?;
                } else if self.impossible_tags.is_empty() {
                    write!(f, "\n  the combination of these tags exists on no bench")?;
                }
                Ok(())
            }
            Failure::Contended => {
                write!(
                    f,
                    "slot {:?}: {} bench(es) match {{{want}}}, {} free",
                    self.slot,
                    self.matching.len(),
                    self.free.len()
                )?;
                for bench_id in &self.matching {
                    match self.holders.get(bench_id) {
                        None => write!(f, "\n  {bench_id}: free")?,
                        Some(info) => {
                            write!(f, "\n  {bench_id}: held by {}", info.owner)?;
                            if let Some(secs) = info.expires_in {
                                write!(f, ", expires in {}s", secs.max(0.0) as i64)?;
                            }
                        }
                    }
                }
                Ok(())
            }
        }
    }
}

/// A claim that could not be satisfied, carrying a full diagnosis.
#[derive(Clone, Debug)]
pub struct NoMatch {
    pub slots: Vec<SlotDiagnosis>,
    /// True when every slot is individually available, but no assignment
    /// satisfies them all at once under the distinctness rule.
    pub conflict_only: bool,
    /// True when the assignment search hit its node limit before reaching any
    /// complete assignment, so nothing is known about whether one exists.
    ///
    /// Kept apart from `conflict_only` because the two say opposite things to
    /// an agent. "No assignment exists" means change the request; "we stopped
    /// looking" means try again, and conflating them told an agent its
    /// perfectly satisfiable claim was impossible.
    pub search_exhausted: bool,
}

impl NoMatch {
    /// True if retrying unchanged can never succeed.
    ///
    /// `conflict_only` counts. Two slots that both match exactly one bench,
    /// which is *free*, fail on distinctness — and no amount of waiting fixes
    /// that, because nothing is busy. Reporting it as merely contended told an
    /// agent to retry a request that can never succeed, which is precisely the
    /// spin D14 exists to prevent.
    ///
    /// An exhausted search is the mirror image: we proved nothing, so the only
    /// honest answer is "retryable".
    pub fn unsatisfiable(&self) -> bool {
        if self.search_exhausted {
            return false;
        }
        self.conflict_only
            || self
                .slots
                .iter()
                .any(|s| s.failure == Failure::Unsatisfiable)
    }

    /// Soonest the *whole* claim could be satisfiable: every slot must free up,
    /// so this is the max over slots of each slot's earliest free bench.
    pub fn earliest_free(&self) -> Option<f64> {
        self.slots
            .iter()
            .map(SlotDiagnosis::earliest_free)
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .max_by(f64::total_cmp)
    }
}

impl fmt::Display for NoMatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.conflict_only {
            writeln!(
                f,
                "no assignment satisfies all slots at once: these slots must land on \
                 different benches, and there are not enough distinct benches that match. \
                 Waiting will not help - relax a slot's tags, or set distinct=false if \
                 sharing one bench is acceptable."
            )?;
        }
        if self.search_exhausted {
            writeln!(
                f,
                "the search for an assignment gave up after {SEARCH_NODE_LIMIT} steps \
                 without finishing, so this is not a claim that no assignment exists. \
                 Retry, or ask for fewer slots at once."
            )?;
        }
        for (i, slot) in self.slots.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{slot}")?;
        }
        Ok(())
    }
}

impl std::error::Error for NoMatch {}

/// Cost of using `bench` for `requirement`: the scarcity it wastes.
///
/// Every capability the bench has that the requirement did not ask for
/// contributes `weight(key) / (benches carrying that tag)`. A capability only
/// one bench in the lab can supply is expensive to squander; one every bench
/// has is free. `tag_counts` must therefore count *allocatable* benches, not
/// every bench that exists — see [`allocate`]. Lower is a better fit.
///
/// The per-key weight matters more than it looks, and it is the only knob:
/// there are no per-value weights and no ordering between values, because
/// matching is exact per tag and nothing here knows that 8mb is "more" than
/// 4mb.
///
/// Two things follow, and the vocabulary has to hold up both ends:
///
/// * Scarcity conflates *rare* with *valuable*. Being the lab's only CP2102N
///   board makes `console=uart` unique but not precious, and unweighted
///   scoring would therefore protect the cheapest board most. So keys that
///   *describe* a board — soc, family, arch, console, flash — carry weight 0,
///   and only keys naming a capability a claim can waste (peripheral, jtag,
///   psram, sdmux) keep weight 1.
/// * The denominator counts benches carrying that exact `key=value`, so a key
///   whose values are ordinal prices the rarity of the value rather than any
///   capability: `flash=4mb` on one board cost twice `flash=8mb` on two, which
///   is "least capable wins" running backwards. Such a key belongs at weight 0.
///
/// A value naming the *absence* of something (`psram=none`, `jtag=none`) is
/// charged exactly like a real capability and so must not exist; a board
/// without the hardware omits the key. The shipped vocabularies say so where
/// the temptation is.
pub fn fit_cost(
    bench: &Bench,
    requirement: &Requirement,
    tag_counts: &BTreeMap<Tag, usize>,
    weights: &BTreeMap<String, f64>,
) -> f64 {
    bench
        .tags
        .difference(&requirement.tags)
        // Bench and host identities carry no capability meaning and would
        // otherwise make rare names dominate the score.
        .filter(|tag| tag.key != "name" && tag.key != "host")
        // One chip, one charge. A bench declaring `peripheral=accel[mpu6050]`
        // carries the bare `peripheral=accel` too, so scoring both would make a
        // board look twice as capable purely because someone recorded its part
        // number. The category is the capability; the qualifier only says which
        // part provides it.
        .filter(|tag| tag.qualifier().is_none())
        // The mirror case: when the request named an exact part, the category
        // that part implies is not spare capability either.
        .filter(|tag| {
            !requirement
                .tags
                .iter()
                .any(|asked| asked.qualifier().is_some() && &asked.base_tag() == *tag)
        })
        .filter_map(|tag| {
            let weight = weights.get(&tag.key).copied().unwrap_or(1.0);
            if weight == 0.0 {
                return None;
            }
            match tag_counts.get(tag) {
                Some(&count) if count > 0 => Some(weight / count as f64),
                _ => None,
            }
        })
        .sum()
}

/// Assign every slot in `request` to a distinct free bench, or explain why not.
///
/// The scarcity counts the scoring needs are derived here rather than passed
/// in, because they have to be taken over the benches this call could actually
/// hand out: `busy` is the difference between "the lab owns two JTAG boards"
/// and "one JTAG board is available", and pricing the last free one at half
/// its worth spends it on a claim that never asked for JTAG.
pub fn allocate(
    request: &ClaimRequest,
    benches: &[&Bench],
    busy: &BTreeMap<String, BusyInfo>,
    weights: &BTreeMap<String, f64>,
) -> Result<Allocation, NoMatch> {
    let enabled: Vec<&Bench> = benches.iter().copied().filter(|b| b.enabled).collect();
    let by_id: BTreeMap<&str, &Bench> = enabled.iter().map(|b| (b.id.as_str(), *b)).collect();

    let mut tag_counts: BTreeMap<Tag, usize> = BTreeMap::new();
    for bench in enabled.iter().filter(|b| !busy.contains_key(&b.id)) {
        for tag in &bench.tags {
            *tag_counts.entry(tag.clone()).or_insert(0) += 1;
        }
    }

    // Per-slot candidate sets.
    let mut matching: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut free: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (slot, requirement) in &request.slots {
        let hits: Vec<String> = enabled
            .iter()
            .filter(|b| requirement.matches(b))
            .map(|b| b.id.clone())
            .collect();
        let available = hits
            .iter()
            .filter(|id| !busy.contains_key(*id))
            .cloned()
            .collect();
        matching.insert(slot.as_str(), hits);
        free.insert(slot.as_str(), available);
    }

    // Any slot with no free candidate fails the whole claim. Diagnose all of
    // them at once so the agent can fix everything in one turn.
    let failing: Vec<&str> = request
        .slots
        .keys()
        .map(String::as_str)
        .filter(|slot| free[slot].is_empty())
        .collect();
    if !failing.is_empty() {
        return Err(NoMatch {
            slots: failing
                .into_iter()
                .map(|slot| {
                    diagnose(
                        slot,
                        &request.slots[slot],
                        &enabled,
                        &matching[slot],
                        &free[slot],
                        busy,
                    )
                })
                .collect(),
            conflict_only: false,
            search_exhausted: false,
        });
    }

    let searched = best_assignment(request, &free, &by_id, &tag_counts, weights);
    match searched.best {
        Some(allocation) => Ok(allocation),
        None => {
            // Every slot had a free candidate, so unless the search gave up
            // early this is purely a distinctness conflict: e.g. two slots that
            // both only match the same bench.
            Err(NoMatch {
                slots: request
                    .slots
                    .keys()
                    .map(|slot| {
                        diagnose(
                            slot,
                            &request.slots[slot],
                            &enabled,
                            &matching[slot.as_str()],
                            &free[slot.as_str()],
                            busy,
                        )
                    })
                    .collect(),
                conflict_only: !searched.exhausted,
                search_exhausted: searched.exhausted,
            })
        }
    }
}

/// What one assignment search came back with.
struct Searched {
    best: Option<Allocation>,
    /// The node budget ran out, so `best` is whatever had been found by then
    /// and `None` means only that the search stopped looking.
    exhausted: bool,
}

/// Exact minimum-cost assignment under the distinctness constraint.
///
/// Depth-first with branch-and-bound, slots ordered most-constrained-first so
/// conflicts surface at shallow depth. Exhaustive rather than greedy because
/// greedy can report failure for a request that *is* satisfiable, and a false
/// "no bench available" is the worst possible answer here.
///
/// Whether the search finished is as much of an answer as what it found: with
/// an assignment in hand a truncated search only risks a suboptimal fit, but
/// with none it has proved nothing at all, and the caller must not report that
/// as "impossible".
fn best_assignment(
    request: &ClaimRequest,
    free: &BTreeMap<&str, Vec<String>>,
    by_id: &BTreeMap<&str, &Bench>,
    tag_counts: &BTreeMap<Tag, usize>,
    weights: &BTreeMap<String, f64>,
) -> Searched {
    let mut cost: BTreeMap<(&str, &str), f64> = BTreeMap::new();
    for (slot, requirement) in &request.slots {
        for bench_id in &free[slot.as_str()] {
            let bench = by_id[bench_id.as_str()];
            cost.insert(
                (slot.as_str(), bench_id.as_str()),
                fit_cost(bench, requirement, tag_counts, weights),
            );
        }
    }

    // Most constrained slot first, then cheapest candidate first: finds a good
    // bound early, which prunes hard.
    let mut order: Vec<&str> = request.slots.keys().map(String::as_str).collect();
    order.sort_by_key(|slot| (free[slot].len(), *slot));

    let mut candidates: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for slot in &order {
        let mut ids: Vec<&str> = free[slot].iter().map(String::as_str).collect();
        ids.sort_by(|a, b| {
            cost[&(*slot, *a)]
                .total_cmp(&cost[&(*slot, *b)])
                .then_with(|| a.cmp(b))
        });
        candidates.insert(slot, ids);
    }

    struct Search<'a> {
        order: Vec<&'a str>,
        candidates: BTreeMap<&'a str, Vec<&'a str>>,
        cost: BTreeMap<(&'a str, &'a str), f64>,
        distinct: &'a crate::model::Distinct,
        best: Option<Allocation>,
        nodes: u64,
        exhausted: bool,
    }

    fn recurse(
        s: &mut Search<'_>,
        index: usize,
        used: &mut BTreeSet<String>,
        acc: &mut BTreeMap<String, String>,
        total: f64,
    ) {
        s.nodes += 1;
        if s.nodes > SEARCH_NODE_LIMIT {
            // Safety valve; the caller sees whatever bound we found, and is
            // told the answer is incomplete.
            s.exhausted = true;
            return;
        }
        if let Some(best) = &s.best {
            if total >= best.cost {
                return;
            }
        }
        if index == s.order.len() {
            s.best = Some(Allocation {
                assignment: acc.clone(),
                cost: total,
            });
            return;
        }
        let slot = s.order[index];
        let must_be_distinct = s.distinct.applies_to(slot);
        for bench_id in s.candidates[slot].clone() {
            if must_be_distinct && used.contains(bench_id) {
                continue;
            }
            acc.insert(slot.to_string(), bench_id.to_string());
            if must_be_distinct {
                used.insert(bench_id.to_string());
            }
            recurse(s, index + 1, used, acc, total + s.cost[&(slot, bench_id)]);
            if must_be_distinct {
                used.remove(bench_id);
            }
            acc.remove(slot);
        }
    }

    let mut search = Search {
        order,
        candidates,
        cost,
        distinct: &request.distinct,
        best: None,
        nodes: 0,
        exhausted: false,
    };
    recurse(
        &mut search,
        0,
        &mut BTreeSet::new(),
        &mut BTreeMap::new(),
        0.0,
    );
    Searched {
        best: search.best,
        exhausted: search.exhausted,
    }
}

/// Classify a slot failure as unsatisfiable or merely contended.
fn diagnose(
    slot: &str,
    requirement: &Requirement,
    benches: &[&Bench],
    matching: &[String],
    free: &[String],
    busy: &BTreeMap<String, BusyInfo>,
) -> SlotDiagnosis {
    if !matching.is_empty() {
        return SlotDiagnosis {
            slot: slot.to_string(),
            failure: Failure::Contended,
            required: requirement.tags.clone(),
            matching: matching.to_vec(),
            free: free.to_vec(),
            impossible_tags: TagSet::new(),
            closest_satisfiable: TagSet::new(),
            drop_to_match: TagSet::new(),
            holders: matching
                .iter()
                .filter_map(|id| busy.get(id).map(|info| (id.clone(), info.clone())))
                .collect(),
        };
    }

    // Nothing matches at all: work out why, so the agent can relax its request
    // instead of guessing.
    let impossible: TagSet = requirement
        .tags
        .iter()
        .filter(|tag| !benches.iter().any(|b| b.tags.contains(*tag)))
        .cloned()
        .collect();
    let (closest, drop) = largest_satisfiable_subset(&requirement.tags, benches);

    SlotDiagnosis {
        slot: slot.to_string(),
        failure: Failure::Unsatisfiable,
        required: requirement.tags.clone(),
        matching: Vec::new(),
        free: Vec::new(),
        impossible_tags: impossible,
        closest_satisfiable: closest,
        drop_to_match: drop,
        holders: BTreeMap::new(),
    }
}

/// Biggest subset of `required` that some bench satisfies.
///
/// Returns `(subset, tags_to_drop)`. Enumerated by bitmask and selected by
/// popcount, so the answer is maximal. Requirements are a handful of tags, so
/// the search is bounded by requirement size, not bench count.
fn largest_satisfiable_subset(required: &TagSet, benches: &[&Bench]) -> (TagSet, TagSet) {
    let tags: Vec<&Tag> = required.iter().collect();
    let n = tags.len();
    if n == 0 || n > MAX_SUBSET_ANALYSIS_TAGS {
        return (TagSet::new(), TagSet::new());
    }

    let mut best: Option<TagSet> = None;
    let mut best_len = 0;
    // Skip the full set (it is known not to match) and the empty set.
    for mask in 1u32..(1u32 << n) - 1 {
        let len = mask.count_ones() as usize;
        if len <= best_len {
            continue;
        }
        let subset: TagSet = (0..n)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| tags[i].clone())
            .collect();
        if benches.iter().any(|b| subset.is_subset(&b.tags)) {
            best_len = len;
            best = Some(subset);
        }
    }

    match best {
        Some(subset) => {
            let drop = required.difference(&subset).cloned().collect();
            (subset, drop)
        }
        None => (TagSet::new(), TagSet::new()),
    }
}
