"""Inventory model: resources, benches, and claim requirements.

A **bench** is the unit of exclusion: a named set of physical resources that
must be held together (the board, its USB-JTAG interface, the relay that
power-cycles it).  Agents never name a bench; they describe one with tags and
the matcher finds it.  See ``docs/design.md``.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable, Mapping

from .tags import Tag, TagError, Vocabulary, format_tags, parse_tags

__all__ = [
    "Resource",
    "SerialResource",
    "UsbResource",
    "Bench",
    "Requirement",
    "ClaimRequest",
    "Inventory",
    "InventoryError",
]


class InventoryError(ValueError):
    """Malformed inventory definition."""


@dataclass(frozen=True)
class Resource:
    """Something that gets materialised into a lease directory."""

    name: str

    @property
    def kind(self) -> str:  # pragma: no cover - overridden
        raise NotImplementedError


@dataclass(frozen=True)
class SerialResource(Resource):
    """A USB serial device, identified by its stable ``/dev/serial/by-id`` path.

    We key on by-id rather than ``ttyUSB0`` because kernel indices renumber on
    replug, and an agent that gets handed the wrong board mid-session is the
    exact failure this whole system exists to prevent.
    """

    by_id: str

    @property
    def kind(self) -> str:
        return "serial"

    @property
    def path(self) -> Path:
        return Path(self.by_id)

    def resolve(self) -> Path | None:
        """Resolve to the current ``/dev/ttyX`` node, or ``None`` if absent."""
        p = self.path
        try:
            if not p.exists():
                return None
            return p.resolve()
        except OSError:
            return None


@dataclass(frozen=True)
class UsbResource(Resource):
    """A whole USB device, for USB/IP export to a remote client.

    Placeholder for the remote backend: carries the bus id (``1-2.3``) that
    ``usbip bind`` needs.  Not materialised by the local backend.
    """

    busid: str

    @property
    def kind(self) -> str:
        return "usb"


@dataclass
class Bench:
    """A named, atomically-claimable set of resources."""

    id: str
    tags: frozenset[Tag]
    resources: dict[str, Resource] = field(default_factory=dict)
    description: str = ""
    #: Set False to keep a bench in the inventory but out of the matcher
    #: (dead board, cable being reseated) without deleting its definition.
    enabled: bool = True

    def __post_init__(self) -> None:
        if not self.id:
            raise InventoryError("bench id must not be empty")

    @property
    def resource_names(self) -> list[str]:
        return sorted(self.resources)

    def describe(self) -> str:
        return f"{self.id} [{format_tags(self.tags)}]"


@dataclass(frozen=True)
class Requirement:
    """Tags a single slot must satisfy.

    A bench matches when its tag set is a *superset* of ``tags``.
    """

    tags: frozenset[Tag]

    @classmethod
    def parse(cls, items: Iterable[str]) -> "Requirement":
        return cls(tags=parse_tags(items))

    def matches(self, bench: Bench) -> bool:
        # The entire matching semantic, in one line: a bench matches when its
        # tags are a superset of what was asked for.
        return self.tags <= bench.tags

    def missing_from(self, bench: Bench) -> frozenset[Tag]:
        """Which required tags this bench lacks. Drives near-miss reporting."""
        return self.tags - bench.tags


@dataclass
class ClaimRequest:
    """A request for one or more benches, granted atomically or not at all.

    ``slots`` maps a caller-chosen role name (``dut``, ``peer``) to its
    requirement.  Partial allocation is never returned: two agents each holding
    half of what they need is a deadlock, so the matcher either satisfies every
    slot or reports why it cannot.
    """

    slots: dict[str, Requirement]
    #: Slot names that must land on *different* benches. ``True`` means all.
    distinct: bool | frozenset[str] = True
    ttl_seconds: int = 0
    reason: str = ""

    def __post_init__(self) -> None:
        if not self.slots:
            raise InventoryError("claim must request at least one slot")
        for name in self.slots:
            if not name or not name.replace("_", "").isalnum():
                raise InventoryError(
                    f"invalid slot name {name!r} (alphanumeric and underscore only)"
                )

    def distinct_slots(self) -> frozenset[str]:
        if self.distinct is True:
            return frozenset(self.slots)
        if self.distinct is False:
            return frozenset()
        return frozenset(self.distinct)


@dataclass
class Inventory:
    """The set of known benches plus the vocabulary describing them."""

    benches: dict[str, Bench] = field(default_factory=dict)
    vocabulary: Vocabulary = field(default_factory=Vocabulary)

    @classmethod
    def from_dict(cls, data: Mapping[str, object]) -> "Inventory":
        vocab = Vocabulary.from_dict(data)
        benches: dict[str, Bench] = {}

        raw_benches = data.get("benches") or {}
        if not isinstance(raw_benches, Mapping):
            raise InventoryError("'benches' section must be a mapping")

        for bench_id, body in raw_benches.items():
            if not isinstance(body, Mapping):
                raise InventoryError(f"bench {bench_id!r} must be a mapping")
            try:
                declared = parse_tags(body.get("tags") or [])
            except TagError as exc:
                raise InventoryError(f"bench {bench_id!r}: {exc}") from exc

            try:
                vocab.check(declared)
            except TagError as exc:
                raise InventoryError(f"bench {bench_id!r}: {exc}") from exc

            # Bench tags are expanded through the implication graph so that a
            # bench declared `soc=esp32s3` also matches `family=esp32`.
            tags = vocab.expand(declared)
            # Name is injected as a tag so that human/debug selection by name
            # rides the same matching path as everything else (labgrid does the
            # same trick).
            tags = tags | {("name", str(bench_id))}

            resources = _parse_resources(bench_id, body.get("resources") or {})
            benches[str(bench_id)] = Bench(
                id=str(bench_id),
                tags=tags,
                resources=resources,
                description=str(body.get("description", "")),
                enabled=bool(body.get("enabled", True)),
            )

        return cls(benches=benches, vocabulary=vocab)

    @classmethod
    def from_yaml(cls, path: str | Path) -> "Inventory":
        import yaml  # imported lazily so the core stays dependency-free

        with open(path, "r", encoding="utf-8") as fh:
            data = yaml.safe_load(fh) or {}
        if not isinstance(data, Mapping):
            raise InventoryError(f"{path}: top level must be a mapping")
        return cls.from_dict(data)

    def enabled_benches(self) -> list[Bench]:
        return [b for b in self.benches.values() if b.enabled]

    def tag_counts(self) -> dict[Tag, int]:
        """How many enabled benches carry each tag. Drives scarcity scoring."""
        counts: dict[Tag, int] = {}
        for bench in self.enabled_benches():
            for tag in bench.tags:
                counts[tag] = counts.get(tag, 0) + 1
        return counts

    @property
    def tag_weights(self) -> dict[str, float]:
        """Per-key best-fit weights, from the vocabulary."""
        return self.vocabulary.key_weights


def _parse_resources(bench_id: object, raw: object) -> dict[str, Resource]:
    if not isinstance(raw, Mapping):
        raise InventoryError(f"bench {bench_id!r}: 'resources' must be a mapping")
    out: dict[str, Resource] = {}
    for name, body in raw.items():
        name = str(name)
        if not name.replace("_", "").isalnum():
            raise InventoryError(
                f"bench {bench_id!r}: invalid resource name {name!r}"
            )
        if not isinstance(body, Mapping):
            raise InventoryError(
                f"bench {bench_id!r}: resource {name!r} must be a mapping"
            )
        kind = str(body.get("kind", "serial"))
        if kind == "serial":
            by_id = body.get("by_id")
            if not by_id:
                raise InventoryError(
                    f"bench {bench_id!r}: serial resource {name!r} needs 'by_id'"
                )
            out[name] = SerialResource(name=name, by_id=str(by_id))
        elif kind == "usb":
            busid = body.get("busid")
            if not busid:
                raise InventoryError(
                    f"bench {bench_id!r}: usb resource {name!r} needs 'busid'"
                )
            out[name] = UsbResource(name=name, busid=str(busid))
        else:
            raise InventoryError(
                f"bench {bench_id!r}: resource {name!r} has unknown kind {kind!r}"
            )
    return out
