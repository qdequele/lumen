//! SystemOne (typed decision) types - ADR 013.
//!
//! A SystemOne model (TypeSafe's Jev is the first) evaluates a `state` against
//! a map of named, typed questions (`noul`, `choice`, `score`) and returns one
//! typed answer per question id, with calibrated probabilities. The public
//! `POST /v1/systemone` shape is TypeSafe's own, so its SDKs work against the
//! gateway by pointing `TYPESAFE_BASE_URL` at it.
//!
//! The gateway validates the documented request contract (see
//! [`SystemOneRequest::validate`]) but never interprets the content: `state`,
//! each question body, unknown top-level fields and every response field are
//! carried as raw JSON ([`RawValue`]). That keeps object key order exactly as
//! sent (the workspace `serde_json` sorts `Map` keys, and key order inside
//! `state` or `choice` criteria is visible to the model) and avoids
//! re-serializing a state that can run to tens of thousands of tokens.
//!
//! Key order is only preserved when deserializing from JSON text
//! (`serde_json::from_str` / `from_slice`, which axum's `Json` extractor
//! uses); going through a `serde_json::Value` first loses it.

use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use crate::error::GatewayError;

/// Most options a `choice` question may define (TypeSafe API limit, 2026-09).
pub const MAX_CHOICE_OPTIONS: usize = 255;

/// Most levels a `score` question may define (TypeSafe API limit, 2026-09).
pub const MAX_SCORE_LEVELS: usize = 10;

/// The question types the gateway knows how to validate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionType {
    /// A yes/no question; the answer is the probability of yes.
    Noul,
    /// Pick one option from a defined set.
    Choice,
    /// Rate the state against ordered levels.
    Score,
}

impl QuestionType {
    /// Parse the wire `type` value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "noul" => Some(Self::Noul),
            "choice" => Some(Self::Choice),
            "score" => Some(Self::Score),
            _ => None,
        }
    }
}

/// An ordered JSON object whose values are kept as raw, unparsed JSON.
pub type RawEntries = Vec<(String, Box<RawValue>)>;

/// A SystemOne evaluation request (`POST /v1/systemone`).
///
/// Everything but `model` is immutable and shared behind an [`Arc`], so the
/// per-attempt clone the retry/fallback executor makes is a refcount bump,
/// not a copy of a state that can weigh 100+ KB.
#[derive(Debug, Clone)]
pub struct SystemOneRequest {
    /// Client-facing model id (rewritten to the upstream id per attempt).
    pub model: String,
    body: Arc<RequestBody>,
}

#[derive(Debug)]
struct RequestBody {
    state: Box<RawValue>,
    questions: RawEntries,
    extra: RawEntries,
}

impl SystemOneRequest {
    /// Build a request programmatically (e.g. the rerank converter). Call
    /// [`validate`](Self::validate) if the parts are not known-good.
    #[must_use]
    pub fn new(model: String, state: Box<RawValue>, questions: RawEntries) -> Self {
        Self {
            model,
            body: Arc::new(RequestBody {
                state,
                questions,
                extra: Vec::new(),
            }),
        }
    }

    /// The content to evaluate: a string, object or array, verbatim.
    #[must_use]
    pub fn state(&self) -> &RawValue {
        &self.body.state
    }

    /// The named questions, in request order, each body verbatim.
    #[must_use]
    pub fn questions(&self) -> &[(String, Box<RawValue>)] {
        &self.body.questions
    }

    /// Unknown top-level fields, forwarded verbatim.
    #[must_use]
    pub fn extra(&self) -> &[(String, Box<RawValue>)] {
        &self.body.extra
    }

    /// Validate the documented request contract (ADR 013 §3) so a malformed
    /// question fails fast with a precise `LM-1001` instead of an opaque
    /// upstream `422` surfacing as `LM-3003`.
    ///
    /// # Errors
    /// * [`GatewayError::EmptyQuestions`] (`LM-2011`) for an empty `questions` map.
    /// * [`GatewayError::InvalidRequest`] (`LM-1001`) for an empty `model`, a
    ///   null `state`, a duplicate question id or any malformed question.
    pub fn validate(&self) -> Result<(), GatewayError> {
        let invalid = |msg: &str| Err(GatewayError::InvalidRequest(msg.to_owned()));
        if self.model.is_empty() {
            return invalid("`model` must not be empty");
        }
        if self.body.state.get() == "null" {
            return invalid("`state` must not be null");
        }
        if self.body.questions.is_empty() {
            return Err(GatewayError::EmptyQuestions);
        }
        let mut seen = HashSet::with_capacity(self.body.questions.len());
        for (id, body) in &self.body.questions {
            if !seen.insert(id.as_str()) {
                return Err(GatewayError::InvalidRequest(format!(
                    "duplicate question id `{id}`"
                )));
            }
            validate_question(id, body).map_err(GatewayError::InvalidRequest)?;
        }
        Ok(())
    }
}

