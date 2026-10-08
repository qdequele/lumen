//! The TypeSafe edge format (`state`, `questions` object), validated
//! against the union of the TypeSafe and Perplexity contracts (spec 6.3).
//! `state` and every question body stay raw so a TypeSafe-family upstream
//! receives the client's bytes (D9).

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::value::RawValue;

use super::object_entries;
use crate::decisions::{
    Answer, ChoiceOption, ChoiceValue, DecisionRequest, DecisionResponse, DecisionUsage, Image,
    Input, Level, Part, PredicateCriteria, Question, QuestionKind, RawEntries, Text,
};
use crate::error::GatewayError;

/// Most questions in one TypeSafe-format request (Perplexity's cap, the
/// smallest of the family).
pub const MAX_QUESTIONS: usize = 128;
/// Most options of a `choice`.
pub const MAX_CHOICE_OPTIONS: usize = 255;
/// Most levels of a `score`.
pub const MAX_SCORE_LEVELS: usize = 10;

fn invalid(msg: impl Into<String>) -> GatewayError {
    GatewayError::InvalidRequest(msg.into())
}

/// Parse and validate TypeSafe-format top-level entries.
///
/// # Errors
/// `LM-2011` for an empty `questions` map, `LM-2004` for a remote image URL,
/// `LM-1001` for anything else (the message names the question).
pub fn parse(entries: RawEntries) -> Result<DecisionRequest, GatewayError> {
    let mut model = None;
    let mut state = None;
    let mut questions = None;
    let mut extra = Vec::new();
    for (key, value) in entries {
        match key.as_str() {
            "model" => model = Some(value),
            "state" => state = Some(value),
            "questions" => questions = Some(value),
            _ => extra.push((key, value)),
        }
    }
    let model: String = match model {
        None => return Err(invalid("missing field `model`")),
        Some(raw) => {
            serde_json::from_str(raw.get()).map_err(|_| invalid("`model` must be a string"))?
        }
    };
    if model.is_empty() {
        return Err(invalid("`model` must not be empty"));
    }
    let state = state.ok_or_else(|| invalid("missing field `state`"))?;
    if state.get() == "null" {
        return Err(invalid("`state` must not be null"));
    }
    let questions = questions.ok_or_else(|| invalid("missing field `questions`"))?;
    let questions = object_entries(questions.get().as_bytes(), "`questions`").map_err(invalid)?;
    if questions.is_empty() {
        return Err(GatewayError::EmptyQuestions);
    }
    if questions.len() > MAX_QUESTIONS {
        return Err(invalid(format!(
            "`questions` holds {} questions; at most {MAX_QUESTIONS} are allowed",
            questions.len()
        )));
    }
    let questions = questions
        .into_iter()
        .map(|(id, body)| parse_question(id, body))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DecisionRequest::new(model, parse_state(state)?, questions).with_extra(extra))
}

/// A `state` array carrying `image_url` parts becomes [`Input::Messages`];
/// any other state stays raw.
fn parse_state(state: Box<RawValue>) -> Result<Input, GatewayError> {
    if !state.get().trim_start().starts_with('[') {
        return Ok(Input::Structured(state));
    }
    let elements: Vec<Box<RawValue>> = serde_json::from_str(state.get())
        .map_err(|e| invalid(format!("malformed `state`: {e}")))?;
    let mut parts = Vec::with_capacity(elements.len());
    let mut images = 0;
    for element in elements {
        match image_part(&element)? {
            Some(image) => {
                images += 1;
                parts.push(Part::Image(image));
            }
            None => parts.push(Part::Text(Text::Json(element))),
        }
    }
    if images == 0 {
        return Ok(Input::Structured(state));
    }
    Ok(Input::Messages(parts))
}

