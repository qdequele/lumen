//! Retry with exponential backoff and jitter (M6 §6.1).
//!
//! Only *retryable* [`ProviderError`]s are retried (5xx, timeouts, unreachable,
//! 429 — never a 4xx client fault; see [`ProviderError::is_retryable`]). Backoff
//! is exponential (`base·2ⁿ`, capped at `max`) with **equal jitter**, and an
//! upstream `Retry-After` acts as a floor. The delay maths live in a pure,
//! unit-tested [`backoff_delay`]; production feeds it a lock-free pseudo-random
//! fraction so there is no dependency, no blocking and no clock read on the hot
//! path.
//!
//! The overall request deadline (the total timeout, M6 §6.4) is enforced by the
//! caller wrapping the whole chain in `timeout_at`; this loop only decides
//! *whether* and *how long* to wait between attempts.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use lumen_core::ProviderError;
use std::future::Future;
use tokio_util::sync::CancellationToken;

/// Retry parameters. Defaults (M6 spec): 3 attempts, 200 ms base, 5 s cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts, including the first (so `1` disables retrying).
    pub max_attempts: u32,
    /// Base backoff delay (the wait after the first failure, pre-jitter).
    pub base: Duration,
    /// Ceiling on the exponential term, before jitter and `Retry-After`.
    pub max: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base: Duration::from_millis(200),
            max: Duration::from_secs(5),
        }
    }
}

impl RetryPolicy {
    /// A policy that never retries (a single attempt).
    #[must_use]
    pub const fn never() -> Self {
        Self {
            max_attempts: 1,
            base: Duration::from_millis(200),
            max: Duration::from_secs(5),
        }
    }
}

/// The delay before the retry that follows `failed_attempt` (0-based: `0` is the
/// wait after the very first failure).
///
/// `exp = min(base·2^failed_attempt, max)`, then **equal jitter**
/// `d = exp/2 + (exp/2)·rand01` so `d ∈ [exp/2, exp]`, then floored at
/// `retry_after` when the upstream asked for a longer wait. `rand01` is expected
/// in `[0, 1)`; it is clamped defensively.
#[must_use]
pub fn backoff_delay(
    failed_attempt: u32,
    policy: &RetryPolicy,
    retry_after: Option<Duration>,
    rand01: f64,
) -> Duration {
    let base_ms = u64::try_from(policy.base.as_millis()).unwrap_or(u64::MAX);
    let max_ms = u64::try_from(policy.max.as_millis()).unwrap_or(u64::MAX);
    let factor = 1u64.checked_shl(failed_attempt).unwrap_or(u64::MAX);
    let exp_ms = base_ms.saturating_mul(factor).min(max_ms);

    let half = exp_ms / 2;
    let r = rand01.clamp(0.0, 1.0);
    // `half` is at most `max_ms` (a few thousand), well within f64's exact
    // integer range; truncating the product toward zero is the intended floor.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let jitter = (half as f64 * r) as u64;
    let mut delay_ms = half + jitter;

    if let Some(after) = retry_after {
        delay_ms = delay_ms.max(u64::try_from(after.as_millis()).unwrap_or(u64::MAX));
    }
    Duration::from_millis(delay_ms)
}

