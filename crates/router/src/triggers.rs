//! Fallback triggers and escapes (ADR 014).
//!
//! A virtual model's `fallback_on` list compiles to a [`Triggers`] bitset.
//! The decide phase flattens a routing tree into an ordered attempt list in
//! which each attempt carries [`Escape`]s, innermost first: "on a failure
//! matching `on`, continue at attempt `next`". The executor follows the first
//! matching escape, or returns the error when none matches.

use lumen_core::ProviderError;
use serde::{Deserialize, Serialize};

/// One named fallback trigger (the `fallback_on` vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// Upstream 5xx (including TypeSafe 529 Overloaded), unreachable upstream, or a stream that opened with no content frame.
    ProviderError,
    /// Upstream 429.
    RateLimited,
    /// Connect, first-token or read timeout.
    Timeout,
    /// The target's circuit breaker is open.
    CircuitOpen,
    /// Opt-in: the upstream rejected the input as too long.
    ContextLength,
    /// Opt-in: the upstream refused on content policy.
    ContentFilter,
}

const ALL: [Trigger; 6] = [
    Trigger::ProviderError,
    Trigger::RateLimited,
    Trigger::Timeout,
    Trigger::CircuitOpen,
    Trigger::ContextLength,
    Trigger::ContentFilter,
];

/// The single-bit mask of `t` in a [`Triggers`] set.
const fn bit(t: Trigger) -> u8 {
    1 << (t as u8)
}

/// A set of [`Trigger`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Triggers(u8);

impl Triggers {
    /// No trigger: a failure is final.
    pub const NONE: Self = Self(0);
    /// `provider_error`, `rate_limited`, `timeout`, `circuit_open`: exactly
    /// the ADR 005 fallback behaviour.
    pub const DEFAULT: Self = Self(
        bit(Trigger::ProviderError)
            | bit(Trigger::RateLimited)
            | bit(Trigger::Timeout)
            | bit(Trigger::CircuitOpen),
    );

    /// The set of the listed triggers.
    #[must_use]
    pub fn from_list(list: &[Trigger]) -> Self {
        Self(list.iter().fold(0, |acc, t| acc | bit(*t)))
    }

    /// The triggers in this set, in declaration order.
    #[must_use]
    pub fn to_list(self) -> Vec<Trigger> {
        ALL.into_iter().filter(|t| self.contains(*t)).collect()
    }

    /// Whether `trigger` is in the set.
    #[must_use]
    pub const fn contains(self, trigger: Trigger) -> bool {
        self.0 & bit(trigger) != 0
    }

    /// Whether a failed attempt with `error` may move on under this set.
    /// Client errors, cancellation and translation failures never do.
    #[must_use]
    pub const fn allows(self, error: &ProviderError) -> bool {
        match error {
            ProviderError::Upstream {
                retryable: true, ..
            }
            | ProviderError::Unavailable { .. }
            | ProviderError::EmptyStream { .. } => self.contains(Trigger::ProviderError),
            ProviderError::RateLimited { .. } => self.contains(Trigger::RateLimited),
            ProviderError::Timeout { .. }
            | ProviderError::ConnectTimeout { .. }
            | ProviderError::FirstTokenTimeout { .. } => self.contains(Trigger::Timeout),
            ProviderError::ContextLengthExceeded { .. } => self.contains(Trigger::ContextLength),
            ProviderError::ContentFiltered { .. } => self.contains(Trigger::ContentFilter),
            ProviderError::Upstream {
                retryable: false, ..
            }
            | ProviderError::Cancelled
            | ProviderError::Translation(_)
            | ProviderError::ImageUrlNotSupported { .. }
            | ProviderError::UnsupportedField { .. }
            | ProviderError::UnsupportedInput { .. } => false,
        }
    }
}

/// "On a failure matching `on`, continue at attempt `next`."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Escape {
    /// The failures this escape catches.
    pub on: Triggers,
    /// The attempt index to continue at (always greater than the current one).
    pub next: usize,
}

/// The escapes of link `index` in a plain linear chain of `len` links: the
/// default triggers to the next link, none for the last one.
#[must_use]
pub fn linear_escapes(index: usize, len: usize) -> Vec<Escape> {
    if index + 1 < len {
        vec![Escape {
            on: Triggers::DEFAULT,
            next: index + 1,
        }]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_core::ProviderError;

    fn up(status: u16, retryable: bool) -> ProviderError {
        ProviderError::Upstream {
            provider: "p".into(),
            status,
            retryable,
        }
    }

    #[test]
    fn default_triggers_reproduce_adr_005_fallback() {
        let d = Triggers::DEFAULT;
        assert!(d.allows(&up(503, true)));
        assert!(d.allows(&ProviderError::RateLimited {
            provider: "p".into(),
            retry_after: None
        }));
        assert!(d.allows(&ProviderError::Timeout {
            provider: "p".into()
        }));
        assert!(d.allows(&ProviderError::ConnectTimeout {
            provider: "p".into()
        }));
        assert!(d.allows(&ProviderError::FirstTokenTimeout {
            provider: "p".into()
        }));
        assert!(d.allows(&ProviderError::Unavailable {
            provider: "p".into()
        }));
        assert!(d.allows(&ProviderError::EmptyStream {
            provider: "p".into()
        }));
        assert!(d.contains(Trigger::CircuitOpen));
        assert!(!d.allows(&up(400, false)));
        assert!(!d.allows(&ProviderError::Cancelled));
        assert!(!d.allows(&ProviderError::Translation("x".into())));
        assert!(!d.allows(&ProviderError::ContextLengthExceeded {
            provider: "p".into(),
            status: 400
        }));
    }

    #[test]
    fn opt_in_triggers_match_only_their_error() {
        let t = Triggers::from_list(&[Trigger::ContextLength]);
        assert!(t.allows(&ProviderError::ContextLengthExceeded {
            provider: "p".into(),
            status: 400
        }));
        assert!(!t.allows(&up(503, true)));
        assert!(!t.contains(Trigger::CircuitOpen));
        let c = Triggers::from_list(&[Trigger::ContentFilter]);
        assert!(c.allows(&ProviderError::ContentFiltered {
            provider: "p".into(),
            status: 400
        }));
    }

    #[test]
    fn trigger_list_round_trips() {
        let list = vec![Trigger::RateLimited, Trigger::ContextLength];
        assert_eq!(Triggers::from_list(&list).to_list(), list);
    }

    #[test]
    fn linear_escapes_point_at_the_next_link_except_the_last() {
        assert_eq!(
            linear_escapes(0, 3),
            vec![Escape {
                on: Triggers::DEFAULT,
                next: 1
            }]
        );
        let got = linear_escapes(2, 3);
        assert!(got.is_empty(), "{got:?}");
    }
}
