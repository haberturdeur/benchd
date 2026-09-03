//! How long a lease may be held, and how many.
//!
//! One global set applies to every session: there are no classes and no roles
//! (D15). Identity grants nothing, so limits exist to stop a runaway agent
//! hoarding boards, not to express privilege.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Seconds. The whole crate works in whole seconds; sub-second precision would
/// be false precision for leases measured in minutes.
pub type Secs = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Longest single grant.
    pub max_ttl: Secs,
    /// Longest total hold across renewals, so nobody renews forever.
    pub max_total_hold: Secs,
    /// Concurrent benches per session.
    pub max_benches: usize,
    /// Warning window before teardown, during which the lease is `Revoking`
    /// and the holder can park the board. Yanking a device mid-flash can leave
    /// a board in bootloader.
    pub grace: Secs,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_ttl: 15 * 60,
            max_total_hold: 2 * 60 * 60,
            max_benches: 2,
            grace: 30,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LimitError {
    #[error(
        "claims must specify a positive ttl (how long you need the hardware, in \
         seconds); there is no default"
    )]
    NoTtl,

    #[error(
        "you may hold at most {max} bench(es); already holding {held} and asked \
         for {wanted} more. Release something first."
    )]
    TooManyBenches {
        max: usize,
        held: usize,
        wanted: usize,
    },

    #[error(
        "this lease has reached its maximum total hold of {max_total_hold}s. \
         Release it; claim again if you still need the hardware."
    )]
    HoldExhausted { max_total_hold: Secs },
}

/// A granted duration, and whether it was shortened.
///
/// Over-long requests are **clamped rather than rejected**: an agent asking for
/// four hours and getting fifteen minutes can get on with its work, whereas a
/// hard error just costs a round trip. The `requested` field is kept so the
/// caller can say so out loud.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrantedTtl {
    pub granted: Secs,
    pub requested: Secs,
}

impl GrantedTtl {
    pub fn was_clamped(&self) -> bool {
        self.granted < self.requested
    }

    /// Human-readable note for the agent, if anything was shortened.
    pub fn note(&self) -> Option<String> {
        self.was_clamped().then(|| {
            format!(
                "requested {}s, granted {}s (the limit); renew if you need longer",
                self.requested, self.granted
            )
        })
    }
}

impl Limits {
    /// Validate a claim and return the TTL to grant.
    pub fn grant(
        &self,
        requested_ttl: Secs,
        slots: usize,
        benches_held: usize,
    ) -> Result<GrantedTtl, LimitError> {
        if requested_ttl == 0 {
            return Err(LimitError::NoTtl);
        }
        if benches_held + slots > self.max_benches {
            return Err(LimitError::TooManyBenches {
                max: self.max_benches,
                held: benches_held,
                wanted: slots,
            });
        }
        Ok(GrantedTtl {
            granted: requested_ttl.min(self.max_ttl).min(self.max_total_hold),
            requested: requested_ttl,
        })
    }

    /// Validate a renewal and return the extension to grant.
    ///
    /// `held_for` is how long this lease has already existed; the extension is
    /// trimmed so that total lifetime never exceeds `max_total_hold`.
    pub fn renew(&self, requested: Secs, held_for: Secs) -> Result<GrantedTtl, LimitError> {
        if requested == 0 {
            return Err(LimitError::NoTtl);
        }
        let remaining_budget = self.max_total_hold.saturating_sub(held_for);
        if remaining_budget == 0 {
            return Err(LimitError::HoldExhausted {
                max_total_hold: self.max_total_hold,
            });
        }
        Ok(GrantedTtl {
            granted: requested.min(self.max_ttl).min(remaining_budget),
            requested,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_ttl_is_rejected_because_there_is_no_default() {
        assert_eq!(Limits::default().grant(0, 1, 0), Err(LimitError::NoTtl));
    }

    #[test]
    fn an_over_long_request_is_clamped_not_rejected() {
        let g = Limits::default().grant(4 * 3600, 1, 0).unwrap();
        assert_eq!(g.granted, 15 * 60);
        assert!(g.was_clamped());
        assert!(g.note().unwrap().contains("renew if you need longer"));
    }

    #[test]
    fn holding_too_many_benches_is_a_hard_error_that_says_what_to_do() {
        let err = Limits::default().grant(60, 2, 1).unwrap_err();
        assert!(err.to_string().contains("Release something first"));
    }

    #[test]
    fn renewal_is_trimmed_so_total_hold_is_never_exceeded() {
        let l = Limits {
            max_total_hold: 1000,
            ..Default::default()
        };
        let g = l.renew(600, 900).unwrap();
        assert_eq!(g.granted, 100, "only 100s of budget remained");
        assert!(g.was_clamped());
    }

    #[test]
    fn renewal_fails_once_the_hold_budget_is_gone() {
        let l = Limits {
            max_total_hold: 1000,
            ..Default::default()
        };
        assert_eq!(
            l.renew(60, 1000),
            Err(LimitError::HoldExhausted {
                max_total_hold: 1000
            })
        );
    }
}
