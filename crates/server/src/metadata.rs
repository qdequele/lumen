//! Per-request metadata header (ADR 002, Cloudflare AI Gateway style).
//!
//! `x-lumen-metadata` (alias `cf-aig-metadata`) carries a **flat** JSON
//! object of `string → string | number | bool`, parsed once here at the edge.
//! It feeds two sinks with different rules:
//!
//! * **logs / `usage_log`** - the whole (bounded) object;
//! * **Prometheus** - ONLY the keys in the operator's allowlist
//!   (`telemetry.metadata_labels`), everything else stays logs-only.
//!
//! Malformed, oversized or wrong-typed metadata is dropped with a `warn!` and
//! a `metadata_rejected_total` increment; the request itself NEVER fails.
//! The value is opaque: never inspected, and documented as logged (so it must
//! not carry secrets or prompt content).

use axum::http::HeaderMap;
use serde_json::Value;
use std::borrow::Cow;

/// Canonical header name.
pub const METADATA_HEADER: &str = "x-lumen-metadata";
/// Cloudflare-compatible alias, honoured when the canonical header is absent.
pub const METADATA_HEADER_ALIAS: &str = "cf-aig-metadata";

/// Whole-header size cap (bytes).
const MAX_HEADER_BYTES: usize = 4 * 1024;
/// Maximum number of keys.
const MAX_KEYS: usize = 16;
/// Maximum key length (bytes).
const MAX_KEY_BYTES: usize = 64;
/// Maximum string-value length (bytes).
const MAX_VALUE_BYTES: usize = 256;

/// The parsed, validated metadata of one request.
///
/// Values keep their original JSON type (string, number or bool) so the
/// `usage_log.metadata` column stores typed JSON (`{"batch":42}`, not
/// `{"batch":"42"}`) and SQLite `json_extract` can filter numerically.
/// Prometheus labels, which are always strings, stringify on the way out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestMetadata {
    /// Key → typed value; only `String`, `Number` and `Bool` ever appear.
    pairs: Vec<(String, Value)>,
}

/// Outcome of looking for metadata on a request.
#[derive(Debug, Clone, PartialEq)]
pub enum MetadataOutcome {
    /// No metadata header present (the common, zero-allocation case).
    Absent,
    /// A valid header was parsed.
    Valid(RequestMetadata),
    /// A header was present but malformed or out of bounds; it was dropped.
    /// Carries a static reason for the log line.
    Rejected(&'static str),
}

impl RequestMetadata {
    /// Extract and validate the metadata header, if any.
    #[must_use]
    pub fn extract(headers: &HeaderMap) -> MetadataOutcome {
        let raw = headers
            .get(METADATA_HEADER)
            .or_else(|| headers.get(METADATA_HEADER_ALIAS));
        let Some(raw) = raw else {
            return MetadataOutcome::Absent;
        };
        if raw.as_bytes().len() > MAX_HEADER_BYTES {
            return MetadataOutcome::Rejected("header exceeds 4 KiB");
        }
        let Ok(text) = raw.to_str() else {
            return MetadataOutcome::Rejected("header is not valid UTF-8");
        };
        let Ok(Value::Object(object)) = serde_json::from_str::<Value>(text) else {
            return MetadataOutcome::Rejected("not a JSON object");
        };
        if object.len() > MAX_KEYS {
            return MetadataOutcome::Rejected("more than 16 keys");
        }
        let mut pairs = Vec::with_capacity(object.len());
        for (key, value) in object {
            if key.len() > MAX_KEY_BYTES {
                return MetadataOutcome::Rejected("key exceeds 64 bytes");
            }
            match &value {
                Value::String(s) => {
                    if s.len() > MAX_VALUE_BYTES {
                        return MetadataOutcome::Rejected("value exceeds 256 bytes");
                    }
                }
                Value::Number(_) | Value::Bool(_) => {}
                // Flat objects only: nesting is rejected, not flattened.
                Value::Null | Value::Array(_) | Value::Object(_) => {
                    return MetadataOutcome::Rejected("values must be string/number/bool");
                }
            }
            // The original typed value is retained (see the struct doc).
            pairs.push((key, value));
        }
        MetadataOutcome::Valid(RequestMetadata { pairs })
    }

    /// True when no pairs were supplied.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Compact JSON for the `usage_log.metadata` column and structured logs,
    /// with values kept in their original JSON type.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut map = serde_json::Map::with_capacity(self.pairs.len());
        for (key, value) in &self.pairs {
            map.insert(key.clone(), value.clone());
        }
        Value::Object(map).to_string()
    }

    /// Label values aligned with the allowlist order; absent keys become `""`
    /// (ADR 002 sink 2 - only allowlisted keys ever reach Prometheus).
    /// Numbers and bools stringify here since Prometheus labels are strings.
    #[must_use]
    pub fn label_values<'a>(&'a self, allowlist: &[String]) -> Vec<Cow<'a, str>> {
        allowlist
            .iter()
            .map(|wanted| {
                self.pairs
                    .iter()
                    .find(|(key, _)| key == wanted)
                    .map_or(Cow::Borrowed(""), |(_, value)| render_label(value))
            })
            .collect()
    }
}

/// Render a metadata value as a Prometheus label string, borrowing the string
/// case and only allocating for numbers/bools.
fn render_label(value: &Value) -> Cow<'_, str> {
    match value {
        Value::String(s) => Cow::Borrowed(s.as_str()),
        Value::Number(n) => Cow::Owned(n.to_string()),
        Value::Bool(b) => Cow::Owned(b.to_string()),
        // Never constructed (extract rejects other variants); render as empty.
        _ => Cow::Borrowed(""),
    }
}

