"""Tag vocabulary, validation and implication closure.

Tags are ``key=value`` pairs.  A tag *set* is a frozenset of ``(key, value)``
tuples, which means a key may legitimately appear more than once
(``sensor=bme280`` and ``sensor=sht31`` on the same bench).  Matching is pure
subset containment, so multi-valued keys need no special handling.

Wire-compatibility note: labgrid stores place tags as a ``dict``, i.e. one
value per key, and its own validation is ``TAG_KEY = [a-z][a-z0-9_]+`` with
``TAG_VAL = [a-z0-9_]?`` (unanchored, so effectively no validation of values at
all).  We are deliberately stricter and we permit multi-valued keys; a bench
using them cannot be round-tripped through a labgrid place without flattening.
See ``docs/labgrid.md``.
"""

from __future__ import annotations

import difflib
import re
from dataclasses import dataclass, field
from typing import Iterable, Mapping

__all__ = [
    "Tag",
    "TagSet",
    "TagError",
    "UnknownTag",
    "Vocabulary",
    "parse_tag",
    "parse_tags",
    "format_tag",
    "format_tags",
]

# Compatible with labgrid's TAG_KEY. Anchored, unlike labgrid's.
TAG_KEY_RE = re.compile(r"^[a-z][a-z0-9_]+$")
# Deliberately stricter than labgrid's (effectively unvalidated) TAG_VAL.
# Dots are allowed for versions (esp_idf=5.2), dashes are not: they invite
# the `esp32-s3` vs `esp32s3` split we are trying to design out.
TAG_VAL_RE = re.compile(r"^[a-z0-9][a-z0-9_.]*$")

Tag = tuple[str, str]
TagSet = frozenset  # frozenset[Tag]; alias kept loose for 3.12 typing ergonomics


class TagError(ValueError):
    """Base class for tag problems."""


class UnknownTag(TagError):
    """A tag is not in the closed vocabulary.

    Carries ``suggestions`` so callers can render a did-you-mean list; this is
    the difference between an agent self-correcting a typo in one turn and an
    agent burning ten turns on "no bench matches".
    """

    def __init__(self, message: str, suggestions: list[str] | None = None) -> None:
        super().__init__(message)
        self.suggestions = suggestions or []


def parse_tag(text: str) -> Tag:
    """Parse ``"key=value"`` into a ``(key, value)`` tuple."""
    if "=" not in text:
        raise TagError(f"{text!r} is not a valid tag (expected key=value)")
    key, _, value = text.partition("=")
    key = key.strip()
    value = value.strip()
    if not TAG_KEY_RE.match(key):
        raise TagError(
            f"invalid tag key {key!r} in {text!r} "
            "(expected lowercase [a-z][a-z0-9_]+)"
        )
    if not TAG_VAL_RE.match(value):
        raise TagError(
            f"invalid tag value {value!r} in {text!r} "
            "(expected lowercase [a-z0-9][a-z0-9_.]*; note dashes are not allowed)"
        )
    return (key, value)


def parse_tags(items: Iterable[str]) -> frozenset[Tag]:
    return frozenset(parse_tag(item) for item in items)


def format_tag(tag: Tag) -> str:
    return f"{tag[0]}={tag[1]}"


def format_tags(tags: Iterable[Tag]) -> str:
    return " ".join(sorted(format_tag(t) for t in tags))


@dataclass(frozen=True)
class TagDef:
    """A vocabulary entry for one ``key=value`` pair."""

    key: str
    value: str
    description: str = ""
    #: Tags implied by this one, e.g. soc=esp32s3 implies family=esp32.
    implies: frozenset[Tag] = field(default_factory=frozenset)

    @property
    def tag(self) -> Tag:
        return (self.key, self.value)


