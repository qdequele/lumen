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

/// Structured error codes of an input longer than the model's context window,
/// lowercase (OpenAI and compatibles). Matched only as the exact string value
/// of a code-like key (see [`has_error_code`]), never as a substring.
const CONTEXT_LENGTH_CODES: &[&str] = &["context_length_exceeded"];

/// Error-message phrases of an input longer than the model's context window,
/// lowercase, per vendor (Anthropic, Google/Vertex, Bedrock, Mistral, Cohere,
/// OpenAI). Multi-word sentences from rejection messages, never a bare
/// identifier that a parameter name could also spell.
const CONTEXT_LENGTH_PHRASES: &[&str] = &[
    "maximum context length",
    "prompt is too long",
    "input is too long",
    "too many tokens",
    "exceeds the maximum number of tokens",
    "context window",
    "too large for model",
];

/// Structured error codes of a content-policy refusal, lowercase (Azure
/// OpenAI `content_filter` and its inner `ResponsibleAIPolicyViolation`,
/// OpenAI `content_policy_violation`).
const CONTENT_FILTER_CODES: &[&str] = &[
    "content_filter",
    "content_policy_violation",
    "responsibleaipolicyviolation",
];

/// Error-message phrases of a content-policy refusal, lowercase.
const CONTENT_FILTER_PHRASES: &[&str] = &["content management policy", "blocked by safety"];

/// JSON keys whose string value is an error code worth matching.
const CODE_KEYS: &[&str] = &["code", "type", "reason", "status"];

/// Statuses whose body is worth classifying (client errors that may be a
/// context-length or content-policy refusal).
#[must_use]
pub const fn needs_error_body(status: u16) -> bool {
    matches!(status, 400 | 403 | 413 | 422)
}

/// Whether the (lowercase) body carries `code` as the exact string value of a
/// [`CODE_KEYS`] key, as in `"code": "context_length_exceeded"`. A scan rather
/// than a JSON parse, so a body cut at the read bound still classifies; the
/// quotes rule out a longer identifier containing the code, and the key check
/// rules out the same word used as a parameter name or inside a message.
fn has_error_code(text: &str, code: &str) -> bool {
    let quoted = format!("\"{code}\"");
    text.match_indices(&quoted)
        .any(|(at, _)| preceding_key(&text[..at]).is_some_and(|k| CODE_KEYS.contains(&k)))
}

/// The JSON key right before a string value that starts at the end of
/// `before` (`"key" :` then optional whitespace), if any.
fn preceding_key(before: &str) -> Option<&str> {
    let rest = before.trim_end().strip_suffix(':')?.trim_end();
    let rest = rest.strip_suffix('"')?;
    let open = rest.rfind('"')?;
    Some(&rest[open + 1..])
}

