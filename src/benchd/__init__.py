"""benchd - lease-based hardware bench broker.

Agents claim benches by capability tags for a bounded time and get a real
device node; nothing else can reach the hardware.
"""

from .matcher import Allocation, BusyInfo, NoMatch, allocate, fit_cost
from .model import (
    Bench,
    ClaimRequest,
    Inventory,
    InventoryError,
    Requirement,
    SerialResource,
    UsbResource,
)
from .tags import TagError, UnknownTag, Vocabulary, format_tag, format_tags, parse_tag

__version__ = "0.1.0.dev0"

__all__ = [
    "Allocation",
    "Bench",
    "BusyInfo",
    "ClaimRequest",
    "Inventory",
    "InventoryError",
    "NoMatch",
    "Requirement",
    "SerialResource",
    "TagError",
    "UnknownTag",
    "UsbResource",
    "Vocabulary",
    "allocate",
    "fit_cost",
    "format_tag",
    "format_tags",
    "parse_tag",
    "__version__",
]
