//! The OpenAI edge format (`POST /v1/decisions`, `client.decisions.create`),
//! validated against OpenAI's documented contract whatever the target
//! (spec 6.2). Unknown top-level fields are `LM-1001`, as OpenAI answers an
//! unknown parameter. Rendering follows the OpenAI SDK types (`confidence`
//! and the `usage` detail objects are required there).

use std::collections::HashSet;

use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Serialize, Serializer};

use crate::decisions::{
    Answer, ChoiceOption, ChoiceValue, DecisionRequest, DecisionResponse, DecisionUsage, Image,
    Input, Level, Part, Question, QuestionKind, RawEntries, Text,
};
use crate::error::GatewayError;

/// Most image parts across all messages of one request.
pub const MAX_IMAGES: usize = 128;
/// Most characters of `safety_identifier`.
pub const MAX_SAFETY_IDENTIFIER: usize = 128;
/// Most options of a `choice`.
pub const MAX_CHOICE_OPTIONS: usize = 255;

fn invalid(msg: impl Into<String>) -> GatewayError {
    GatewayError::InvalidRequest(msg.into())
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WireInput {
    Text(String),
    Messages(Vec<WireMessage>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMessage {
    role: String,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    content: WireContent,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WireContent {
    Text(String),
    Parts(Vec<WirePart>),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WirePart {
    InputText {
        text: String,
    },
    InputImage {
        image_url: String,
        #[serde(default)]
        detail: Option<String>,
    },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WireValue {
    Bool(bool),
    Str(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireChoice {
    value: WireValue,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireLevel {
    label: String,
    #[serde(default)]
    description: Option<String>,
}

/// One question, read loosely so errors can name it; `type`-specific
/// fields are checked after.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireQuestion {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    choices: Option<Vec<WireChoice>>,
    #[serde(default)]
    levels: Option<Vec<WireLevel>>,
}

/// Parse and validate OpenAI-format top-level entries.
///
/// # Errors
/// `LM-2011` for empty `questions`, `LM-2004` for a non-`data:` image URL,
/// `LM-1001` for anything else.
pub fn parse(entries: RawEntries) -> Result<DecisionRequest, GatewayError> {
    let mut model = None;
    let mut input = None;
    let mut questions = None;
    let mut safety = None;
    for (key, value) in entries {
        match key.as_str() {
            "model" => model = Some(value),
            "input" => input = Some(value),
            "questions" => questions = Some(value),
            "safety_identifier" => safety = Some(value),
            other => return Err(invalid(format!("unknown parameter `{other}`"))),
        }
    }
    let model: String = model
        .and_then(|raw| serde_json::from_str(raw.get()).ok())
        .ok_or_else(|| invalid("missing or non-string `model`"))?;
    if model.is_empty() {
        return Err(invalid("`model` must not be empty"));
    }
    let input = input.ok_or_else(|| invalid("missing field `input`"))?;
    let input: WireInput = serde_json::from_str(input.get())
        .map_err(|_| invalid("`input` must be a string or an array of user messages"))?;
    let input = convert_input(input)?;
    let questions = questions.ok_or_else(|| invalid("missing field `questions`"))?;
    let questions: Vec<Box<serde_json::value::RawValue>> = serde_json::from_str(questions.get())
        .map_err(|_| invalid("`questions` must be an array"))?;
    if questions.is_empty() {
        return Err(GatewayError::EmptyQuestions);
    }
    let mut names = HashSet::new();
    let questions = questions
        .iter()
        .enumerate()
        .map(|(i, raw)| {
            let q = convert_question(i, raw)?;
            if let Some(name) = &q.name {
                if !names.insert(name.clone()) {
                    return Err(invalid(format!("duplicate question name `{name}`")));
                }
            }
            Ok(q)
        })
        .collect::<Result<Vec<_>, GatewayError>>()?;
    let safety: Option<String> = match safety {
        None => None,
        Some(raw) if raw.get() == "null" => None,
        Some(raw) => Some(
            serde_json::from_str(raw.get())
                .map_err(|_| invalid("`safety_identifier` must be a string"))?,
        ),
    };
    if safety
        .as_ref()
        .is_some_and(|s| s.chars().count() > MAX_SAFETY_IDENTIFIER)
    {
        return Err(invalid(format!(
            "`safety_identifier` must be at most {MAX_SAFETY_IDENTIFIER} characters"
        )));
    }
    Ok(DecisionRequest::new(model, input, questions).with_safety_identifier(safety))
}

fn convert_input(input: WireInput) -> Result<Input, GatewayError> {
    let messages = match input {
        WireInput::Text(s) => return Ok(Input::Text(s)),
        WireInput::Messages(m) => m,
    };
    let mut parts = Vec::new();
    let mut images = 0usize;
    for message in messages {
        if message.role != "user" {
            return Err(invalid(format!(
                "`input` messages must have role \"user\", got \"{}\"",
                message.role
            )));
        }
        if message.kind.as_deref().is_some_and(|k| k != "message") {
            return Err(invalid("`input` message `type` must be \"message\""));
        }
        match message.content {
            WireContent::Text(s) => parts.push(Part::Text(Text::Plain(s))),
            WireContent::Parts(list) => {
                for part in list {
                    match part {
                        WirePart::InputText { text } => parts.push(Part::Text(Text::Plain(text))),
                        WirePart::InputImage { image_url, detail } => {
                            if !image_url.starts_with("data:") {
                                return Err(GatewayError::ImageUrlNotSupported {
                                    provider: "/v1/decisions".to_owned(),
                                });
                            }
                            images += 1;
                            parts.push(Part::Image(Image {
                                data_url: image_url,
                                detail,
                            }));
                        }
                    }
                }
            }
        }
    }
    if images > MAX_IMAGES {
        return Err(invalid(format!(
            "`input` holds {images} images; at most {MAX_IMAGES} are allowed"
        )));
    }
    Ok(Input::Messages(parts))
}

fn convert_question(i: usize, raw: &serde_json::value::RawValue) -> Result<Question, GatewayError> {
    let wq: WireQuestion = serde_json::from_str(raw.get()).map_err(|_| {
        // Fixed text: a serde error could quote the client's values.
        invalid(format!(
            "question #{i} is malformed: expected an object with string `type` and \
             `instructions` and only the fields of its type"
        ))
    })?;
    let label = wq
        .name
        .as_ref()
        .map_or_else(|| format!("#{i}"), |n| format!("`{n}`"));
    let fail = |m: &str| invalid(format!("question {label} {m}"));
    let instructions = wq
        .instructions
        .ok_or_else(|| fail("is missing string `instructions`"))?;
    let type_name = wq.kind.as_deref().unwrap_or_default();
    for (field, present, allowed) in [
        ("choices", wq.choices.is_some(), "choice"),
        ("levels", wq.levels.is_some(), "score"),
    ] {
        if present && type_name != allowed && matches!(type_name, "predicate" | "choice" | "score")
        {
            return Err(fail(&format!(
                "has `{field}`, which is not valid for type '{type_name}'"
            )));
        }
    }
    let kind = match wq.kind.as_deref() {
        Some("predicate") => QuestionKind::Predicate { criteria: None },
        Some("choice") => {
            let choices = wq.choices.ok_or_else(|| fail("needs `choices`"))?;
            if choices.len() < 2 || choices.len() > MAX_CHOICE_OPTIONS {
                return Err(fail(&format!(
                    "needs 2 to {MAX_CHOICE_OPTIONS} choices, got {}",
                    choices.len()
                )));
            }
            let choices: Vec<ChoiceOption> = choices
                .into_iter()
                .map(|c| ChoiceOption {
                    value: match c.value {
                        WireValue::Bool(b) => ChoiceValue::Bool(b),
                        WireValue::Str(s) => ChoiceValue::Str(s),
                    },
                    description: c.description.map(Text::Plain),
                })
                .collect();
            for (a, x) in choices.iter().enumerate() {
                if choices[..a].iter().any(|y| y.value == x.value) {
                    return Err(fail("needs unique choice values"));
                }
            }
            QuestionKind::Choice { choices }
        }
        Some("score") => {
            let levels = wq.levels.ok_or_else(|| fail("needs `levels`"))?;
            if levels.is_empty() {
                return Err(fail("needs at least 1 level"));
            }
            QuestionKind::Score {
                levels: levels
                    .into_iter()
                    .map(|l| Level {
                        label: Text::Plain(l.label),
                        description: l.description.map(Text::Plain),
                    })
                    .collect(),
            }
        }
        Some(_) => {
            return Err(fail(
                "has an unknown `type`: expected predicate, choice or score",
            ))
        }
        None => return Err(fail("is missing `type`")),
    };
    Ok(Question {
        name: wq.name,
        instructions: Some(Text::Plain(instructions)),
        kind,
        raw: None,
    })
}

/// Render a response in OpenAI's format; `usage` is completed to OpenAI's
/// schema so typed SDKs deserialize it (spec 6.2).
///
/// # Errors
/// [`GatewayError::Internal`] if serialization fails.
pub fn render(resp: &DecisionResponse, req: &DecisionRequest) -> Result<Vec<u8>, GatewayError> {
    serde_json::to_vec(&Out { resp, req })
        .map_err(|e| GatewayError::Internal(format!("decisions render: {e}")))
}

struct Out<'a> {
    resp: &'a DecisionResponse,
    req: &'a DecisionRequest,
}

impl Serialize for Out<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(3))?;
        map.serialize_entry("model", &self.resp.model)?;
        map.serialize_entry("answers", &Answers(self.resp, self.req))?;
        map.serialize_entry("usage", &UsageOut(self.resp.usage.unwrap_or_default()))?;
        map.end()
    }
}

struct Answers<'a>(&'a DecisionResponse, &'a DecisionRequest);

impl Serialize for Answers<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.answers.len()))?;
        for (answer, q) in self.0.answers.iter().zip(self.1.questions()) {
            seq.serialize_element(&AnswerOut(answer, q))?;
        }
        seq.end()
    }
}

