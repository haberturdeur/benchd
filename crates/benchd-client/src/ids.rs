//! Which coordinator something came from.
//!
//! A client daemon talks to several coordinators at once — typically a shared
//! lab server plus a local one owning the operator's own boards, listening on
//! loopback so it needs no authentication to stay private.
//!
//! Every coordinator is an independent authority (D6) and numbers its own
//! things: `LeaseId`, `SessionId` and per-bench epochs are all counters that
//! start at 1. So two coordinators routinely issue the same `LeaseId`, and any
//! map keyed on one alone silently merges two unrelated leases. That is the
//! same shape as the epoch-watermark bug that broke every relayed lease, except
//! it would corrupt four maps at once — so the ids are made unambiguous in the
//! type system rather than by remembering to pair them.

use std::fmt;

use benchd_core::lease::{LeaseId, SessionId};
use serde::{Deserialize, Serialize};

/// Index into the configured coordinator list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CoordinatorId(pub u32);

impl fmt::Display for CoordinatorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "c{}", self.0)
    }
}

/// A lease, qualified by the coordinator that granted it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseKey {
    pub coordinator: CoordinatorId,
    pub lease: LeaseId,
}

impl LeaseKey {
    pub fn new(coordinator: CoordinatorId, lease: LeaseId) -> Self {
        LeaseKey { coordinator, lease }
    }
}

impl LeaseKey {
    /// The id handed to the agent.
    ///
    /// Agents are not told there is more than one coordinator — that is what
    /// "first-class" means here — but `l1` from the lab server and `l1` from the
    /// local one are different leases, so a bare number would be ambiguous the
    /// moment an agent held both. The coordinator goes in the high 32 bits,
    /// which is reversible and needs no bookkeeping.
    pub fn to_public(self) -> u64 {
        ((self.coordinator.0 as u64) << 32) | (self.lease.0 & 0xffff_ffff)
    }

    pub fn from_public(public: u64) -> Self {
        LeaseKey {
            coordinator: CoordinatorId((public >> 32) as u32),
            lease: LeaseId(public & 0xffff_ffff),
        }
    }
}

impl fmt::Display for LeaseKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.coordinator, self.lease)
    }
}

/// A session, qualified by the coordinator that opened it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionKey {
    pub coordinator: CoordinatorId,
    pub session: SessionId,
}

impl SessionKey {
    pub fn new(coordinator: CoordinatorId, session: SessionId) -> Self {
        SessionKey {
            coordinator,
            session,
        }
    }
}

impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.coordinator, self.session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_lease_number_from_two_coordinators_is_two_different_leases() {
        // The property this module exists for. Every coordinator starts its
        // lease counter at 1, so `l1` is ambiguous on its own; keying a map on
        // it merges a lab lease with a local one, and tearing one down tears
        // down the other.
        let lab = LeaseKey::new(CoordinatorId(0), LeaseId(1));
        let local = LeaseKey::new(CoordinatorId(1), LeaseId(1));
        assert_ne!(lab, local);

        let mut map = std::collections::BTreeMap::new();
        map.insert(lab, "lab bench");
        map.insert(local, "my own board");
        assert_eq!(map.len(), 2, "these must not collide");
        assert_eq!(map[&lab], "lab bench");
    }

    #[test]
    fn the_public_id_round_trips_and_never_collides_across_coordinators() {
        for coordinator in 0..4u32 {
            for lease in 1..50u64 {
                let key = LeaseKey::new(CoordinatorId(coordinator), LeaseId(lease));
                assert_eq!(LeaseKey::from_public(key.to_public()), key);
            }
        }
        // The case that matters: same lease number, different coordinator.
        let lab = LeaseKey::new(CoordinatorId(0), LeaseId(1));
        let local = LeaseKey::new(CoordinatorId(1), LeaseId(1));
        assert_ne!(lab.to_public(), local.to_public());
    }

    #[test]
    fn keys_render_so_a_log_line_says_which_coordinator() {
        assert_eq!(
            LeaseKey::new(CoordinatorId(1), LeaseId(7)).to_string(),
            "c1/l7"
        );
        assert_eq!(
            SessionKey::new(CoordinatorId(0), SessionId(2)).to_string(),
            "c0/s2"
        );
    }
}
