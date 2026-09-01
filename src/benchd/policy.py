"""Who may hold what, for how long.

Every claim carries an explicit TTL - agents must say how long they want the
hardware. This module decides whether that request is allowed.

Two limits are needed and they are not the same thing:

* ``max_ttl`` bounds a *single* grant.
* ``max_total_hold`` bounds the sum across renewals, so an agent cannot renew
  in a loop forever and starve everyone else.

Plus ``max_benches``, or one agent claims four boards for a two-board test.

**This is policy, not security.** Identity here is whatever the caller says it
is; there is no authentication anywhere in this stack (labgrid works the same
way - ``LG_USERNAME`` is an environment variable). It stops honest mistakes and
runaway agents. It does not stop anything that is trying. If you ever need a
boundary, it goes in the transport - per-identity SSH/mTLS - not here.
"""

from __future__ import annotations

import fnmatch
from dataclasses import dataclass, field
from typing import Mapping

__all__ = [
    "IdentityClass",
    "Policy",
    "PolicyError",
    "AGENT",
    "HUMAN",
    "CI",
    "DEFAULT_POLICY",
]


class PolicyError(Exception):
    """A claim or renewal is not permitted.

    The message is written to be read by an agent: it always says what the
    limit is and what to do instead.
    """


@dataclass(frozen=True)
class IdentityClass:
    """Limits applied to one kind of caller."""

    name: str
    #: Longest single grant, in seconds.
    max_ttl: int
    #: Longest total hold across renewals. ``None`` means unbounded.
    max_total_hold: int | None
    #: Most benches held concurrently across all leases. ``None`` = unbounded.
    max_benches: int | None
    #: May extend a lease. CI deliberately cannot: a job that overruns its
    #: budget should fail loudly rather than quietly hold hardware.
    renewable: bool = True
    #: May take a bench from another holder. Agents must never have this.
    may_preempt: bool = False
    #: May claim a specific bench by name rather than by capability. Humans
    #: debugging a specific board need this; agents must not have it, or one
    #: will hardcode a bench name into a test script and reintroduce exactly
    #: the contention this system exists to remove.
    may_claim_by_name: bool = False

    def clamp_ttl(self, requested: int) -> int:
        return min(requested, self.max_ttl)


#: Short leases, renewable, no preemption, no claim-by-name.
AGENT = IdentityClass(
    name="agent",
    max_ttl=15 * 60,
    max_total_hold=2 * 60 * 60,
    max_benches=2,
    renewable=True,
    may_preempt=False,
    may_claim_by_name=False,
)

#: You, at the bench. Long leases, may take a board back from an agent.
HUMAN = IdentityClass(
    name="human",
    max_ttl=8 * 60 * 60,
    max_total_hold=None,
    max_benches=None,
    renewable=True,
    may_preempt=True,
    may_claim_by_name=True,
)

#: Batch jobs: bounded, non-renewable, fail fast.
CI = IdentityClass(
    name="ci",
    max_ttl=45 * 60,
    max_total_hold=45 * 60,
    max_benches=4,
    renewable=False,
    may_preempt=False,
    may_claim_by_name=False,
)