/// The fields of a question the gateway inspects, in one pass; everything
/// else is skipped. `Option` maps JSON `null` to `None`.
#[derive(Deserialize)]
struct QuestionView {
    #[serde(rename = "type")]
    kind: Option<String>,
    instructions: Option<IgnoredAny>,
    criteria: Option<CriteriaShape>,
}

/// The shape of a `criteria` value, counted without materializing it.
enum CriteriaShape {
    Object(usize),
    Array(usize),
    Other,
}

impl<'de> Deserialize<'de> for CriteriaShape {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ShapeVisitor;

        impl<'de> Visitor<'de> for ShapeVisitor {
            type Value = CriteriaShape;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut len = 0;
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {
                    len += 1;
                }
                Ok(CriteriaShape::Object(len))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut len = 0;
                while seq.next_element::<IgnoredAny>()?.is_some() {
                    len += 1;
                }
                Ok(CriteriaShape::Array(len))
            }

            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(CriteriaShape::Other)
            }

            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(CriteriaShape::Other)
            }

            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(CriteriaShape::Other)
            }

            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(CriteriaShape::Other)
            }

            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(CriteriaShape::Other)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(CriteriaShape::Other)
            }
        }

        deserializer.deserialize_any(ShapeVisitor)
    }
}

fn validate_question(id: &str, body: &RawValue) -> Result<(), String> {
    let view: QuestionView = serde_json::from_str(body.get()).map_err(|e| {
        if body.get().starts_with('{') {
            format!("question `{id}` is malformed: {e}")
        } else {
            format!("question `{id}` must be an object")
        }
    })?;
    let kind = view
        .kind
        .ok_or_else(|| format!("question `{id}` is missing `type`"))?;
    let kind = QuestionType::parse(&kind).ok_or_else(|| {
        format!("question `{id}` has unknown type '{kind}': expected noul, choice or score")
    })?;
    if view.instructions.is_none() {
        return Err(format!("question `{id}` is missing `instructions`"));
    }
    match (kind, view.criteria) {
        (QuestionType::Noul, None | Some(CriteriaShape::Object(_))) => Ok(()),
        (QuestionType::Noul, Some(_)) => Err(format!(
            "question `{id}`: noul `criteria` must be an object"
        )),
        (QuestionType::Choice | QuestionType::Score, None) => {
            let name = if kind == QuestionType::Choice {
                "choice"
            } else {
                "score"
            };
            Err(format!("question `{id}`: {name} requires `criteria`"))
        }
        (QuestionType::Choice, Some(CriteriaShape::Object(n))) => {
            if n == 0 || n > MAX_CHOICE_OPTIONS {
                Err(format!(
                    "question `{id}`: choice needs 1 to {MAX_CHOICE_OPTIONS} options, got {n}"
                ))
            } else {
                Ok(())
            }
        }
        (QuestionType::Choice, Some(_)) => Err(format!(
            "question `{id}`: choice `criteria` must be an object of options"
        )),
        (QuestionType::Score, Some(CriteriaShape::Array(n))) => {
            if n == 0 || n > MAX_SCORE_LEVELS {
                Err(format!(
                    "question `{id}`: score needs 1 to {MAX_SCORE_LEVELS} levels, got {n}"
                ))
            } else {
                Ok(())
            }
        }
        (QuestionType::Score, Some(_)) => Err(format!(
            "question `{id}`: score `criteria` must be an array of levels"
        )),
    }
}

/// Token usage for a SystemOne call. Upstream-reported when available;
/// otherwise the gateway fills it and sets `estimated` (ADR 003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SystemOneUsage {
    /// Tokens ingested (state plus questions). The billed unit for Jev.
    #[serde(default)]
    pub input_tokens: u32,
    /// Tokens of the typed answers (not billed by Jev).
    #[serde(default)]
    pub output_tokens: u32,
    /// `Some(true)` when the gateway estimated the counts itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated: Option<bool>,
}

