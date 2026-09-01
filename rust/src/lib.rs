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

pub mod matcher;
pub mod model;
pub mod tags;

pub use matcher::{allocate, fit_cost, Allocation, BusyInfo, Failure, NoMatch, SlotDiagnosis};
pub use model::{
    Bench, ClaimRequest, Distinct, Inventory, InventoryError, Requirement, Resource,
};
pub use tags::{format_tags, parse_tags, Tag, TagError, TagSet, Vocabulary};

/// Convenience wrapper: allocate against a whole inventory, wiring up the tag
/// counts and per-key weights the matcher needs.
pub fn allocate_in(
    inventory: &Inventory,
    request: &ClaimRequest,
    busy: &std::collections::BTreeMap<String, BusyInfo>,
) -> Result<Allocation, NoMatch> {
    let benches = inventory.enabled_benches();
    allocate(
        request,
        &benches,
        busy,
        &inventory.tag_counts(),
        inventory.vocabulary.key_weights(),
    )
}