@dataclass
class Policy:
    """Maps caller identities to classes and enforces their limits."""

    classes: dict[str, IdentityClass] = field(default_factory=dict)
    #: Ordered glob patterns; first match wins. e.g. ``[("agent-*", "agent")]``
    assignments: list[tuple[str, str]] = field(default_factory=list)
    default_class: str = "agent"
    #: Seconds of warning before a lease is actually torn down. The holder sees
    #: state ``revoking`` and can finish writing flash or park the board.
    grace_seconds: int = 30

    def resolve(self, identity: str) -> IdentityClass:
        """Return the class governing ``identity``.

        Unknown identities fall back to the *most restricted* class, so a
        typo in an agent name cannot accidentally grant human privileges.
        """
        for pattern, class_name in self.assignments:
            if fnmatch.fnmatchcase(identity, pattern):
                cls = self.classes.get(class_name)
                if cls is None:
                    raise PolicyError(
                        f"identity {identity!r} maps to unknown class "
                        f"{class_name!r}"
                    )
                return cls
        cls = self.classes.get(self.default_class)
        if cls is None:
            raise PolicyError(f"unknown default class {self.default_class!r}")
        return cls

    # -- checks ----------------------------------------------------------

    def check_claim(
        self,
        identity: str,
        ttl_seconds: int,
        slot_count: int,
        held_benches: int,
        by_name: bool = False,
    ) -> int:
        """Validate a claim and return the granted TTL.

        The TTL is *clamped*, not rejected, when it exceeds the class maximum:
        an agent asking for 4h and getting 15m with a clear message can get on
        with its work, whereas a hard rejection just costs a round trip. Every
        other limit is a hard error.
        """
        cls = self.resolve(identity)

        if ttl_seconds <= 0:
            raise PolicyError(
                "claims must specify a positive ttl (how long you need the "
                "hardware, in seconds); there is no default"
            )

        if by_name and not cls.may_claim_by_name:
            raise PolicyError(
                f"{identity} (class {cls.name}) may not claim a bench by name; "
                "describe what you need with capability tags instead"
            )

        if cls.max_benches is not None:
            if held_benches + slot_count > cls.max_benches:
                raise PolicyError(
                    f"{identity} (class {cls.name}) may hold at most "
                    f"{cls.max_benches} bench(es); already holding "
                    f"{held_benches} and asked for {slot_count} more. "
                    "Release something first."
                )

        granted = cls.clamp_ttl(ttl_seconds)
        if cls.max_total_hold is not None:
            granted = min(granted, cls.max_total_hold)
        return granted

    def check_renew(
        self,
        identity: str,
        extra_seconds: int,
        held_for: float,
    ) -> int:
        """Validate a renewal and return the granted extension in seconds."""
        cls = self.resolve(identity)

        if not cls.renewable:
            raise PolicyError(
                f"{identity} (class {cls.name}) may not renew a lease; "
                "claim a new lease if more time is genuinely needed"
            )
        if extra_seconds <= 0:
            raise PolicyError("renewal must request a positive number of seconds")

        granted = cls.clamp_ttl(extra_seconds)

        if cls.max_total_hold is not None:
            remaining_budget = cls.max_total_hold - held_for
            if remaining_budget <= 0:
                raise PolicyError(
                    f"{identity} (class {cls.name}) has reached the maximum "
                    f"total hold of {cls.max_total_hold}s for this lease. "
                    "Release the bench; claim again if you still need it."
                )
            granted = int(min(granted, remaining_budget))

        return granted

    def check_preempt(self, identity: str) -> None:
        cls = self.resolve(identity)
        if not cls.may_preempt:
            raise PolicyError(
                f"{identity} (class {cls.name}) may not take a bench from "
                "another holder"
            )

    # -- construction ----------------------------------------------------

    @classmethod
    def from_dict(cls, data: Mapping[str, object]) -> "Policy":
        classes = {c.name: c for c in (AGENT, HUMAN, CI)}
        raw_classes = data.get("classes") or {}
        if not isinstance(raw_classes, Mapping):
            raise PolicyError("'classes' must be a mapping")
        for name, body in raw_classes.items():
            if not isinstance(body, Mapping):
                raise PolicyError(f"class {name!r} must be a mapping")
            base = classes.get(str(name))
            classes[str(name)] = IdentityClass(
                name=str(name),
                max_ttl=int(body.get("max_ttl", base.max_ttl if base else 900)),
                max_total_hold=_opt_int(
                    body.get(
                        "max_total_hold",
                        base.max_total_hold if base else None,
                    )
                ),
                max_benches=_opt_int(
                    body.get("max_benches", base.max_benches if base else None)
                ),
                renewable=bool(
                    body.get("renewable", base.renewable if base else True)
                ),
                may_preempt=bool(
                    body.get("may_preempt", base.may_preempt if base else False)
                ),
                may_claim_by_name=bool(
                    body.get(
                        "may_claim_by_name",
                        base.may_claim_by_name if base else False,
                    )
                ),
            )

        assignments: list[tuple[str, str]] = []
        for entry in data.get("identities") or []:
            if isinstance(entry, Mapping):
                pattern = str(entry.get("match", ""))
                class_name = str(entry.get("class", ""))
                if not pattern or not class_name:
                    raise PolicyError(
                        "each 'identities' entry needs 'match' and 'class'"
                    )
                assignments.append((pattern, class_name))
            else:
                raise PolicyError("'identities' entries must be mappings")

        return cls(
            classes=classes,
            assignments=assignments,
            default_class=str(data.get("default_class", "agent")),
            grace_seconds=int(data.get("grace_seconds", 30)),
        )


def _opt_int(value: object) -> int | None:
    if value is None:
        return None
    return int(value)  # type: ignore[arg-type]


#: Sensible starting point: anything named like an agent is one, you are human.
DEFAULT_POLICY = Policy(
    classes={c.name: c for c in (AGENT, HUMAN, CI)},
    assignments=[("agent-*", "agent"), ("ci-*", "ci")],
    default_class="agent",
)