/// A SystemOne evaluation response.
///
/// Every upstream field is kept raw, in upstream order, and re-emitted
/// verbatim, so the client sees TypeSafe's bytes. The one exception is
/// `usage` once the gateway has estimated it (`estimated == Some(true)`),
/// which is then emitted from [`usage`](Self::usage) in place (or appended
/// when the upstream sent none).
#[derive(Debug, Clone)]
pub struct SystemOneResponse {
    model: String,
    /// Upstream token counts, parsed leniently: a missing, null or malformed
    /// `usage` is `None` (the gateway then estimates, ADR 003), never an
    /// error that would turn a billed answer into a 502.
    pub usage: Option<SystemOneUsage>,
    entries: RawEntries,
}

impl SystemOneResponse {
    /// The versioned model that answered (e.g. `jev-1.13.0`), as reported
    /// upstream.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The raw `answers` map (one typed answer per question id).
    #[must_use]
    pub fn answers(&self) -> Option<&RawValue> {
        self.entries
            .iter()
            .find(|(k, _)| k == "answers")
            .map(|(_, v)| &**v)
    }
}

// ---- serde plumbing ---------------------------------------------------------

/// A JSON object deserialized as ordered `(key, raw value)` entries.
struct Entries(RawEntries);

impl<'de> Deserialize<'de> for Entries {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntriesVisitor;

        impl<'de> Visitor<'de> for EntriesVisitor {
            type Value = Entries;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object of named questions")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(4));
                while let Some(key) = map.next_key::<String>()? {
                    entries.push((key, map.next_value::<Box<RawValue>>()?));
                }
                Ok(Entries(entries))
            }
        }

        deserializer.deserialize_map(EntriesVisitor)
    }
}

impl<'de> Deserialize<'de> for SystemOneRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RequestVisitor;

        impl<'de> Visitor<'de> for RequestVisitor {
            type Value = SystemOneRequest;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a SystemOne request object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut model = None;
                let mut state = None;
                let mut questions = None;
                let mut extra: RawEntries = Vec::new();
                let mut extra_keys = HashSet::new();
                // One pass over the body; duplicate keys are rejected so
                // what is validated is exactly what is forwarded.
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "model" if model.is_some() => {
                            return Err(de::Error::duplicate_field("model"))
                        }
                        "state" if state.is_some() => {
                            return Err(de::Error::duplicate_field("state"))
                        }
                        "questions" if questions.is_some() => {
                            return Err(de::Error::duplicate_field("questions"))
                        }
                        "model" => model = Some(map.next_value::<String>()?),
                        "state" => state = Some(map.next_value::<Box<RawValue>>()?),
                        "questions" => questions = Some(map.next_value::<Entries>()?.0),
                        _ => {
                            let value = map.next_value::<Box<RawValue>>()?;
                            if !extra_keys.insert(key.clone()) {
                                return Err(de::Error::custom(format!("duplicate field `{key}`")));
                            }
                            extra.push((key, value));
                        }
                    }
                }
                Ok(SystemOneRequest {
                    model: model.ok_or_else(|| de::Error::missing_field("model"))?,
                    body: Arc::new(RequestBody {
                        state: state.ok_or_else(|| de::Error::missing_field("state"))?,
                        questions: questions
                            .ok_or_else(|| de::Error::missing_field("questions"))?,
                        extra,
                    }),
                })
            }
        }

        deserializer.deserialize_map(RequestVisitor)
    }
}

/// Serializes an ordered raw-entry list as a JSON object.
struct EntriesRef<'a>(&'a RawEntries);

impl Serialize for EntriesRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl Serialize for SystemOneRequest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let body = &self.body;
        let mut map = serializer.serialize_map(Some(3 + body.extra.len()))?;
        map.serialize_entry("model", &self.model)?;
        map.serialize_entry("state", &body.state)?;
        map.serialize_entry("questions", &EntriesRef(&body.questions))?;
        for (key, value) in &body.extra {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for SystemOneResponse {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let Entries(entries) = Entries::deserialize(deserializer)?;
        let mut model = None;
        let mut usage = None;
        let mut has_answers = false;
        for (key, value) in &entries {
            match key.as_str() {
                "model" => {
                    model = Some(
                        serde_json::from_str::<String>(value.get())
                            .map_err(|e| de::Error::custom(format!("invalid `model`: {e}")))?,
                    );
                }
                // Lenient on purpose (see the field doc).
                "usage" => usage = serde_json::from_str::<SystemOneUsage>(value.get()).ok(),
                "answers" => has_answers = true,
                _ => {}
            }
        }
        if !has_answers {
            return Err(de::Error::missing_field("answers"));
        }
        Ok(Self {
            model: model.ok_or_else(|| de::Error::missing_field("model"))?,
            usage,
            entries,
        })
    }
}