@dataclass
class Vocabulary:
    """A closed set of known tags plus their implication graph.

    Closed vocabularies exist so that a typo fails loudly and immediately
    rather than silently matching nothing.  ``open_keys`` is an escape hatch
    for keys whose values are inherently unbounded (e.g. ``name``).
    """

    defs: dict[Tag, TagDef] = field(default_factory=dict)
    open_keys: frozenset[str] = frozenset({"name"})
    #: Per-key scarcity weight for best-fit scoring. Identity keys (*which*
    #: SoC this is) describe a bench without being a contended capability, so
    #: they get weight 0; peripherals worth conserving keep weight 1.
    key_weights: dict[str, float] = field(default_factory=dict)

    def weight(self, key: str) -> float:
        return self.key_weights.get(key, 1.0)

    @classmethod
    def from_dict(cls, data: Mapping[str, object]) -> "Vocabulary":
        """Build from the ``tags:`` section of an inventory file.

        Expected shape::

            tags:
              soc:
                description: "System on chip"
                values:
                  esp32s3:
                    description: "ESP32-S3"
                    implies: [family=esp32, arch=xtensa, jtag=builtin]
        """
        defs: dict[Tag, TagDef] = {}
        key_weights: dict[str, float] = {}
        raw_tags = data.get("tags") or {}
        if not isinstance(raw_tags, Mapping):
            raise TagError("'tags' section must be a mapping")

        for key, key_body in raw_tags.items():
            if not TAG_KEY_RE.match(str(key)):
                raise TagError(f"invalid tag key {key!r} in vocabulary")
            if not isinstance(key_body, Mapping):
                raise TagError(f"vocabulary entry for {key!r} must be a mapping")
            if "weight" in key_body:
                try:
                    key_weights[str(key)] = float(key_body["weight"])  # type: ignore[arg-type]
                except (TypeError, ValueError) as exc:
                    raise TagError(
                        f"weight for tag key {key!r} must be a number"
                    ) from exc
            values = key_body.get("values") or {}
            if not isinstance(values, Mapping):
                raise TagError(f"'values' for tag key {key!r} must be a mapping")
            for value, val_body in values.items():
                if not TAG_VAL_RE.match(str(value)):
                    raise TagError(f"invalid tag value {value!r} for key {key!r}")
                val_body = val_body or {}
                if not isinstance(val_body, Mapping):
                    raise TagError(f"vocabulary entry for {key}={value} must be a mapping")
                implies = parse_tags(val_body.get("implies") or [])
                defs[(str(key), str(value))] = TagDef(
                    key=str(key),
                    value=str(value),
                    description=str(val_body.get("description", "")),
                    implies=implies,
                )

        open_keys = frozenset(str(k) for k in (data.get("open_keys") or ["name"]))
        vocab = cls(defs=defs, open_keys=open_keys, key_weights=key_weights)
        vocab.validate()
        return vocab

    # -- validation ------------------------------------------------------

    def validate(self) -> None:
        """Check that every implied tag exists and that there are no cycles."""
        for tag, tagdef in self.defs.items():
            for implied in tagdef.implies:
                if implied not in self.defs and implied[0] not in self.open_keys:
                    raise TagError(
                        f"{format_tag(tag)} implies unknown tag {format_tag(implied)}"
                    )
        # Cycle detection over the implication graph.
        WHITE, GREY, BLACK = 0, 1, 2
        colour: dict[Tag, int] = {t: WHITE for t in self.defs}

        def visit(node: Tag, path: list[Tag]) -> None:
            colour[node] = GREY
            for nxt in self.defs[node].implies:
                if nxt not in colour:
                    continue
                if colour[nxt] == GREY:
                    cycle = " -> ".join(format_tag(t) for t in [*path, node, nxt])
                    raise TagError(f"implication cycle: {cycle}")
                if colour[nxt] == WHITE:
                    visit(nxt, [*path, node])
            colour[node] = BLACK

        for tag in list(self.defs):
            if colour[tag] == WHITE:
                visit(tag, [])

    def check(self, tags: Iterable[Tag]) -> None:
        """Raise :class:`UnknownTag` for anything outside the vocabulary."""
        for tag in tags:
            key, value = tag
            if key in self.open_keys:
                continue
            if tag in self.defs:
                continue
            raise UnknownTag(
                f"unknown tag {format_tag(tag)}",
                suggestions=self.suggest(tag),
            )

    def suggest(self, tag: Tag, limit: int = 4) -> list[str]:
        """Closest known tags, for did-you-mean output."""
        key, _ = tag
        known = [format_tag(t) for t in self.defs]
        target = format_tag(tag)
        close = difflib.get_close_matches(target, known, n=limit, cutoff=0.5)
        if close:
            return close
        # Unknown value but known key: offer that key's values, which is the
        # common case (`soc=esp32s4`).
        same_key = sorted(format_tag(t) for t in self.defs if t[0] == key)
        if same_key:
            return same_key[:limit]
        # Unknown key entirely: offer near-miss keys.
        keys = sorted({t[0] for t in self.defs})
        return difflib.get_close_matches(key, keys, n=limit, cutoff=0.4)

    # -- implication closure ---------------------------------------------

    def expand(self, tags: Iterable[Tag]) -> frozenset[Tag]:
        """Return ``tags`` plus everything they transitively imply.

        Applied to *bench* tags at load time, so an agent asking for
        ``family=esp32`` matches a bench declared only as ``soc=esp32s3``.
        Requirements are never expanded: expanding them would make requests
        strictly harder to satisfy, which is the opposite of the intent.
        """
        seen: set[Tag] = set()
        stack = list(tags)
        while stack:
            tag = stack.pop()
            if tag in seen:
                continue
            seen.add(tag)
            tagdef = self.defs.get(tag)
            if tagdef is not None:
                stack.extend(tagdef.implies - seen)
        return frozenset(seen)

    def describe(self, tag: Tag) -> str:
        tagdef = self.defs.get(tag)
        return tagdef.description if tagdef else ""