/// Perplexity's image part: `{"type": "image_url", "image_url": {"url": "data:..."}}`.
fn image_part(element: &RawValue) -> Result<Option<Image>, GatewayError> {
    #[derive(Deserialize)]
    struct Url {
        url: String,
    }
    #[derive(Deserialize)]
    struct ImagePart {
        #[serde(rename = "type")]
        kind: Option<String>,
        image_url: Option<Url>,
    }
    if !element.get().trim_start().starts_with('{') {
        return Ok(None);
    }
    let Ok(part) = serde_json::from_str::<ImagePart>(element.get()) else {
        return Ok(None);
    };
    if part.kind.as_deref() != Some("image_url") {
        return Ok(None);
    }
    let url = part
        .image_url
        .ok_or_else(|| invalid("an `image_url` state part needs `image_url.url`"))?
        .url;
    if !url.starts_with("data:") {
        return Err(GatewayError::ImageUrlNotSupported {
            provider: "/v1/decisions".to_owned(),
        });
    }
    Ok(Some(Image {
        data_url: url,
        detail: None,
    }))
}

/// The fields of a question the gateway reads; everything else is skipped.
#[derive(Deserialize)]
struct QuestionView {
    #[serde(rename = "type")]
    kind: Option<String>,
    instructions: Option<Box<RawValue>>,
    criteria: Option<Box<RawValue>>,
}

fn parse_question(id: String, body: Box<RawValue>) -> Result<Question, GatewayError> {
    let fail = |m: String| Err(invalid(format!("question `{id}` {m}")));
    let view: QuestionView = match serde_json::from_str(body.get()) {
        Ok(v) => v,
        Err(e) if body.get().trim_start().starts_with('{') => {
            return fail(format!("is malformed: {e}"))
        }
        Err(_) => return fail("must be an object".to_owned()),
    };
    let Some(kind) = view.kind else {
        return fail("is missing `type`".to_owned());
    };
    let instructions = view.instructions.map(Text::from_raw);
    let criteria = view.criteria.filter(|c| c.get() != "null");
    let kind = match kind.as_str() {
        "noul" => {
            let criteria = match criteria {
                None => None,
                Some(raw) => match predicate_criteria(&raw) {
                    Some(c) => Some(c),
                    None => return fail("noul `criteria` must be an object".to_owned()),
                },
            };
            if instructions.is_none() && criteria.is_none() {
                return fail("needs instructions or criteria".to_owned());
            }
            QuestionKind::Predicate { criteria }
        }
        "choice" => {
            if instructions.is_none() {
                return fail("is missing `instructions`".to_owned());
            }
            let Some(raw) = criteria else {
                return fail("choice requires `criteria`".to_owned());
            };
            let Ok(options) = object_entries(raw.get().as_bytes(), "criteria") else {
                return fail("choice `criteria` must be an object of options".to_owned());
            };
            if options.is_empty() || options.len() > MAX_CHOICE_OPTIONS {
                return fail(format!(
                    "choice needs 1 to {MAX_CHOICE_OPTIONS} options, got {}",
                    options.len()
                ));
            }
            QuestionKind::Choice {
                choices: options
                    .into_iter()
                    .map(|(value, description)| ChoiceOption {
                        value: ChoiceValue::Str(value),
                        description: (description.get() != "null")
                            .then(|| Text::from_raw(description)),
                    })
                    .collect(),
            }
        }
        "score" => {
            if instructions.is_none() {
                return fail("is missing `instructions`".to_owned());
            }
            let Some(raw) = criteria else {
                return fail("score requires `criteria`".to_owned());
            };
            let Ok(levels) = serde_json::from_str::<Vec<Box<RawValue>>>(raw.get()) else {
                return fail("score `criteria` must be an array of levels".to_owned());
            };
            if levels.is_empty() || levels.len() > MAX_SCORE_LEVELS {
                return fail(format!(
                    "score needs 1 to {MAX_SCORE_LEVELS} levels, got {}",
                    levels.len()
                ));
            }
            QuestionKind::Score {
                levels: levels
                    .into_iter()
                    .map(|l| Level {
                        label: Text::from_raw(l),
                        description: None,
                    })
                    .collect(),
            }
        }
        other => {
            return fail(format!(
                "has unknown type '{other}': expected noul, choice or score"
            ));
        }
    };
    Ok(Question {
        name: Some(id),
        instructions,
        kind,
        raw: Some(body),
    })
}