struct AnswerOut<'a>(&'a Answer, &'a Question);

impl Serialize for AnswerOut<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let name = &self.1.name;
        let mut map = s.serialize_map(None)?;
        match self.0 {
            Answer::Predicate { probability } => {
                map.serialize_entry("type", "predicate")?;
                map.serialize_entry("name", name)?;
                map.serialize_entry("probability", probability)?;
            }
            Answer::Choice {
                choice,
                probabilities,
                confidence,
            } => {
                map.serialize_entry("type", "choice")?;
                map.serialize_entry("name", name)?;
                map.serialize_entry("choice", &Value(choice))?;
                map.serialize_entry("probabilities", &ChoiceProbs(probabilities))?;
                map.serialize_entry(
                    "confidence",
                    &confidence.unwrap_or_else(|| max_p(probabilities.iter().map(|(_, p)| *p))),
                )?;
            }
            Answer::Score {
                score,
                probabilities,
                confidence,
            } => {
                map.serialize_entry("type", "score")?;
                map.serialize_entry("name", name)?;
                map.serialize_entry("score", score)?;
                let levels: &[Level] = match &self.1.kind {
                    QuestionKind::Score { levels } => levels,
                    _ => &[],
                };
                map.serialize_entry("probabilities", &ScoreProbs(probabilities, levels))?;
                map.serialize_entry(
                    "confidence",
                    &confidence.unwrap_or_else(|| max_p(probabilities.iter().map(|(_, p)| *p))),
                )?;
            }
            Answer::Refusal => {
                map.serialize_entry("type", "refusal")?;
                map.serialize_entry("name", name)?;
            }
        }
        map.end()
    }
}

