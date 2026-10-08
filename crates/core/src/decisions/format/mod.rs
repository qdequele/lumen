//! Edge formats of `POST /v1/decisions` (ADR 016): the OpenAI format
//! (`input`, `questions` array) and the TypeSafe format (`state`,
//! `questions` object). The body is split once into ordered raw top-level
//! entries; detection reads only the keys and the first byte of
//! `questions`, then the chosen parser works on the same entries.

pub mod openai;
pub mod typesafe;

use std::collections::HashSet;
use std::fmt;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;

use super::{DecisionRequest, DecisionResponse, RawEntries};
use crate::error::GatewayError;

/// A request or response format at the edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// OpenAI's `/v1/decisions` format.
    OpenAi,
    /// The TypeSafe format (TypeSafe, Perplexity, Ollama, ...).
    TypeSafe,
}

const EITHER: &str = "send either OpenAI format (`input`, `questions` array) or TypeSafe format \
                      (`state`, `questions` object)";

/// Detect, parse and validate a request body. `forced` is the format a
/// route accepts exclusively (`/v1/systemone`: TypeSafe).
///
/// # Errors
/// `LM-1001` for malformed JSON, an undetectable or forbidden format, or any
/// contract violation; `LM-2011` for empty questions; `LM-2004` for a
/// remote image URL.
pub fn parse(
    bytes: &[u8],
    forced: Option<Format>,
) -> Result<(Format, DecisionRequest), GatewayError> {
    let entries = object_entries(bytes, "request body").map_err(GatewayError::InvalidRequest)?;
    let detected = detect(&entries);
    let format = match (forced, detected) {
        (Some(Format::TypeSafe), Some(Format::OpenAi)) => {
            return Err(GatewayError::InvalidRequest(
                "/v1/systemone accepts only the TypeSafe format (`state`, `questions` object); \
                 send OpenAI-format bodies to /v1/decisions"
                    .to_owned(),
            ))
        }
        (Some(forced), _) => forced,
        (None, Some(format)) => format,
        (None, None) => return Err(GatewayError::InvalidRequest(EITHER.to_owned())),
    };
    let req = match format {
        Format::TypeSafe => typesafe::parse(entries)?,
        Format::OpenAi => openai::parse(entries)?,
    };
    Ok((format, req))
}

/// Render a response in `format`.
///
/// # Errors
/// [`GatewayError::Internal`] if serialization fails (it cannot for these
/// types; kept so the handler never panics).
pub fn render(
    format: Format,
    resp: &DecisionResponse,
    req: &DecisionRequest,
) -> Result<Vec<u8>, GatewayError> {
    match format {
        Format::TypeSafe => typesafe::render(resp, req),
        Format::OpenAi => openai::render(resp, req),
    }
}

/// `Some(format)` when the keys match exactly one format (spec 6.1).
fn detect(entries: &RawEntries) -> Option<Format> {
    let has = |k: &str| entries.iter().any(|(key, _)| key == k);
    let questions = entries
        .iter()
        .find(|(k, _)| k == "questions")
        .and_then(|(_, v)| v.get().trim_start().bytes().next());
    match (has("input"), has("state"), questions) {
        (true, false, Some(b'[')) => Some(Format::OpenAi),
        (false, true, Some(b'{')) => Some(Format::TypeSafe),
        _ => None,
    }
}

/// Split a JSON object into ordered `(key, raw value)` entries, rejecting
/// duplicate keys so what is validated is exactly what is forwarded.
///
/// # Errors
/// A message for malformed JSON, a non-object, or a duplicate key.
pub fn object_entries(bytes: &[u8], what: &str) -> Result<RawEntries, String> {
    struct EntriesVisitor;

    impl<'de> Visitor<'de> for EntriesVisitor {
        type Value = RawEntries;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a JSON object")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut entries: RawEntries = Vec::with_capacity(map.size_hint().unwrap_or(4));
            let mut seen = HashSet::new();
            while let Some(key) = map.next_key::<String>()? {
                if !seen.insert(key.clone()) {
                    return Err(serde::de::Error::custom(format!("duplicate field `{key}`")));
                }
                entries.push((key, map.next_value::<Box<RawValue>>()?));
            }
            Ok(entries)
        }
    }

    struct Entries(RawEntries);
    impl<'de> Deserialize<'de> for Entries {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_map(EntriesVisitor).map(Entries)
        }
    }

    serde_json::from_slice::<Entries>(bytes)
        .map(|e| e.0)
        .map_err(|e| {
            if e.is_syntax() || e.is_eof() {
                format!("malformed JSON {what}: {e}")
            } else {
                format!("{what} must be a JSON object: {e}")
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(body: &str, forced: Option<Format>) -> String {
        match parse(body.as_bytes(), forced) {
            Err(GatewayError::InvalidRequest(m)) => m,
            other => panic!("expected LM-1001, got {other:?}"),
        }
    }

    #[test]
    fn detection_table() {
        let ts =
            r#"{"model":"jev","state":"s","questions":{"q":{"type":"noul","instructions":"i"}}}"#;
        assert!(matches!(
            parse(ts.as_bytes(), None),
            Ok((Format::TypeSafe, _))
        ));
        for bad in [
            r#"{"model":"m","input":"x","state":"s","questions":[]}"#,
            r#"{"model":"m","questions":{}}"#,
            r#"{"model":"m","input":"x","questions":{"q":{}}}"#,
            r#"{"model":"m","state":"s","questions":[{"type":"predicate"}]}"#,
            r#"{"model":"m","state":"s","questions":"nope"}"#,
        ] {
            let m = err(bad, None);
            assert!(m.contains("either OpenAI format"), "{bad}: {m}");
        }
    }

    #[test]
    fn openai_bodies_detect_and_parse() {
        let body = r#"{"model":"gpt-6-luna","input":"x","questions":[{"type":"predicate","instructions":"i"}]}"#;
        assert!(matches!(
            parse(body.as_bytes(), None),
            Ok((Format::OpenAi, _))
        ));
    }

    #[test]
    fn systemone_forces_the_typesafe_format() {
        let openai =
            r#"{"model":"m","input":"x","questions":[{"type":"predicate","instructions":"i"}]}"#;
        assert!(err(openai, Some(Format::TypeSafe)).contains("/v1/systemone"));
        // A TypeSafe body missing `state` keeps the precise TypeSafe message.
        let missing = r#"{"model":"jev","questions":{"q":{"type":"noul","instructions":"i"}}}"#;
        assert!(err(missing, Some(Format::TypeSafe)).contains("`state`"));
    }

    #[test]
    fn malformed_json_and_duplicate_keys_are_lm_1001() {
        assert!(err("{", None).contains("malformed"));
        assert!(err("[1]", None).contains("object"));
        assert!(err(r#"{"model":"a","model":"b"}"#, None).contains("duplicate"));
    }
}
