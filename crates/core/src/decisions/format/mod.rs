//! Edge formats of `POST /v1/decisions` (ADR 017): the OpenAI format
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

use super::{Answer, DecisionRequest, DecisionResponse, RawEntries};
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

/// Detect, parse and validate a request body.
///
/// # Errors
/// `LM-1001` for malformed JSON, an undetectable format, or any contract
/// violation; `LM-2011` for empty questions; `LM-2004` for a remote image
/// URL.
pub fn parse(bytes: &[u8]) -> Result<(Format, DecisionRequest), GatewayError> {
    let entries = object_entries(bytes, "request body").map_err(GatewayError::InvalidRequest)?;
    let Some(format) = detect(&entries) else {
        return Err(GatewayError::InvalidRequest(EITHER.to_owned()));
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

/// A starting capacity for a rendered response: the upstream entries' raw
/// sizes when re-emitted verbatim, else a per-answer allowance (an answer
/// with its probabilities), plus a little framing.
fn response_size_hint(resp: &DecisionResponse) -> usize {
    let body: usize = match &resp.upstream {
        Some(entries) => entries
            .iter()
            .map(|(key, value)| key.len() + value.get().len() + 4)
            .sum(),
        None => resp
            .answers
            .iter()
            .map(|a| match a {
                Answer::Choice { probabilities, .. } => 96 + 32 * probabilities.len(),
                Answer::Score { probabilities, .. } => 128 + 32 * probabilities.len(),
                Answer::Predicate { .. } | Answer::Refusal => 64,
            })
            .sum(),
    };
    body + 128
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

    // Messages are value-free (spec 12): serde's syntax and EOF messages
    // carry only a position, a duplicate names the key (structure), and a
    // type error, whose serde text quotes the value, becomes fixed text.
    serde_json::from_slice::<Entries>(bytes)
        .map(|e| e.0)
        .map_err(|e| {
            if e.is_syntax() || e.is_eof() {
                format!("malformed JSON {what}: {e}")
            } else if let Some(duplicate) = duplicate_field(&e) {
                format!("{what} has a {duplicate}")
            } else {
                format!("{what} must be a JSON object")
            }
        })
}

/// The `` duplicate field `key` `` part of a duplicate-key error (from
/// [`object_entries`] or a serde derive), without its position; `None` for
/// any other error. A key is structure, not content, so it may be named.
pub(crate) fn duplicate_field(e: &serde_json::Error) -> Option<String> {
    if !e.is_data() {
        return None;
    }
    let message = e.to_string();
    if !message.starts_with("duplicate field `") {
        return None;
    }
    let end = message.rfind(" at line ").unwrap_or(message.len());
    Some(message[..end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(body: &str) -> String {
        match parse(body.as_bytes()) {
            Err(GatewayError::InvalidRequest(m)) => m,
            other => panic!("expected LM-1001, got {other:?}"),
        }
    }

    #[test]
    fn detection_table() {
        let ts =
            r#"{"model":"jev","state":"s","questions":{"q":{"type":"noul","instructions":"i"}}}"#;
        assert!(matches!(parse(ts.as_bytes()), Ok((Format::TypeSafe, _))));
        for bad in [
            r#"{"model":"m","input":"x","state":"s","questions":[]}"#,
            r#"{"model":"m","questions":{}}"#,
            r#"{"model":"m","input":"x","questions":{"q":{}}}"#,
            r#"{"model":"m","state":"s","questions":[{"type":"predicate"}]}"#,
            r#"{"model":"m","state":"s","questions":"nope"}"#,
        ] {
            let m = err(bad);
            assert!(m.contains("either OpenAI format"), "{bad}: {m}");
        }
    }

    #[test]
    fn openai_bodies_detect_and_parse() {
        let body = r#"{"model":"gpt-6-luna","input":"x","questions":[{"type":"predicate","instructions":"i"}]}"#;
        assert!(matches!(parse(body.as_bytes()), Ok((Format::OpenAi, _))));
    }

    #[test]
    fn typesafe_edge_errors_never_echo_client_values() {
        let q = |question: &str| {
            format!(r#"{{"model":"m","state":"s","questions":{{"q1":{question}}}}}"#)
        };
        let cases = [
            r#""SECRET-BODY-STRING""#.to_owned(),
            "12345678901234567890123".to_owned(),
            r#"{"model":"m","state":"s","questions":"SECRET-QUESTIONS"}"#.to_owned(),
            q("98765"),
            q(r#"{"type":["SECRET-TYPE"],"instructions":"i"}"#),
            q(r#"{"type":"SECRET-UNKNOWN-TYPE","instructions":"i"}"#),
            q(r#"{"type":"noul","type":"SECRET-DUP","instructions":"i"}"#),
            r#"{"model":"m","state":["SECRET-STATE",{"type":"image_url"}],"questions":{"q":{"type":"noul","instructions":"i"}}}"#.to_owned(),
            r#"{"model":["SECRET-MODEL"],"state":"s","questions":{"q":{"type":"noul","instructions":"i"}}}"#.to_owned(),
            r#"{"model":"m","state":"SECRET-SYNTAX"#.to_owned(),
        ];
        for body in &cases {
            if let Err(e) = parse(body.as_bytes()) {
                let m = e.to_string();
                assert!(
                    !m.contains("SECRET") && !m.contains("98765") && !m.contains("1234567"),
                    "{body}: {m}"
                );
            }
        }
        // The question that is a number is named by its id, not its value.
        let m = err(&q("98765"));
        assert!(m.contains("`q1`") && m.contains("must be an object"), "{m}");
    }

    #[test]
    fn malformed_json_and_duplicate_keys_are_lm_1001() {
        assert!(err("{").contains("malformed"));
        assert!(err("[1]").contains("object"));
        assert!(err(r#"{"model":"a","model":"b"}"#).contains("duplicate"));
    }
}