fn max_p(ps: impl Iterator<Item = f64>) -> f64 {
    ps.fold(0.0, f64::max)
}

struct Value<'a>(&'a ChoiceValue);

impl Serialize for Value<'_> {
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
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for (value, p) in self.0 {
            seq.serialize_element(&ChoiceProb(value, *p))?;
        }
        seq.end()
    }
}

struct ChoiceProb<'a>(&'a ChoiceValue, f64);

impl Serialize for ChoiceProb<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(2))?;
        map.serialize_entry("value", &Value(self.0))?;
        map.serialize_entry("probability", &self.1)?;
        map.end()
    }
}

struct ScoreProbs<'a>(&'a [(usize, f64)], &'a [Level]);

impl Serialize for ScoreProbs<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for (i, p) in self.0 {
            let label = self
                .1
                .get(*i)
                .map(|l| l.label.as_prompt().into_owned())
                .unwrap_or_default();
            seq.serialize_element(&ScoreProb {
                value: *i,
                label,
                probability: *p,
            })?;
        }
        seq.end()
    }
}

#[derive(Serialize)]
struct ScoreProb {
    value: usize,
    label: String,
    probability: f64,
}

struct UsageOut(DecisionUsage);

impl Serialize for UsageOut {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct In {
            cached_tokens: u32,
            cache_write_tokens: u32,
        }
        #[derive(Serialize)]
        struct OutD {
            reasoning_tokens: u32,
        }
        let u = self.0;
        let estimated = u.estimated == Some(true);
        let mut map = s.serialize_map(Some(5 + usize::from(estimated)))?;
        map.serialize_entry("input_tokens", &u.input_tokens)?;
        map.serialize_entry(
            "input_tokens_details",
            &In {
                cached_tokens: u.cached_tokens,
                cache_write_tokens: 0,
            },
        )?;
        map.serialize_entry("output_tokens", &u.output_tokens)?;
        map.serialize_entry(
            "output_tokens_details",
            &OutD {
                reasoning_tokens: u.reasoning_tokens,
            },
        )?;
        map.serialize_entry(
            "total_tokens",
            &(u64::from(u.input_tokens) + u64::from(u.output_tokens)),
        )?;
        if estimated {
            map.serialize_entry("estimated", &true)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decisions::format::object_entries;

    fn parse_ok(json: &str) -> DecisionRequest {
        parse(object_entries(json.as_bytes(), "request body").unwrap()).unwrap()
    }

    fn parse_err(json: &str) -> GatewayError {
        parse(object_entries(json.as_bytes(), "request body").unwrap()).unwrap_err()
    }

    fn msg(e: GatewayError) -> String {
        match e {
            GatewayError::InvalidRequest(m) => m,
            other => panic!("expected LM-1001, got {other:?}"),
        }
    }

    const DOC: &str = r#"{"model":"gpt-6-luna","input":"Help! payouts failing.","questions":[
        {"type":"predicate","name":"urgent","instructions":"Is it urgent?"},
        {"type":"choice","instructions":"Which team?","choices":[{"value":"billing","description":"Payments"},{"value":true}]},
        {"type":"score","name":"mood","instructions":"How upset?","levels":[{"label":"calm"},{"label":"angry","description":"shouting"}]}],
        "safety_identifier":"user-123"}"#;

    #[test]
    fn documented_request_parses() {
        let req = parse_ok(DOC);
        assert!(matches!(req.input(), Input::Text(s) if s == "Help! payouts failing."));
        let q = req.questions();
        assert_eq!(q.len(), 3);
        assert_eq!(q[1].name, None);
        assert!(q.iter().all(|q| q.raw.is_none()));
        match &q[1].kind {
            QuestionKind::Choice { choices } => {
                assert!(choices[1].value == ChoiceValue::Bool(true));
            }
            _ => panic!(),
        }
        assert_eq!(req.safety_identifier(), Some("user-123"));
    }

    #[test]
    fn messages_with_images_flatten_in_order() {
        let req = parse_ok(
            r#"{"model":"m","input":[
            {"role":"user","content":"first"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"second"},
              {"type":"input_image","image_url":"data:image/png;base64,AA","detail":"low"}]}],
            "questions":[{"type":"predicate","instructions":"i"}]}"#,
        );
        match req.input() {
            Input::Messages(parts) => {
                assert_eq!(parts.len(), 3);
                assert!(matches!(&parts[2], Part::Image(i) if i.detail.as_deref() == Some("low")));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn contract_violations() {
        let q = r#""questions":[{"type":"predicate","instructions":"i"}]"#;
        assert!(matches!(
            parse_err(r#"{"model":"m","input":"x","questions":[]}"#),
            GatewayError::EmptyQuestions
        ));
        assert!(msg(parse_err(&format!(r#"{{"model":"","input":"x",{q}}}"#))).contains("`model`"));
        assert!(msg(parse_err(&format!(
            r#"{{"model":"m","input":"x",{q},"temperature":1}}"#
        )))
        .contains("`temperature`"));
        assert!(msg(parse_err(&format!(
            r#"{{"model":"m","input":[{{"role":"system","content":"x"}}],{q}}}"#
        )))
        .contains("role \"user\", got \"system\""));
        assert!(matches!(
            parse_err(&format!(
                r#"{{"model":"m","input":[{{"role":"user","content":[{{"type":"input_image","image_url":"https://x/y.png"}}]}}],{q}}}"#
            )),
            GatewayError::ImageUrlNotSupported { .. }
        ));
        assert!(msg(parse_err(
            r#"{"model":"m","input":"x","questions":[{"type":"predicate"}]}"#
        ))
        .contains("#0"));
        assert!(msg(parse_err(
            r#"{"model":"m","input":"x","questions":[{"type":"nope","instructions":"i"}]}"#
        ))
        .contains("type"));
        assert!(msg(parse_err(r#"{"model":"m","input":"x","questions":[{"type":"choice","instructions":"i","choices":[{"value":"a"}]}]}"#)).contains("2 to 255"));
        assert!(msg(parse_err(r#"{"model":"m","input":"x","questions":[{"type":"choice","instructions":"i","choices":[{"value":"a"},{"value":"a"}]}]}"#)).contains("unique"));
        // A string and a boolean with the same text are distinct values.
        parse_ok(
            r#"{"model":"m","input":"x","questions":[{"type":"choice","instructions":"i","choices":[{"value":"true"},{"value":true}]}]}"#,
        );
        assert!(msg(parse_err(r#"{"model":"m","input":"x","questions":[{"type":"score","instructions":"i","levels":[]}]}"#)).contains("level"));
        assert!(msg(parse_err(r#"{"model":"m","input":"x","questions":[{"type":"predicate","name":"a","instructions":"i"},{"type":"predicate","name":"a","instructions":"j"}]}"#)).contains("duplicate"));
        let long = "x".repeat(129);
        assert!(msg(parse_err(&format!(
            r#"{{"model":"m","input":"x",{q},"safety_identifier":"{long}"}}"#
        )))
        .contains("128"));
    }

    #[test]
    fn foreign_and_malformed_question_fields_are_rejected_without_echoing_values() {
        let foreign = |body: &str| msg(parse_err(body));
        let m = foreign(
            r#"{"model":"m","input":"x","questions":[{"type":"predicate","name":"p","instructions":"i","choices":[{"value":"a"},{"value":"b"}]}]}"#,
        );
        assert!(
            m.contains("`p`") && m.contains("`choices`") && m.contains("'predicate'"),
            "{m}"
        );
        let m = foreign(
            r#"{"model":"m","input":"x","questions":[{"type":"choice","instructions":"i","levels":[{"label":"a"}],"choices":[{"value":"a"},{"value":"b"}]}]}"#,
        );
        assert!(
            m.contains("#0") && m.contains("`levels`") && m.contains("'choice'"),
            "{m}"
        );
        let m = foreign(
            r#"{"model":"m","input":"x","questions":[{"type":"score","instructions":"i","levels":[{"label":"a"}],"choices":[{"value":"a"},{"value":"b"}]}]}"#,
        );
        assert!(m.contains("`choices`") && m.contains("'score'"), "{m}");
        let m = foreign(
            r#"{"model":"m","input":"x","questions":[{"type":"predicate","instructions":"SECRET-VALUE"},{"type":7,"instructions":"SECRET-VALUE"}]}"#,
        );
        assert!(m.contains("#1") && !m.contains("SECRET-VALUE"), "{m}");
        let m = foreign(
            r#"{"model":"m","input":"x","questions":[{"type":"SECRET-TYPE","instructions":"i"}]}"#,
        );
        assert!(m.contains("type") && !m.contains("SECRET-TYPE"), "{m}");
    }

    #[test]
    fn more_than_128_images_is_lm_1001() {
        let img = r#"{"type":"input_image","image_url":"data:image/png;base64,AA"}"#;
        let parts = vec![img; 129].join(",");
        let body = format!(
            r#"{{"model":"m","input":[{{"role":"user","content":[{parts}]}}],"questions":[{{"type":"predicate","instructions":"i"}}]}}"#
        );
        assert!(msg(parse_err(&body)).contains("128"));
    }

    #[test]
    fn render_matches_the_openai_sdk_shape() {
        let req = parse_ok(DOC);
        let resp = DecisionResponse {
            model: "gpt-6-luna".into(),
            answers: vec![
                Answer::Predicate { probability: 0.9 },
                Answer::Choice {
                    choice: ChoiceValue::Bool(true),
                    probabilities: vec![
                        (ChoiceValue::Str("billing".into()), 0.25),
                        (ChoiceValue::Bool(true), 0.75),
                    ],
                    confidence: None,
                },
                Answer::Refusal,
            ],
            usage: Some(DecisionUsage {
                input_tokens: 10,
                output_tokens: 2,
                cached_tokens: 0,
                reasoning_tokens: 0,
                estimated: Some(true),
            }),
            upstream: None,
        };
        let out = String::from_utf8(render(&resp, &req).unwrap()).unwrap();
        assert_eq!(
            out,
            concat!(
                r#"{"model":"gpt-6-luna","answers":["#,
                r#"{"type":"predicate","name":"urgent","probability":0.9},"#,
                r#"{"type":"choice","name":null,"choice":true,"probabilities":[{"value":"billing","probability":0.25},{"value":true,"probability":0.75}],"confidence":0.75},"#,
                r#"{"type":"refusal","name":"mood"}],"#,
                r#""usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},"output_tokens":2,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":12,"estimated":true}}"#
            )
        );
    }

    #[test]
    fn score_render_carries_labels_from_the_request() {
        let req = parse_ok(
            r#"{"model":"m","input":"x","questions":[{"type":"score","instructions":"i","levels":[{"label":"low"},{"label":"high"}]}]}"#,
        );
        let resp = DecisionResponse {
            model: "m".into(),
            upstream: None,
            answers: vec![Answer::Score {
                score: 0.7,
                probabilities: vec![(0, 0.3), (1, 0.7)],
                confidence: Some(0.5),
            }],
            usage: Some(DecisionUsage::default()),
        };
        let out = String::from_utf8(render(&resp, &req).unwrap()).unwrap();
        assert!(out.contains(r#""probabilities":[{"value":0,"label":"low","probability":0.3},{"value":1,"label":"high","probability":0.7}],"confidence":0.5"#), "{out}");
    }
}