/// `criteria.true` / `criteria.false` of a noul; `None` if not an object.
fn predicate_criteria(raw: &RawValue) -> Option<PredicateCriteria> {
    let entries = object_entries(raw.get().as_bytes(), "criteria").ok()?;
    let mut criteria = PredicateCriteria {
        when_true: None,
        when_false: None,
    };
    for (key, value) in entries {
        match key.as_str() {
            "true" => criteria.when_true = Some(Text::from_raw(value)),
            "false" => criteria.when_false = Some(Text::from_raw(value)),
            _ => {}
        }
    }
    Some(criteria)
}

/// Render a response in the TypeSafe format. A TypeSafe-family upstream's
/// object is re-emitted verbatim (D9), only `usage` replaced when the gateway
/// estimated it (or appended when absent); otherwise every answer is
/// synthesized under its question name.
///
/// # Errors
/// [`GatewayError::Internal`] if serialization fails.
pub fn render(resp: &DecisionResponse, req: &DecisionRequest) -> Result<Vec<u8>, GatewayError> {
    let out = match &resp.upstream {
        Some(entries) => serde_json::to_vec(&Verbatim {
            entries,
            usage: resp.usage,
        }),
        None => serde_json::to_vec(&Synthesized { resp, req }),
    };
    out.map_err(|e| GatewayError::Internal(format!("decisions render: {e}")))
}

/// TypeSafe's `usage`: `{input_tokens, output_tokens}` plus `estimated`.
struct UsageOut(DecisionUsage);

impl Serialize for UsageOut {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let estimated = self.0.estimated == Some(true);
        let mut map = s.serialize_map(Some(2 + usize::from(estimated)))?;
        map.serialize_entry("input_tokens", &self.0.input_tokens)?;
        map.serialize_entry("output_tokens", &self.0.output_tokens)?;
        if estimated {
            map.serialize_entry("estimated", &true)?;
        }
        map.end()
    }
}

struct Verbatim<'a> {
    entries: &'a RawEntries,
    usage: Option<DecisionUsage>,
}

impl Serialize for Verbatim<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let estimated = self.usage.filter(|u| u.estimated == Some(true));
        let has_usage = self.entries.iter().any(|(k, _)| k == "usage");
        let append = self.usage.is_some() && !has_usage;
        let mut map = s.serialize_map(Some(self.entries.len() + usize::from(append)))?;
        for (key, value) in self.entries {
            match (key.as_str(), estimated) {
                ("usage", Some(u)) => map.serialize_entry(key, &UsageOut(u))?,
                _ => map.serialize_entry(key, value)?,
            }
        }
        if let (true, Some(u)) = (append, self.usage) {
            map.serialize_entry("usage", &UsageOut(u))?;
        }
        map.end()
    }
}

struct Synthesized<'a> {
    resp: &'a DecisionResponse,
    req: &'a DecisionRequest,
}

impl Serialize for Synthesized<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(2 + usize::from(self.resp.usage.is_some())))?;
        map.serialize_entry("model", &self.resp.model)?;
        map.serialize_entry("answers", &AnswerMap(self.resp, self.req))?;
        if let Some(u) = self.resp.usage {
            map.serialize_entry("usage", &UsageOut(u))?;
        }
        map.end()
    }
}

struct AnswerMap<'a>(&'a DecisionResponse, &'a DecisionRequest);

impl Serialize for AnswerMap<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let questions = self.1.questions();
        let mut map = s.serialize_map(Some(self.0.answers.len()))?;
        for (i, (answer, q)) in self.0.answers.iter().zip(questions).enumerate() {
            let id = q.name.clone().unwrap_or_else(|| format!("q{i}"));
            map.serialize_entry(&id, &TsAnswer(answer, q))?;
        }
        map.end()
    }
}

struct TsAnswer<'a>(&'a Answer, &'a Question);