/// [`classify_status`], refined by the (bounded) error body: a client error
/// whose body carries a known context-length or content-policy error code (as
/// a structured field) or rejection phrase maps to
/// [`ProviderError::ContextLengthExceeded`] / [`ProviderError::ContentFiltered`]
/// (ADR 014). The body is only searched, never logged or returned.
#[must_use]
pub fn classify_error(
    provider: &str,
    status: u16,
    retry_after: Option<Duration>,
    body: &[u8],
) -> ProviderError {
    if needs_error_body(status) && !body.is_empty() {
        let text = String::from_utf8_lossy(body).to_ascii_lowercase();
        let matches = |codes: &[&str], phrases: &[&str]| {
            codes.iter().any(|c| has_error_code(&text, c))
                || phrases.iter().any(|p| text.contains(p))
        };
        if matches(CONTEXT_LENGTH_CODES, CONTEXT_LENGTH_PHRASES) {
            return ProviderError::ContextLengthExceeded {
                provider: provider.to_owned(),
                status,
            };
        }
        if matches(CONTENT_FILTER_CODES, CONTENT_FILTER_PHRASES) {
            return ProviderError::ContentFiltered {
                provider: provider.to_owned(),
                status,
            };
        }
    }
    classify_status(provider, status, retry_after)
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

    #[test]
    fn context_length_bodies_are_classified_per_vendor() {
        let cases: &[&[u8]] = &[
            br#"{"error":{"code":"context_length_exceeded","message":"..."}}"#,
            br#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#,
            br#"{"error":{"message":"The input token count (1200000) exceeds the maximum number of tokens allowed (1048576)."}}"#,
            br#"{"message":"Input is too long for requested model."}"#,
            br#"{"message":"too many tokens: total number of tokens in the prompt cannot exceed 128000"}"#,
        ];
        for body in cases {
            match classify_error("p", 400, None, body) {
                ProviderError::ContextLengthExceeded { provider, status } => {
                    assert_eq!((provider.as_str(), status), ("p", 400));
                }
                other => panic!(
                    "expected context length for {:?}, got {other:?}",
                    String::from_utf8_lossy(body)
                ),
            }
        }
    }

    #[test]
    fn content_filter_bodies_are_classified() {
        let body = br#"{"error":{"code":"content_filter","message":"The response was filtered"}}"#;
        assert!(matches!(
            classify_error("azure", 400, None, body),
            ProviderError::ContentFiltered { status: 400, .. }
        ));
        let body = br#"{"error":{"code":"content_policy_violation"}}"#;
        assert!(matches!(
            classify_error("openai", 400, None, body),
            ProviderError::ContentFiltered { .. }
        ));
    }

    #[test]
    fn a_code_word_outside_a_code_field_is_not_a_refusal() {
        let cases: &[&[u8]] = &[
            // The marker as a parameter name, in the message and in `param`.
            br#"{"error":{"code":"invalid_parameter","message":"Unsupported parameter: content_filter"}}"#,
            br#"{"error":{"code":"invalid_parameter","param":"content_filter"}}"#,
            // A longer identifier that contains a marker.
            br#"{"error":{"code":"context_length_exceeded_for_tools_schema"}}"#,
            br#"{"error":{"code":"not_a_content_filter"}}"#,
        ];
        for body in cases {
            assert!(
                matches!(
                    classify_error("p", 400, None, body),
                    ProviderError::Upstream { status: 400, .. }
                ),
                "{:?}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn a_code_is_matched_with_spacing_and_in_a_truncated_body() {
        assert!(matches!(
            classify_error(
                "p",
                400,
                None,
                br#"{"error": {"code" : "Context_Length_Exceeded", "mess"#
            ),
            ProviderError::ContextLengthExceeded { .. }
        ));
        assert!(matches!(
            classify_error(
                "azure",
                400,
                None,
                br#"{"error":{"code":"x","innererror":{"code":"ResponsibleAIPolicyViolation"}}}"#
            ),
            ProviderError::ContentFiltered { .. }
        ));
    }

    #[test]
    fn unrecognised_bodies_and_other_statuses_fall_back_to_classify_status() {
        assert!(matches!(
            classify_error("p", 400, None, br#"{"error":"bad field"}"#),
            ProviderError::Upstream {
                status: 400,
                retryable: false,
                ..
            }
        ));
        // A 5xx is never reclassified, whatever its body says.
        assert!(matches!(
            classify_error("p", 503, None, br"context_length_exceeded"),
            ProviderError::Upstream {
                status: 503,
                retryable: true,
                ..
            }
        ));
        assert!(matches!(
            classify_error("p", 429, None, b""),
            ProviderError::RateLimited { .. }
        ));
    }

    #[test]
    fn the_new_variants_are_client_errors_not_provider_faults() {
        let e = ProviderError::ContextLengthExceeded {
            provider: "p".into(),
            status: 400,
        };
        assert!(!e.is_retryable());
        assert!(!e.is_provider_fault());
        let g = lumen_core::GatewayError::from_provider("p", e);
        assert_eq!(
            g.http_status(),
            lumen_core::GatewayError::Upstream {
                provider: "p".into(),
                status: 400
            }
            .http_status()
        );
    }
}