impl Serialize for SystemOneResponse {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let estimated = self.usage.filter(|u| u.estimated == Some(true));
        let has_raw_usage = self.entries.iter().any(|(k, _)| k == "usage");
        let append = self.usage.is_some() && !has_raw_usage;
        let len = self.entries.len() + usize::from(append);
        let mut map = serializer.serialize_map(Some(len))?;
        for (key, value) in &self.entries {
            match (key.as_str(), &estimated) {
                ("usage", Some(usage)) => map.serialize_entry(key, usage)?,
                _ => map.serialize_entry(key, value)?,
            }
        }
        if append {
            map.serialize_entry("usage", &self.usage)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(json: &str) -> SystemOneRequest {
        serde_json::from_str(json).expect("valid request")
    }

    fn invalid(json: &str) -> String {
        match request(json).validate() {
            Err(GatewayError::InvalidRequest(msg)) => msg,
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    const DOCS_EXAMPLE: &str = r#"{
        "state": {"zeta": "Help! My payouts have been failing for 3 days.", "alpha": 1},
        "model": "jev-latest",
        "questions": {
            "is_urgent": {"type": "noul", "instructions": "Does this convey urgency?",
                          "criteria": {"true": "Explicitly time-sensitive", "false": "No urgency"}},
            "department": {"type": "choice", "instructions": "Which team?",
                           "criteria": {"technical": "Bugs", "billing": "Payments", "sales": null}},
            "frustration": {"type": "score", "instructions": "How frustrated?",
                            "criteria": ["Calm", "Frustrated", "Very angry"]}
        },
        "future_flag": true
    }"#;

    #[test]
    fn documented_request_parses_and_validates() {
        let req = request(DOCS_EXAMPLE);
        assert_eq!(req.model, "jev-latest");
        let ids: Vec<&str> = req.questions().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(ids, ["is_urgent", "department", "frustration"]);
        assert!(req.validate().is_ok());
    }

    #[test]
    fn round_trip_preserves_key_order_and_unknown_fields() {
        let req = request(DOCS_EXAMPLE);
        let out = serde_json::to_string(&req).expect("serializes");
        // `state` keys stay in client order (zeta before alpha), not sorted.
        let zeta = out.find("\"zeta\"").expect("zeta present");
        let alpha = out.find("\"alpha\"").expect("alpha present");
        assert!(zeta < alpha, "state key order must be preserved: {out}");
        // Choice options stay in client order too.
        let technical = out.find("\"technical\"").expect("technical present");
        let billing = out.find("\"billing\"").expect("billing present");
        assert!(
            technical < billing,
            "criteria order must be preserved: {out}"
        );
        assert!(out.contains("\"future_flag\":true"));
        // And the result is still the same request.
        let back: SystemOneRequest = serde_json::from_str(&out).expect("reparses");
        assert_eq!(back.questions().len(), 3);
    }

    #[test]
    fn clones_share_the_body() {
        let req = request(DOCS_EXAMPLE);
        let mut attempt = req.clone();
        "jev-1.13.0".clone_into(&mut attempt.model);
        assert!(std::ptr::eq(req.state(), attempt.state()));
        assert_eq!(req.model, "jev-latest");
    }

    #[test]
    fn string_state_is_accepted() {
        let req = request(
            r#"{"model":"m","state":"text","questions":{"q":{"type":"noul","instructions":"?"}}}"#,
        );
        assert!(req.validate().is_ok());
        assert_eq!(req.state().get(), "\"text\"");
    }

    #[test]
    fn missing_required_fields_fail_to_parse() {
        for json in [
            r#"{"state":"s","questions":{}}"#,
            r#"{"model":"m","questions":{}}"#,
            r#"{"model":"m","state":"s"}"#,
            r#"{"model":5,"state":"s","questions":{}}"#,
            r#"["not","an","object"]"#,
        ] {
            assert!(
                serde_json::from_str::<SystemOneRequest>(json).is_err(),
                "{json}"
            );
        }
        let err =
            serde_json::from_str::<SystemOneRequest>(r#"{"model":"m","state":"s","questions":[]}"#)
                .expect_err("array questions rejected");
        assert!(
            err.to_string().contains("object of named questions"),
            "{err}"
        );
    }

    #[test]
    fn duplicate_top_level_keys_are_rejected() {
        for (json, field) in [
            (
                r#"{"model":"m","model":"n","state":"s","questions":{}}"#,
                "model",
            ),
            (
                r#"{"model":"m","state":"s","state":"t","questions":{}}"#,
                "state",
            ),
            (
                r#"{"model":"m","state":"s","questions":{},"questions":{}}"#,
                "questions",
            ),
            (
                r#"{"model":"m","state":"s","questions":{},"x":1,"x":2}"#,
                "x",
            ),
        ] {
            let err = serde_json::from_str::<SystemOneRequest>(json).expect_err(json);
            assert!(
                err.to_string()
                    .contains(&format!("duplicate field `{field}`")),
                "{err}"
            );
        }
    }

    #[test]
    fn empty_questions_is_lm_2011() {
        let err = request(r#"{"model":"m","state":"s","questions":{}}"#)
            .validate()
            .expect_err("empty");
        assert_eq!(err.code(), "LM-2011");
        assert_eq!(err.http_status(), 400);
    }

    #[test]
    fn empty_model_and_null_state_are_rejected() {
        let msg = invalid(
            r#"{"model":"","state":"s","questions":{"q":{"type":"noul","instructions":"?"}}}"#,
        );
        assert!(msg.contains("`model`"), "{msg}");
        let msg = invalid(
            r#"{"model":"m","state":null,"questions":{"q":{"type":"noul","instructions":"?"}}}"#,
        );
        assert!(msg.contains("`state`"), "{msg}");
    }

    #[test]
    fn duplicate_question_ids_are_rejected() {
        let msg = invalid(
            r#"{"model":"m","state":"s","questions":{
                "q":{"type":"noul","instructions":"?"},
                "q":{"type":"noul","instructions":"!"}}}"#,
        );
        assert!(msg.contains("duplicate question id `q`"), "{msg}");
    }

    #[test]
    fn malformed_questions_name_the_question() {
        let cases = [
            (r#""q":"not an object""#, "must be an object"),
            (r#""q":{"instructions":"?"}"#, "missing `type`"),
            (r#""q":{"type":5,"instructions":"?"}"#, "is malformed"),
            (
                r#""q":{"type":"noul","type":"noul","instructions":"?"}"#,
                "is malformed",
            ),
            (
                r#""q":{"type":"essay","instructions":"?"}"#,
                "unknown type 'essay'",
            ),
            (r#""q":{"type":"noul"}"#, "missing `instructions`"),
            (
                r#""q":{"type":"noul","instructions":null}"#,
                "missing `instructions`",
            ),
            (
                r#""q":{"type":"noul","instructions":"?","criteria":[1]}"#,
                "must be an object",
            ),
            (
                r#""q":{"type":"choice","instructions":"?"}"#,
                "requires `criteria`",
            ),
            (
                r#""q":{"type":"choice","instructions":"?","criteria":null}"#,
                "requires `criteria`",
            ),
            (
                r#""q":{"type":"choice","instructions":"?","criteria":["a"]}"#,
                "object of options",
            ),
            (
                r#""q":{"type":"choice","instructions":"?","criteria":{}}"#,
                "1 to 255 options",
            ),
            (
                r#""q":{"type":"score","instructions":"?"}"#,
                "requires `criteria`",
            ),
            (
                r#""q":{"type":"score","instructions":"?","criteria":{"a":1}}"#,
                "array of levels",
            ),
            (
                r#""q":{"type":"score","instructions":"?","criteria":"x"}"#,
                "array of levels",
            ),
            (
                r#""q":{"type":"score","instructions":"?","criteria":[]}"#,
                "1 to 10 levels",
            ),
        ];
        for (question, expected) in cases {
            let json = format!(r#"{{"model":"m","state":"s","questions":{{{question}}}}}"#);
            let msg = invalid(&json);
            assert!(msg.contains("`q`"), "{msg}");
            assert!(msg.contains(expected), "{question}: {msg}");
        }
    }

    #[test]
    fn option_and_level_limits_are_enforced() {
        let options: Vec<String> = (0..=MAX_CHOICE_OPTIONS)
            .map(|i| format!(r#""o{i}":null"#))
            .collect();
        let json = format!(
            r#"{{"model":"m","state":"s","questions":{{"q":{{"type":"choice","instructions":"?","criteria":{{{}}}}}}}}}"#,
            options.join(",")
        );
        assert!(invalid(&json).contains("got 256"));

        let levels = ["\"l\""; MAX_SCORE_LEVELS + 1].join(",");
        let json = format!(
            r#"{{"model":"m","state":"s","questions":{{"q":{{"type":"score","instructions":"?","criteria":[{levels}]}}}}}}"#
        );
        assert!(invalid(&json).contains("got 11"));

        let levels = ["\"l\""; MAX_SCORE_LEVELS].join(",");
        let json = format!(
            r#"{{"model":"m","state":"s","questions":{{"q":{{"type":"score","instructions":"?","criteria":[{levels}]}}}}}}"#
        );
        assert!(request(&json).validate().is_ok());
    }

    #[test]
    fn response_passes_through_verbatim() {
        let raw = r#"{"model":"jev-1.13.0","answers":{"department":{"type":"choice","choice":"billing","probabilities":{"sales":0.0,"billing":0.88},"confidence":0.81}},"usage":{"input_tokens":318,"output_tokens":34}}"#;
        let resp: SystemOneResponse = serde_json::from_str(raw).expect("valid response");
        assert_eq!(resp.model(), "jev-1.13.0");
        assert_eq!(
            resp.usage,
            Some(SystemOneUsage {
                input_tokens: 318,
                output_tokens: 34,
                estimated: None
            })
        );
        assert!(resp.answers().is_some());
        // Byte-identical when nothing was estimated.
        assert_eq!(serde_json::to_string(&resp).expect("serializes"), raw);
    }

    #[test]
    fn upstream_key_order_and_unknown_usage_fields_survive() {
        let raw = r#"{"usage":{"input_tokens":3,"output_tokens":1,"cached_tokens":2},"answers":{},"model":"jev","trace":"t-1"}"#;
        let resp: SystemOneResponse = serde_json::from_str(raw).expect("valid");
        assert_eq!(resp.usage.map(|u| u.input_tokens), Some(3));
        assert_eq!(serde_json::to_string(&resp).expect("serializes"), raw);
    }

    #[test]
    fn missing_or_malformed_usage_is_none_not_an_error() {
        for raw in [
            r#"{"model":"jev","answers":{}}"#,
            r#"{"model":"jev","answers":{},"usage":null}"#,
            r#"{"model":"jev","answers":{},"usage":{"input_tokens":null}}"#,
            r#"{"model":"jev","answers":{},"usage":{"input_tokens":318.0}}"#,
            r#"{"model":"jev","answers":{},"usage":{"input_tokens":-1}}"#,
        ] {
            let resp: SystemOneResponse = serde_json::from_str(raw).expect(raw);
            assert!(resp.usage.is_none(), "{raw}");
        }
    }

    #[test]
    fn response_without_model_or_answers_is_an_error() {
        for raw in [
            r#"{"answers":{}}"#,
            r#"{"model":"jev"}"#,
            r#"{"model":1,"answers":{}}"#,
        ] {
            assert!(
                serde_json::from_str::<SystemOneResponse>(raw).is_err(),
                "{raw}"
            );
        }
    }

    #[test]
    fn estimated_usage_replaces_or_appends_the_usage_field() {
        let estimate = SystemOneUsage {
            input_tokens: 12,
            output_tokens: 0,
            estimated: Some(true),
        };
        let mut resp: SystemOneResponse =
            serde_json::from_str(r#"{"model":"jev","answers":{}}"#).expect("valid");
        resp.usage = Some(estimate);
        let out = serde_json::to_string(&resp).expect("serializes");
        assert_eq!(
            out,
            r#"{"model":"jev","answers":{},"usage":{"input_tokens":12,"output_tokens":0,"estimated":true}}"#
        );

        let mut resp: SystemOneResponse =
            serde_json::from_str(r#"{"usage":null,"model":"jev","answers":{}}"#).expect("valid");
        resp.usage = Some(estimate);
        let out = serde_json::to_string(&resp).expect("serializes");
        assert_eq!(
            out,
            r#"{"usage":{"input_tokens":12,"output_tokens":0,"estimated":true},"model":"jev","answers":{}}"#
        );
    }
}