impl Serialize for TsAnswer<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Answer::Predicate { probability } => {
                let mut map = s.serialize_map(Some(2))?;
                map.serialize_entry("type", "noul")?;
                map.serialize_entry("noul", probability)?;
                map.end()
            }
            Answer::Choice {
                choice,
                probabilities,
                confidence,
            } => {
                let mut map = s.serialize_map(None)?;
                map.serialize_entry("type", "choice")?;
                map.serialize_entry("choice", &ChoiceOut(choice))?;
                map.serialize_entry("probabilities", &ChoiceProbs(probabilities))?;
                if let Some(c) = confidence {
                    map.serialize_entry("confidence", c)?;
                }
                map.end()
            }
            Answer::Score {
                score,
                probabilities,
                confidence,
            } => {
                let mut map = s.serialize_map(None)?;
                map.serialize_entry("type", "score")?;
                map.serialize_entry("score", score)?;
                if let QuestionKind::Score { levels } = &self.1.kind {
                    map.serialize_entry("legend", &Legend(levels))?;
                }
                map.serialize_entry("probabilities", &ScoreProbs(probabilities))?;
                if let Some(c) = confidence {
                    map.serialize_entry("confidence", c)?;
                }
                map.end()
            }
            Answer::Refusal => {
                let mut map = s.serialize_map(Some(1))?;
                map.serialize_entry("type", "refusal")?;
                map.end()
            }
        }
    }
}

/// A choice value as TypeSafe JSON: strings as strings, booleans as booleans.
struct ChoiceOut<'a>(&'a ChoiceValue);

impl Serialize for ChoiceOut<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            ChoiceValue::Str(v) => s.serialize_str(v),
            ChoiceValue::Bool(b) => s.serialize_bool(*b),
        }
    }
}

struct ChoiceProbs<'a>(&'a [(ChoiceValue, f64)]);

impl Serialize for ChoiceProbs<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for (value, p) in self.0 {
            map.serialize_entry(&value.wire_key(), p)?;
        }
        map.end()
    }
}

struct ScoreProbs<'a>(&'a [(usize, f64)]);

impl Serialize for ScoreProbs<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for (i, p) in self.0 {
            map.serialize_entry(&i.to_string(), p)?;
        }
        map.end()
    }
}

/// `{"0": "<label>", ...}` from the request's levels.
struct Legend<'a>(&'a [Level]);

impl Serialize for Legend<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for (i, level) in self.0.iter().enumerate() {
            map.serialize_entry(&i.to_string(), &level.label.as_prompt())?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ts(json: &str) -> DecisionRequest {
        parse(object_entries(json.as_bytes(), "request body").unwrap()).unwrap()
    }