/// All-empty label values for requests without metadata.
#[must_use]
pub fn empty_label_values(allowlist: &[String]) -> Vec<Cow<'static, str>> {
    vec![Cow::Borrowed(""); allowlist.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(name: &str, value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).expect("valid name"),
            HeaderValue::from_str(value).expect("valid value"),
        );
        headers
    }

    #[test]
    fn absent_header_is_absent() {
        assert_eq!(
            RequestMetadata::extract(&HeaderMap::new()),
            MetadataOutcome::Absent
        );
    }

    #[test]
    fn valid_flat_object_keeps_json_value_types() {
        let headers = headers_with(
            METADATA_HEADER,
            r#"{"team":"search","batch":42,"canary":true}"#,
        );
        let MetadataOutcome::Valid(meta) = RequestMetadata::extract(&headers) else {
            panic!("expected valid metadata");
        };
        // The usage-log JSON keeps the original types: string stays quoted,
        // number and bool stay unquoted (so numeric filtering works).
        let json = meta.to_json();
        assert!(json.contains(r#""team":"search""#));
        assert!(json.contains(r#""batch":42"#), "number stays typed: {json}");
        assert!(
            json.contains(r#""canary":true"#),
            "bool stays typed: {json}"
        );
        assert!(!json.contains(r#""batch":"42""#), "not stringified: {json}");

        // Prometheus labels, however, stringify (labels are always strings).
        let allowlist = vec!["batch".to_owned(), "canary".to_owned()];
        let labels = meta.label_values(&allowlist);
        let labels: Vec<&str> = labels.iter().map(std::convert::AsRef::as_ref).collect();
        assert_eq!(labels, vec!["42", "true"]);
    }

    #[test]
    fn cloudflare_alias_is_honoured() {
        let headers = headers_with(METADATA_HEADER_ALIAS, r#"{"env":"prod"}"#);
        assert!(matches!(
            RequestMetadata::extract(&headers),
            MetadataOutcome::Valid(_)
        ));
    }

    #[test]
    fn canonical_header_wins_over_the_alias() {
        let mut headers = headers_with(METADATA_HEADER, r#"{"src":"canonical"}"#);
        headers.insert(
            METADATA_HEADER_ALIAS,
            HeaderValue::from_static(r#"{"src":"alias"}"#),
        );
        let MetadataOutcome::Valid(meta) = RequestMetadata::extract(&headers) else {
            panic!("expected valid metadata");
        };
        assert!(meta.to_json().contains("canonical"));
    }

    #[test]
    fn malformed_json_is_rejected() {
        let headers = headers_with(METADATA_HEADER, "not json at all");
        assert!(matches!(
            RequestMetadata::extract(&headers),
            MetadataOutcome::Rejected(_)
        ));
    }

    #[test]
    fn non_object_and_nested_values_are_rejected() {
        for bad in [
            r#"["a"]"#,
            r#""str""#,
            r#"{"k":{"nested":1}}"#,
            r#"{"k":[1]}"#,
            r#"{"k":null}"#,
        ] {
            let headers = headers_with(METADATA_HEADER, bad);
            assert!(
                matches!(
                    RequestMetadata::extract(&headers),
                    MetadataOutcome::Rejected(_)
                ),
                "should reject {bad}"
            );
        }
    }

    #[test]
    fn bounds_are_enforced() {
        // > 16 keys
        use std::fmt::Write;
        let mut many = String::new();
        for i in 0..17 {
            let _ = write!(many, r#""k{i}":1,"#);
        }
        let json = format!("{{{}}}", many.trim_end_matches(','));
        assert!(matches!(
            RequestMetadata::extract(&headers_with(METADATA_HEADER, &json)),
            MetadataOutcome::Rejected(_)
        ));

        // key > 64 bytes
        let long_key = format!(r#"{{"{}":1}}"#, "k".repeat(65));
        assert!(matches!(
            RequestMetadata::extract(&headers_with(METADATA_HEADER, &long_key)),
            MetadataOutcome::Rejected(_)
        ));

        // string value > 256 bytes
        let long_value = format!(r#"{{"k":"{}"}}"#, "v".repeat(257));
        assert!(matches!(
            RequestMetadata::extract(&headers_with(METADATA_HEADER, &long_value)),
            MetadataOutcome::Rejected(_)
        ));

        // whole header > 4 KiB
        let huge = format!(r#"{{"k":"{}"}}"#, "v".repeat(5000));
        assert!(matches!(
            RequestMetadata::extract(&headers_with(METADATA_HEADER, &huge)),
            MetadataOutcome::Rejected(_)
        ));
    }

    #[test]
    fn label_values_follow_the_allowlist_order_and_default_empty() {
        let headers = headers_with(METADATA_HEADER, r#"{"team":"search","secretish":"x"}"#);
        let MetadataOutcome::Valid(meta) = RequestMetadata::extract(&headers) else {
            panic!("expected valid metadata");
        };
        let allowlist = vec!["env".to_owned(), "team".to_owned()];
        // Non-allowlisted keys ("secretish") never surface here.
        let labels = meta.label_values(&allowlist);
        let labels: Vec<&str> = labels.iter().map(std::convert::AsRef::as_ref).collect();
        assert_eq!(labels, vec!["", "search"]);
        let empty = empty_label_values(&allowlist);
        let empty: Vec<&str> = empty.iter().map(std::convert::AsRef::as_ref).collect();
        assert_eq!(empty, vec!["", ""]);
    }
}
