"""Matching benches to requirements.

Three properties matter here, in order:

1. **Superset matching.** A bench matches when its tags are a superset of the
   requirement's. That is the whole contract agents see.
2. **Best fit, not first fit.** Among adequate benches, prefer the *least
   capable* one, scored by how scarce the capabilities it would waste are.
   Handing the lab's only JTAG bench to an agent that asked for a blinking LED
   is how a lab deadlocks at 3am.
3. **Atomic multi-slot allocation.** Either every slot is satisfied or none is.
   Greedy per-slot assignment can fail to find an assignment that exists, so we
   solve it exactly.

When allocation fails, the *diagnosis* matters more than the failure: an agent
must be able to tell "no such bench will ever exist" (change your request)
from "they're all busy" (wait). Conflating those makes agents retry-spin
forever on impossible requests.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from itertools import combinations
from typing import Iterable, Mapping

from .model import Bench, ClaimRequest, Requirement
from .tags import Tag, format_tag, format_tags

__all__ = [
    "BusyInfo",
    "Allocation",
    "SlotDiagnosis",
    "NoMatch",
    "fit_cost",
    "allocate",
]

#: Hard cap on the assignment search. Slots are single digits and benches are
#: tens in any realistic lab; this only exists so a pathological inventory
#: degrades to an error rather than hanging the broker.
_SEARCH_NODE_LIMIT = 200_000


@dataclass(frozen=True)
class BusyInfo:
    """Why a bench is unavailable, for near-miss reporting."""

    owner: str
    expires_at: float | None = None
    reason: str = ""


@dataclass(frozen=True)
class Allocation:
    """A satisfiable assignment of slots to benches."""

    assignment: dict[str, str]
    cost: float

    def bench_ids(self) -> list[str]:
        return [self.assignment[s] for s in sorted(self.assignment)]


@dataclass
class SlotDiagnosis:
    """Why one slot could not be filled."""

    slot: str
    #: "unsatisfiable" -> no such bench exists, ever. Change the request.
    #: "contended"     -> benches exist but are all held. Wait.
    kind: str
    required: frozenset[Tag]
    #: Benches matching the requirement regardless of availability.
    matching: list[str] = field(default_factory=list)
    #: Of those, the ones currently free.
    free: list[str] = field(default_factory=list)
    #: Required tags that no enabled bench carries at all.
    impossible_tags: frozenset[Tag] = frozenset()
    #: Largest subset of the requirement that *does* match something, and the
    #: tags you would have to drop to get there.
    closest_satisfiable: frozenset[Tag] = frozenset()
    drop_to_match: frozenset[Tag] = frozenset()
    holders: dict[str, BusyInfo] = field(default_factory=dict)

    @property
    def unsatisfiable(self) -> bool:
        return self.kind == "unsatisfiable"

    def earliest_free(self) -> float | None:
        times = [
            info.expires_at
            for info in self.holders.values()
            if info.expires_at is not None
        ]
        return min(times) if times else None

    def render(self) -> str:
        want = format_tags(self.required) or "(no tags)"
        if self.unsatisfiable:
            lines = [f"slot {self.slot!r}: no bench exists matching {{{want}}}"]
            if self.impossible_tags:
                lines.append(
                    f"  no bench has: {format_tags(self.impossible_tags)}"
                )
            if self.drop_to_match:
                lines.append(
                    f"  drop {format_tags(self.drop_to_match)} "
                    f"-> matches {{{format_tags(self.closest_satisfiable)}}}"
                )
            elif not self.impossible_tags:
                lines.append("  the combination of these tags exists on no bench")
            return "\n".join(lines)

        lines = [
            f"slot {self.slot!r}: {len(self.matching)} bench(es) match "
            f"{{{want}}}, {len(self.free)} free"
        ]
        for bench_id in self.matching:
            info = self.holders.get(bench_id)
            if info is None:
                lines.append(f"  {bench_id}: free")
                continue
            when = (
                f", expires in {max(0, int(info.expires_at))}s"
                if info.expires_at is not None
                else ""
            )
            lines.append(f"  {bench_id}: held by {info.owner}{when}")
        return "\n".join(lines)


@dataclass
class NoMatch(Exception):
    """Raised when a claim cannot be satisfied, carrying a full diagnosis."""

    slots: list[SlotDiagnosis]
    #: True when every slot is individually satisfiable and free, but no
    #: assignment satisfies them all at once under the distinctness rule.
    conflict_only: bool = False

    @property
    def unsatisfiable(self) -> bool:
        """True if any slot can never be satisfied. Do not retry."""
        return any(s.unsatisfiable for s in self.slots)

    def earliest_free(self) -> float | None:
        times = [t for t in (s.earliest_free() for s in self.slots) if t is not None]
        return max(times) if times else None

    def render(self) -> str:
        if self.conflict_only:
            head = (
                "no assignment satisfies all slots at once "
                "(slots are individually available but must be distinct)"
            )
            return "\n".join([head, *(s.render() for s in self.slots)])
        return "\n".join(s.render() for s in self.slots)

    def __str__(self) -> str:  # pragma: no cover - convenience
        return self.render()


def fit_cost(
    bench: Bench,
    requirement: Requirement,
    tag_counts: Mapping[Tag, int],
    weights: Mapping[str, float] | None = None,
) -> float:
    """Cost of using ``bench`` for ``requirement``: the scarcity it wastes.

    Every capability the bench has that the requirement did not ask for
    contributes ``weight(key) / (benches carrying that tag)``. A capability
    only one bench in the lab has is expensive to squander; one that every
    bench has is free. Lower is a better fit.

    The per-key weight matters more than it looks. Scarcity alone conflates
    *rare* with *valuable*: being the lab's only CP2102N board makes
    ``usb=cp2102n`` unique but not precious, and unweighted scoring would
    therefore protect the cheapest board most. Identity keys (soc, arch, usb)
    should be weighted 0; genuinely contended peripherals (jtag probes, RF
    chambers, PSRAM) keep weight 1.
    """
    wasted = bench.tags - requirement.tags
    cost = 0.0
    for tag in wasted:
        key = tag[0]
        if key == "name":
            # Every bench has exactly one name tag; it carries no capability
            # meaning and would otherwise dominate the score.
            continue
        weight = 1.0 if weights is None else weights.get(key, 1.0)
        if weight == 0.0:
            continue
        count = tag_counts.get(tag, 0)
        if count <= 0:
            continue
        cost += weight / count
    return cost


def allocate(
    request: ClaimRequest,
    benches: Iterable[Bench],
    busy: Mapping[str, BusyInfo] | None = None,
    tag_counts: Mapping[Tag, int] | None = None,
    weights: Mapping[str, float] | None = None,
) -> Allocation:
    """Assign every slot in ``request`` to a distinct free bench, or raise.

    Raises :class:`NoMatch` with a per-slot diagnosis on failure.
    """
    busy = dict(busy or {})
    bench_list = [b for b in benches if b.enabled]
    by_id = {b.id: b for b in bench_list}

    if tag_counts is None:
        counts: dict[Tag, int] = {}
        for bench in bench_list:
            for tag in bench.tags:
                counts[tag] = counts.get(tag, 0) + 1
        tag_counts = counts

    # Per-slot candidate sets.
    matching: dict[str, list[str]] = {}
    free: dict[str, list[str]] = {}
    for slot, requirement in request.slots.items():
        hits = [b.id for b in bench_list if requirement.matches(b)]
        matching[slot] = sorted(hits)
        free[slot] = sorted(b for b in hits if b not in busy)

    # Any slot with no free candidate fails the whole claim; diagnose all of
    # them at once so the agent can fix everything in one turn.
    failing = [s for s in request.slots if not free[s]]
    if failing:
        raise NoMatch(
            slots=[
                _diagnose(
                    slot,
                    request.slots[slot],
                    bench_list,
                    matching[slot],
                    free[slot],
                    busy,
                )
                for slot in failing
            ]
        )

    distinct = request.distinct_slots()
    result = _best_assignment(
        slots=list(request.slots),
        candidates=free,
        distinct=distinct,
        requirements=request.slots,
        by_id=by_id,
        tag_counts=tag_counts,
        weights=weights,
    )
    if result is None:
        # Every slot had a free candidate, so this is purely a distinctness
        # conflict: e.g. two slots that both only match the same single bench.
        raise NoMatch(
            slots=[
                _diagnose(
                    slot,
                    request.slots[slot],
                    bench_list,
                    matching[slot],
                    free[slot],
                    busy,
                )
                for slot in request.slots
            ],
            conflict_only=True,
        )
    assignment, cost = result
    return Allocation(assignment=assignment, cost=cost)


def _best_assignment(
    slots: list[str],
    candidates: Mapping[str, list[str]],
    distinct: frozenset[str],
    requirements: Mapping[str, Requirement],
    by_id: Mapping[str, Bench],
    tag_counts: Mapping[Tag, int],
    weights: Mapping[str, float] | None = None,
) -> tuple[dict[str, str], float] | None:
    """Exact minimum-cost assignment under the distinctness constraint.

    Depth-first with branch-and-bound. Slots are ordered most-constrained-first
    so conflicts surface at shallow depth. Exhaustive rather than greedy
    because greedy can report failure for a request that is satisfiable, and a
    false "no bench available" is the worst possible answer here.
    """
    # Precompute per-(slot, bench) cost.
    cost: dict[tuple[str, str], float] = {}
    for slot in slots:
        req = requirements[slot]
        for bench_id in candidates[slot]:
            cost[(slot, bench_id)] = fit_cost(
                by_id[bench_id], req, tag_counts, weights
            )

    order = sorted(slots, key=lambda s: (len(candidates[s]), s))

    best: list[tuple[dict[str, str], float] | None] = [None]
    nodes = [0]

    def recurse(index: int, used: set[str], acc: dict[str, str], total: float) -> None:
        nodes[0] += 1
        if nodes[0] > _SEARCH_NODE_LIMIT:  # pragma: no cover - safety valve
            raise RuntimeError(
                "bench assignment search exceeded node limit; "
                "inventory or request is pathological"
            )
        if best[0] is not None and total >= best[0][1]:
            return  # bound
        if index == len(order):
            best[0] = (dict(acc), total)
            return
        slot = order[index]
        must_be_distinct = slot in distinct
        # Cheapest candidates first: finds a good bound early, prunes hard.
        for bench_id in sorted(candidates[slot], key=lambda b: (cost[(slot, b)], b)):
            if must_be_distinct and bench_id in used:
                continue
            acc[slot] = bench_id
            if must_be_distinct:
                used.add(bench_id)
            recurse(index + 1, used, acc, total + cost[(slot, bench_id)])
            if must_be_distinct:
                used.discard(bench_id)
            del acc[slot]

    recurse(0, set(), {}, 0.0)
    return best[0]


def _diagnose(
    slot: str,
    requirement: Requirement,
    benches: list[Bench],
    matching: list[str],
    free: list[str],
    busy: Mapping[str, BusyInfo],
) -> SlotDiagnosis:
    """Classify a slot failure as unsatisfiable or merely contended."""
    if matching:
        return SlotDiagnosis(
            slot=slot,
            kind="contended",
            required=requirement.tags,
            matching=matching,
            free=free,
            holders={b: busy[b] for b in matching if b in busy},
        )

    # Nothing matches at all: work out why, so the agent can relax its request
    # instead of guessing.
    impossible = frozenset(
        tag for tag in requirement.tags if not any(tag in b.tags for b in benches)
    )
    closest, drop = _largest_satisfiable_subset(requirement.tags, benches)
    return SlotDiagnosis(
        slot=slot,
        kind="unsatisfiable",
        required=requirement.tags,
        matching=[],
        free=[],
        impossible_tags=impossible,
        closest_satisfiable=closest,
        drop_to_match=drop,
    )


def _largest_satisfiable_subset(
    required: frozenset[Tag],
    benches: list[Bench],
) -> tuple[frozenset[Tag], frozenset[Tag]]:
    """Biggest subset of ``required`` that some bench satisfies.

    Returns ``(subset, tags_to_drop)``. Searched by decreasing size so the
    first hit is maximal. Requirements are small (a handful of tags), and the
    search is capped by that size, not by the bench count.
    """
    tags = sorted(required)
    if len(tags) > 12:  # pragma: no cover - absurd request, skip the analysis
        return frozenset(), frozenset()
    for size in range(len(tags) - 1, 0, -1):
        for subset in combinations(tags, size):
            candidate = frozenset(subset)
            if any(candidate <= b.tags for b in benches):
                return candidate, required - candidate
    return frozenset(), frozenset()
