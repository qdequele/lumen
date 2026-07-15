//! Shared helpers for mapping upstream HTTP responses to [`ProviderError`].
//!
//! Every provider maps error statuses the same way, so the policy lives here:
//! 429 → rate limited (with `Retry-After`), 5xx → retryable upstream error,
//! other non-2xx → fatal upstream error. Providers translate their own success
//! bodies but share this failure classification.

use lumen_core::ProviderError;
use std::time::Duration;

/// Classify a non-success upstream status into a [`ProviderError`].
#[must_use]
pub fn classify_status(
    provider: &str,
    status: u16,
    retry_after: Option<Duration>,
) -> ProviderError {
    match status {
        429 => ProviderError::RateLimited {
            provider: provider.to_owned(),
            retry_after,
        },
        // Server-side failures are worth retrying (possibly on a fallback).
        500..=599 => ProviderError::Upstream {
            provider: provider.to_owned(),
            status,
            retryable: true,
        },
        // Other 4xx (401/403/404/422/...) are the caller's fault: not retryable.
        _ => ProviderError::Upstream {
            provider: provider.to_owned(),
            status,
            retryable: false,
        },
    }
}

/// Parse a `Retry-After` header expressed in delta-seconds. HTTP-date form is
/// intentionally not handled in v1 (returns `None`).
#[must_use]
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Current unix time in whole seconds, for response `created` timestamps.
///
/// Providers whose upstream API does not return a creation time (Anthropic,
/// ...) stamp the translated response with this instead of a hardcoded `0`.
/// Falls back to `0` on a pre-epoch system clock rather than panicking - a
/// nonsensical local clock must never take request handling down.
#[must_use]
pub fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_429_is_rate_limited_with_retry_after() {
        let err = classify_status("openai", 429, Some(Duration::from_secs(2)));
        match err {
            ProviderError::RateLimited {
                provider,
                retry_after,
            } => {
                assert_eq!(provider, "openai");
                assert_eq!(retry_after, Some(Duration::from_secs(2)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn status_500_is_retryable_upstream() {
        match classify_status("openai", 503, None) {
            ProviderError::Upstream {
                status, retryable, ..
            } => {
                assert_eq!(status, 503);
                assert!(retryable);
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn status_401_is_fatal_upstream() {
        match classify_status("openai", 401, None) {
            ProviderError::Upstream {
                status, retryable, ..
            } => {
                assert_eq!(status, 401);
                assert!(!retryable);
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn retry_after_parses_only_delta_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "5".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(5)));

        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2025 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn unix_timestamp_is_a_plausible_recent_value() {
        // Sanity-bounds the result against a fixed past instant (2024-01-01
        // UTC) without pinning it to an exact wall-clock value.
        assert!(unix_timestamp() > 1_704_067_200);
    }
}
