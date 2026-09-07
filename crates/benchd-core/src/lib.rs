//! benchd - lease-based hardware bench broker.
//!
//! Agents claim benches by capability tags for a bounded time and get a real
//! device node; nothing else can reach the hardware.
//!
//! Module map:
//! - [`tags`] - the `key=value` vocabulary, validation, implication closure
//! - [`model`] - benches, resources, claim requests, inventory loading
//! - [`matcher`] - superset matching, best-fit scoring, atomic multi-slot
//!   allocation, and unsatisfiable-vs-contended diagnosis
//! - [`limits`] - how long a lease may be held, and how many
//! - [`lease`] - the lease lifecycle state machine (pure; time is a parameter)
//! - [`wire`] - the JSON-lines protocol shared by all three binaries
//! - [`usbip`] - the USB/IP handshake, behind the `usbip` feature
//! - [`sysfs`] - handing a connected socket to the kernel's USB/IP drivers

pub mod lease;
pub mod limits;
pub mod matcher;
pub mod model;
#[cfg(feature = "usbip")]
pub mod sysfs;
pub mod tags;
#[cfg(feature = "usbip")]
pub mod usbip;
pub mod wire;

pub use lease::{
    ClaimError, Effect, EndReason, Epoch, Granted, Lease, LeaseError, LeaseEvent, LeaseId,
    LeaseManager, LeaseState, RevokeReason, SessionId,
};
pub use limits::{GrantedTtl, LimitError, Limits, Secs};
pub use matcher::{allocate, fit_cost, Allocation, BusyInfo, Failure, NoMatch, SlotDiagnosis};
pub use model::{Bench, ClaimRequest, Distinct, Inventory, InventoryError, Requirement, Resource};
pub use tags::{format_tags, parse_tags, Tag, TagError, TagSet, Vocabulary};

/// Convenience wrapper: allocate against a whole inventory, wiring up the
/// per-key weights the matcher needs.
pub fn allocate_in(
    inventory: &Inventory,
    request: &ClaimRequest,
    busy: &std::collections::BTreeMap<String, BusyInfo>,
) -> Result<Allocation, NoMatch> {
    let benches = inventory.enabled_benches();
    allocate(request, &benches, busy, inventory.vocabulary.key_weights())
}