/// A lock-free pseudo-random fraction in `[0, 1)` (splitmix64). Good enough for
/// jitter; needs no dependency, never blocks, and reads no clock — so it is
/// safe on the request hot path. Not for cryptographic use.
fn jitter01() -> f64 {
    const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
    static STATE: AtomicU64 = AtomicU64::new(GAMMA);
    let seed = STATE
        .fetch_add(GAMMA, Ordering::Relaxed)
        .wrapping_add(GAMMA);
    let mut z = seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // Top 53 bits scaled into [0, 1): exact, no precision loss for this range.
    #[allow(clippy::cast_precision_loss)]
    {
        (z >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Run `attempt_fn`, retrying retryable failures with backoff + jitter until it
/// succeeds, a non-retryable error occurs, or `max_attempts` is reached.
///
/// `attempt_fn` is expected to already bound each individual attempt (e.g. with
/// a first-token `timeout`); the caller bounds the *whole* call with the total
/// timeout. Cancellation (client disconnect) aborts immediately, mid-attempt or
/// mid-backoff.
pub async fn retry<F, Fut, T>(
    policy: &RetryPolicy,
    cancel: &CancellationToken,
    mut attempt_fn: F,
) -> Result<T, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    let mut failed = 0u32;
    loop {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        match attempt_fn().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                let next_attempt = failed + 1;
                if !error.is_retryable() || next_attempt >= policy.max_attempts {
                    return Err(error);
                }
                let delay = backoff_delay(failed, policy, error.retry_after(), jitter01());
                failed = next_attempt;
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(ProviderError::Cancelled),
                    () = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use tokio::time::Instant;

    fn policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base: Duration::from_millis(200),
            max: Duration::from_secs(5),
        }
    }

    #[test]
    fn backoff_is_exponential_within_equal_jitter_bounds() {
        let p = policy();
        // rand01 = 0 → the lower bound exp/2; rand01 → 1 → the upper bound exp.
        assert_eq!(backoff_delay(0, &p, None, 0.0), Duration::from_millis(100));
        assert_eq!(backoff_delay(0, &p, None, 1.0), Duration::from_millis(200));
        assert_eq!(backoff_delay(1, &p, None, 0.0), Duration::from_millis(200));
        assert_eq!(backoff_delay(1, &p, None, 1.0), Duration::from_millis(400));
        assert_eq!(backoff_delay(2, &p, None, 0.0), Duration::from_millis(400));
    }

    #[test]
    fn backoff_is_capped_at_max() {
        let p = policy();
        // 200·2^20 would be huge; the cap (5 s) applies before jitter.
        assert_eq!(backoff_delay(20, &p, None, 1.0), Duration::from_secs(5));
        assert_eq!(
            backoff_delay(20, &p, None, 0.0),
            Duration::from_millis(2500)
        );
    }

    #[test]
    fn retry_after_acts_as_a_floor() {
        let p = policy();
        // A 3 s Retry-After dominates the ~100–200 ms first backoff.
        let d = backoff_delay(0, &p, Some(Duration::from_secs(3)), 0.0);
        assert_eq!(d, Duration::from_secs(3));
        // But a tiny Retry-After does not shrink a larger computed backoff.
        let d = backoff_delay(2, &p, Some(Duration::from_millis(1)), 1.0);
        assert_eq!(d, Duration::from_millis(800));
    }

    #[test]
    fn jitter_stays_in_unit_interval() {
        for _ in 0..1000 {
            let r = jitter01();
            assert!((0.0..1.0).contains(&r), "jitter out of range: {r}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retries_until_success_then_stops() {
        let calls = AtomicU32::new(0);
        let cancel = CancellationToken::new();
        let result = retry(&policy(), &cancel, || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    Err(ProviderError::Upstream {
                        provider: "p".to_owned(),
                        status: 500,
                        retryable: true,
                    })
                } else {
                    Ok::<u32, ProviderError>(n)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 3, "500,500,200 → three calls");
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_delays_are_actually_waited() {
        let start = Instant::now();
        let cancel = CancellationToken::new();
        let _ = retry(&policy(), &cancel, || async {
            Err::<(), _>(ProviderError::Timeout {
                provider: "p".to_owned(),
            })
        })
        .await;
        // Two backoffs (after attempts 1 and 2), each ≥ exp/2 = 100 ms, 200 ms.
        assert!(
            start.elapsed() >= Duration::from_millis(300),
            "elapsed {:?} < 300 ms floor",
            start.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_loop_waits_at_least_the_retry_after() {
        // A 429 with Retry-After: 3 s must delay the retry by ≥ 3 s (simulated).
        let start = Instant::now();
        let cancel = CancellationToken::new();
        let calls = AtomicU32::new(0);
        let result = retry(&policy(), &cancel, || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err(ProviderError::RateLimited {
                        provider: "p".to_owned(),
                        retry_after: Some(Duration::from_secs(3)),
                    })
                } else {
                    Ok::<u32, ProviderError>(n)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 1);
        assert!(
            start.elapsed() >= Duration::from_secs(3),
            "Retry-After floor not honoured: {:?}",
            start.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn client_4xx_is_never_retried() {
        let calls = AtomicU32::new(0);
        let cancel = CancellationToken::new();
        let result: Result<(), _> = retry(&policy(), &cancel, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Err(ProviderError::Upstream {
                    provider: "p".to_owned(),
                    status: 400,
                    retryable: false,
                })
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "4xx must not retry");
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_during_backoff_aborts() {
        let cancel = CancellationToken::new();
        let child = cancel.clone();
        // Cancel shortly; the retry must surface Cancelled, not keep looping.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            child.cancel();
        });
        let result: Result<(), _> = retry(&policy(), &cancel, || async {
            Err(ProviderError::Unavailable {
                provider: "p".to_owned(),
            })
        })
        .await;
        assert!(matches!(result, Err(ProviderError::Cancelled)));
    }
}