    #[test]
    fn string_state_with_escapes_is_kept_verbatim() {
        let req = parse_ts(
            r#"{"model":"jev","state":"café\n","questions":{"q":{"type":"noul","instructions":"i"}}}"#,
        );
        match req.input() {
            Input::Structured(raw) => assert_eq!(raw.get(), r#""café\n""#),
            _ => panic!("a TypeSafe state is always Structured"),
        }
    }

    #[test]
    fn question_bodies_are_kept_raw_and_typed() {
        let req = parse_ts(
            r#"{"model":"jev","state":{"b":1,"a":2},"questions":{
            "u":{"type":"noul","instructions":{"q":"urgent?"},"criteria":{"true":"y"}},
            "d":{"type":"choice","instructions":"team","criteria":{"tech":"Bugs","billing":null}},
            "f":{"type":"score","instructions":"mood","criteria":["calm",{"x":1}]}}}"#,
        );
        let q = req.questions();
        assert_eq!(q.len(), 3);
        assert_eq!(q[0].name.as_deref(), Some("u"));
        assert!(q[0]
            .raw
            .as_ref()
            .unwrap()
            .get()
            .contains(r#""criteria":{"true":"y"}"#));
        assert!(matches!(q[0].instructions, Some(Text::Json(_))));
        match &q[1].kind {
            QuestionKind::Choice { choices } => {
                assert_eq!(choices.len(), 2);
                assert!(choices[0].value == ChoiceValue::Str("tech".into()));
                assert!(choices[1].description.is_none());
            }
            _ => panic!(),
        }
        match &q[2].kind {
            QuestionKind::Score { levels } => assert!(matches!(levels[1].label, Text::Json(_))),
            _ => panic!(),
        }
    }

    #[test]
    fn criteria_only_noul_is_accepted_and_neither_is_rejected() {
        let ok = parse_ts(
            r#"{"model":"p","state":"s","questions":{"q":{"type":"noul","criteria":{"true":"y"}}}}"#,
        );
        assert!(ok.questions()[0].instructions.is_none());
        let entries = object_entries(
            br#"{"model":"p","state":"s","questions":{"q":{"type":"noul"}}}"#,
            "request body",
        )
        .unwrap();
        let m = match parse(entries) {
            Err(GatewayError::InvalidRequest(m)) => m,
            other => panic!("{other:?}"),
        };
        assert!(
            m.contains("`q`") && m.contains("instructions or criteria"),
            "{m}"
        );
    }

    #[test]
    fn more_than_128_questions_is_lm_1001() {
        let qs: Vec<String> = (0..129)
            .map(|i| format!(r#""q{i}":{{"type":"noul","instructions":"i"}}"#))
            .collect();
        let body = format!(
            r#"{{"model":"p","state":"s","questions":{{{}}}}}"#,
            qs.join(",")
        );
        let entries = object_entries(body.as_bytes(), "request body").unwrap();
        assert!(
            matches!(parse(entries), Err(GatewayError::InvalidRequest(m)) if m.contains("128"))
        );
    }

    #[test]
    fn image_url_parts_in_a_state_array_build_messages() {
        let req = parse_ts(
            r#"{"model":"p","state":["look at this",{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}},{"k":1}],
            "questions":{"q":{"type":"noul","instructions":"i"}}}"#,
        );
        match req.input() {
            Input::Messages(parts) => {
                assert_eq!(parts.len(), 3);
                assert!(
                    matches!(&parts[0], Part::Text(Text::Json(raw)) if raw.get() == r#""look at this""#)
                );
                assert!(
                    matches!(&parts[1], Part::Image(img) if img.data_url.starts_with("data:image/png"))
                );
                assert!(matches!(&parts[2], Part::Text(Text::Json(_))));
            }
            _ => panic!("images build Messages"),
        }
    }

    #[test]
    fn a_remote_image_url_is_lm_2004() {
        let entries = object_entries(
            br#"{"model":"p","state":[{"type":"image_url","image_url":{"url":"https://x/y.png"}}],
            "questions":{"q":{"type":"noul","instructions":"i"}}}"#,
            "request body",
        )
        .unwrap();
        assert!(matches!(
            parse(entries),
            Err(GatewayError::ImageUrlNotSupported { .. })
        ));
    }

    #[test]
    fn unknown_top_level_fields_are_kept_in_order() {
        let req = parse_ts(
            r#"{"model":"jev","state":"s","questions":{"q":{"type":"noul","instructions":"i"}},"z":1,"a":true}"#,
        );
        let keys: Vec<&str> = req.extra().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["z", "a"]);
    }

    fn usage(input: u32, estimated: Option<bool>) -> DecisionUsage {
        DecisionUsage {
            input_tokens: input,
            output_tokens: 3,
            estimated,
            ..DecisionUsage::default()
        }
    }

    #[test]
    fn upstream_entries_render_verbatim_with_estimated_usage_replaced() {
        let req = parse_ts(
            r#"{"model":"jev","state":"s","questions":{"a":{"type":"noul","instructions":"i"}}}"#,
        );
        let upstream = object_entries(
            br#"{"model":"jev-1.13.0","answers":{"a":{"type":"noul","noul":0.9,"extra":1}},"request_id":"r"}"#,
            "upstream").unwrap();
        let resp = DecisionResponse {
            model: "jev-1.13.0".into(),
            answers: vec![Answer::Predicate { probability: 0.9 }],
            usage: Some(usage(7, Some(true))),
            upstream: Some(upstream),
        };
        let out = String::from_utf8(render(&resp, &req).unwrap()).unwrap();
        assert_eq!(
            out,
            r#"{"model":"jev-1.13.0","answers":{"a":{"type":"noul","noul":0.9,"extra":1}},"request_id":"r","usage":{"input_tokens":7,"output_tokens":3,"estimated":true}}"#
        );
    }

    #[test]
    fn synthesized_answers_carry_a_legend_and_refusals() {
        let req = parse_ts(
            r#"{"model":"jev","state":"s","questions":{
            "s":{"type":"score","instructions":"i","criteria":["low","high"]},
            "c":{"type":"choice","instructions":"i","criteria":{"x":null,"y":null}},
            "r":{"type":"noul","instructions":"i"}}}"#,
        );
        let resp = DecisionResponse {
            model: "gpt-6-luna".into(),
            answers: vec![
                Answer::Score {
                    score: 0.8,
                    probabilities: vec![(0, 0.2), (1, 0.8)],
                    confidence: Some(0.6),
                },
                Answer::Choice {
                    choice: ChoiceValue::Str("y".into()),
                    probabilities: vec![
                        (ChoiceValue::Str("x".into()), 0.3),
                        (ChoiceValue::Str("y".into()), 0.7),
                    ],
                    confidence: None,
                },
                Answer::Refusal,
            ],
            usage: Some(usage(5, None)),
            upstream: None,
        };
        let out = String::from_utf8(render(&resp, &req).unwrap()).unwrap();
        assert_eq!(
            out,
            concat!(
                r#"{"model":"gpt-6-luna","answers":{"#,
                r#""s":{"type":"score","score":0.8,"legend":{"0":"low","1":"high"},"probabilities":{"0":0.2,"1":0.8},"confidence":0.6},"#,
                r#""c":{"type":"choice","choice":"y","probabilities":{"x":0.3,"y":0.7}},"#,
                r#""r":{"type":"refusal"}},"#,
                r#""usage":{"input_tokens":5,"output_tokens":3}}"#
            )
        );
    }

    fn invalid_msg(json: &str) -> String {
        let entries = object_entries(json.as_bytes(), "request body").unwrap();
        match parse(entries) {
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
        let req = parse_ts(DOCS_EXAMPLE);
        assert_eq!(req.model, "jev-latest");
        let ids: Vec<&str> = req
            .questions()
            .iter()
            .filter_map(|q| q.name.as_deref())
            .collect();
        assert_eq!(ids, ["is_urgent", "department", "frustration"]);
        match req.input() {
            Input::Structured(raw) => {
                let s = raw.get();
                assert!(s.find("zeta").unwrap() < s.find("alpha").unwrap());
            }
            _ => panic!("object state is Structured"),
        }
        assert_eq!(req.extra().len(), 1);
    }

    #[test]
    fn missing_required_fields_fail_to_parse() {
        for json in [
            r#"{"state":"s","questions":{}}"#,
            r#"{"model":"m","questions":{}}"#,
            r#"{"model":"m","state":"s"}"#,
            r#"{"model":5,"state":"s","questions":{}}"#,
            r#"{"model":"m","state":"s","questions":[]}"#,
        ] {
            let entries = object_entries(json.as_bytes(), "request body").unwrap();
            assert!(
                matches!(parse(entries), Err(GatewayError::InvalidRequest(_))),
                "{json}"
            );
        }
        assert!(object_entries(br#"["not","an","object"]"#, "request body").is_err());
        assert!(invalid_msg(r#"{"model":"m","state":"s","questions":[]}"#).contains("object"));
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
            let err = object_entries(json.as_bytes(), "request body").expect_err(json);
            assert!(err.contains(&format!("duplicate field `{field}`")), "{err}");
        }
    }

    #[test]
    fn empty_questions_is_lm_2011() {
        let entries = object_entries(
            br#"{"model":"m","state":"s","questions":{}}"#,
            "request body",
        )
        .unwrap();
        let err = parse(entries).expect_err("empty");
        assert_eq!(err.code(), "LM-2011");
        assert_eq!(err.http_status(), 400);
    }

    #[test]
    fn empty_model_and_null_state_are_rejected() {
        let msg = invalid_msg(
            r#"{"model":"","state":"s","questions":{"q":{"type":"noul","instructions":"?"}}}"#,
        );
        assert!(msg.contains("`model`"), "{msg}");
        let msg = invalid_msg(
            r#"{"model":"m","state":null,"questions":{"q":{"type":"noul","instructions":"?"}}}"#,
        );
        assert!(msg.contains("`state`"), "{msg}");
    }

    #[test]
    fn duplicate_question_ids_are_rejected() {
        let msg = invalid_msg(
            r#"{"model":"m","state":"s","questions":{
                "q":{"type":"noul","instructions":"?"},
                "q":{"type":"noul","instructions":"!"}}}"#,
        );
        assert!(msg.contains("duplicate") && msg.contains("`q`"), "{msg}");
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
            (r#""q":{"type":"noul"}"#, "instructions or criteria"),
            (
                r#""q":{"type":"noul","instructions":null}"#,
                "instructions or criteria",
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
            let msg = invalid_msg(&json);
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
        assert!(invalid_msg(&json).contains("got 256"));

        let levels = ["\"l\""; MAX_SCORE_LEVELS + 1].join(",");
        let json = format!(
            r#"{{"model":"m","state":"s","questions":{{"q":{{"type":"score","instructions":"?","criteria":[{levels}]}}}}}}"#
        );
        assert!(invalid_msg(&json).contains("got 11"));

        let levels = ["\"l\""; MAX_SCORE_LEVELS].join(",");
        let json = format!(
            r#"{{"model":"m","state":"s","questions":{{"q":{{"type":"score","instructions":"?","criteria":[{levels}]}}}}}}"#
        );
        parse_ts(&json);
    }

    #[test]
    fn estimated_usage_appends_or_replaces_the_usage_field() {
        let req = parse_ts(
            r#"{"model":"jev","state":"s","questions":{"a":{"type":"noul","instructions":"i"}}}"#,
        );
        let mk = |raw: &[u8]| DecisionResponse {
            model: "jev".into(),
            answers: vec![],
            usage: Some(DecisionUsage {
                input_tokens: 12,
                output_tokens: 0,
                estimated: Some(true),
                ..DecisionUsage::default()
            }),
            upstream: Some(object_entries(raw, "upstream").unwrap()),
        };
        let out = String::from_utf8(render(&mk(br#"{"model":"jev","answers":{}}"#), &req).unwrap())
            .unwrap();
        assert_eq!(
            out,
            r#"{"model":"jev","answers":{},"usage":{"input_tokens":12,"output_tokens":0,"estimated":true}}"#
        );
        let out = String::from_utf8(
            render(&mk(br#"{"usage":null,"model":"jev","answers":{}}"#), &req).unwrap(),
        )
        .unwrap();
        assert_eq!(
            out,
            r#"{"usage":{"input_tokens":12,"output_tokens":0,"estimated":true},"model":"jev","answers":{}}"#
        );
    }

    #[test]
    fn response_passes_through_verbatim() {
        let req = parse_ts(
            r#"{"model":"jev","state":"s","questions":{"department":{"type":"noul","instructions":"i"}}}"#,
        );
        let raw = r#"{"model":"jev-1.13.0","answers":{"department":{"type":"choice","choice":"billing","probabilities":{"sales":0.0,"billing":0.88},"confidence":0.81}},"usage":{"input_tokens":318,"output_tokens":34}}"#;
        let resp = DecisionResponse {
            model: "jev-1.13.0".into(),
            answers: vec![],
            usage: Some(usage(318, None)).map(|u| DecisionUsage {
                output_tokens: 34,
                ..u
            }),
            upstream: Some(object_entries(raw.as_bytes(), "upstream").unwrap()),
        };
        assert_eq!(
            String::from_utf8(render(&resp, &req).unwrap()).unwrap(),
            raw
        );
    }

    #[test]
    fn upstream_key_order_and_unknown_usage_fields_survive() {
        let req = parse_ts(
            r#"{"model":"jev","state":"s","questions":{"a":{"type":"noul","instructions":"i"}}}"#,
        );
        let raw = r#"{"usage":{"input_tokens":3,"output_tokens":1,"cached_tokens":2},"answers":{},"model":"jev","trace":"t-1"}"#;
        let resp = DecisionResponse {
            model: "jev".into(),
            answers: vec![],
            usage: Some(usage(3, None)),
            upstream: Some(object_entries(raw.as_bytes(), "upstream").unwrap()),
        };
        assert_eq!(
            String::from_utf8(render(&resp, &req).unwrap()).unwrap(),
            raw
        );
    }
}
