//! Shared helpers for mapping upstream HTTP responses to [`ProviderError`].
//!
//! Every provider maps error statuses the same way, so the policy lives here:
//! 429 → rate limited (with `Retry-After`), 5xx → retryable upstream error,
//! other non-2xx → fatal upstream error. Providers translate their own success
//! bodies but share this failure classification.

use lumen_core::{EmbedInput, ProviderError};
use std::time::Duration;

/// Reject pre-tokenized embedding input (token-id arrays) for providers whose
/// APIs only take text. Called BEFORE any upstream call so the client gets an
/// honest 400 (`LM-1001`) instead of an empty result (Cohere/TEI would send an
/// empty texts array) or an opaque upstream error (rule 8). OpenAI-compatible
/// passthrough providers consume token arrays natively and never call this.
///
/// # Errors
///
/// Returns [`ProviderError::UnsupportedInput`] when `input` is `Tokens` or
/// `TokenBatch`; `Ok(())` for every text/multimodal shape.
pub fn reject_pretokenized_input(provider: &str, input: &EmbedInput) -> Result<(), ProviderError> {
    match input {
        EmbedInput::Tokens(_) | EmbedInput::TokenBatch(_) => Err(ProviderError::UnsupportedInput {
            provider: provider.to_owned(),
            reason: "pre-tokenized input (token id arrays)".to_owned(),
        }),
        EmbedInput::Single(_) | EmbedInput::Batch(_) | EmbedInput::Multi(_) => Ok(()),
    }
}

/// Handle OpenAI request fields a translated provider's wire schema cannot
/// express (issue #72). OpenAI-compatible providers forward `ChatRequest.extra`
/// verbatim, so these fields silently work there; a translated provider must
/// either reject or knowingly drop them, never lose them invisibly.
///
/// Called BEFORE any upstream call. In strict mode the first offending field
/// is rejected with an honest 400 (`LM-1001`, rule 8) naming the field and
/// provider; lenient mode (the default) drops each with a `debug!` trace,
/// matching the Ollama `dimensions` precedent (issue #25). A field set to
/// JSON `null` counts as absent (OpenAI treats it as unset).
///
/// # Errors
///
/// Returns [`ProviderError::UnsupportedField`] in strict mode when `extra`
/// carries any of `unsupported`; `Ok(())` otherwise.
pub fn check_unsupported_chat_fields(
    provider: &str,
    strict: bool,
    extra: &serde_json::Map<String, serde_json::Value>,
    unsupported: &[&str],
) -> Result<(), ProviderError> {
    for &field in unsupported {
        if extra.get(field).is_some_and(|v| !v.is_null()) {
            if strict {
                return Err(ProviderError::UnsupportedField {
                    provider: provider.to_owned(),
                    field: field.to_owned(),
                });
            }
            tracing::debug!(
                provider,
                field,
                "dropping chat request field this provider cannot honor"
            );
        }
    }
    Ok(())
}

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

    #[test]
    fn strict_mode_rejects_the_first_unsupported_chat_field_naming_it() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "response_format".to_owned(),
            serde_json::json!({ "type": "json_object" }),
        );
        let err = check_unsupported_chat_fields("anthropic", true, &extra, &["response_format"])
            .unwrap_err();
        match err {
            ProviderError::UnsupportedField { provider, field } => {
                assert_eq!(provider, "anthropic");
                assert_eq!(field, "response_format");
            }
            other => panic!("expected UnsupportedField, got {other:?}"),
        }
    }

    #[test]
    fn lenient_mode_accepts_unsupported_chat_fields() {
        let mut extra = serde_json::Map::new();
        extra.insert("seed".to_owned(), serde_json::json!(42));
        extra.insert("logprobs".to_owned(), serde_json::json!(true));
        assert!(check_unsupported_chat_fields("p", false, &extra, &["seed", "logprobs"]).is_ok());
    }

    #[test]
    fn null_valued_and_absent_fields_pass_even_in_strict_mode() {
        let mut extra = serde_json::Map::new();
        extra.insert("seed".to_owned(), serde_json::Value::Null);
        assert!(check_unsupported_chat_fields("p", true, &extra, &["seed", "logprobs"]).is_ok());
    }
}
