//! OpenAI's `/v1/decisions` wire codec and provider (spec 7.2, 8).

use async_trait::async_trait;
use lumen_core::decisions::{
    Answer, ChoiceValue, DecisionLimits, DecisionRequest, DecisionResponse, DecisionUsage, Input,
    PackLimits, Part, PredicateCriteria, Question, QuestionKind, Text,
};
use lumen_core::{DecisionProvider, ProviderError};
use tokio_util::sync::CancellationToken;

use crate::http::post_json_bytes;
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::value::RawValue;

/// What `gpt-6-luna` accepts (OpenAI publishes no question cap).
pub const LIMITS: DecisionLimits = DecisionLimits {
    max_questions: None,
    min_choice_options: 2,
    max_choice_options: 255,
    min_score_levels: 1,
    max_score_levels: 255,
    predicate_needs_instructions: false,
    string_keyed_choices: false,
    max_images: Some(128),
    pack: PackLimits {
        max_call_tokens: 48_000,
        max_docs_per_call: 100,
        concurrency: 4,
    },
};

fn translation(msg: impl Into<String>) -> ProviderError {
    ProviderError::Translation(msg.into())
}

/// Encode `req` for OpenAI.
///
/// # Errors
/// [`ProviderError::Translation`] if serialization fails.
pub fn encode(req: &DecisionRequest, upstream_id: &str) -> Result<Vec<u8>, ProviderError> {
    serde_json::to_vec(&WireRequest { req, upstream_id })
        .map_err(|_| translation("openai decisions request could not be serialized"))
}

struct WireRequest<'a> {
    req: &'a DecisionRequest,
    upstream_id: &'a str,
}

impl Serialize for WireRequest<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(None)?;
        map.serialize_entry("model", self.upstream_id)?;
        map.serialize_entry("input", &InputOut(self.req.input()))?;
        map.serialize_entry("questions", &QuestionsOut(self.req.questions()))?;
        if let Some(id) = self.req.safety_identifier() {
            map.serialize_entry("safety_identifier", id)?;
        }
        map.end()
    }
}

struct InputOut<'a>(&'a Input);

impl Serialize for InputOut<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Input::Text(text) => s.serialize_str(text),
            // A JSON-string state is sent as its text; anything else as its
            // JSON text, which the model reads (spec 7.2).
            Input::Structured(raw) => s.serialize_str(&Text::Json(raw.clone()).as_prompt()),
            Input::Messages(parts) => {
                #[derive(Serialize)]
                struct Message<'a> {
                    role: &'static str,
                    content: Parts<'a>,
                }
                let mut seq = s.serialize_seq(Some(1))?;
                seq.serialize_element(&Message {
                    role: "user",
                    content: Parts(parts),
                })?;
                seq.end()
            }
        }
    }
}

struct Parts<'a>(&'a [Part]);

impl Serialize for Parts<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for part in self.0 {
            match part {
                Part::Text(t) => {
                    #[derive(Serialize)]
                    struct InputText<'a> {
                        #[serde(rename = "type")]
                        kind: &'static str,
                        text: &'a str,
                    }
                    seq.serialize_element(&InputText {
                        kind: "input_text",
                        text: &t.as_prompt(),
                    })?;
                }
                Part::Image(img) => {
                    #[derive(Serialize)]
                    struct InputImage<'a> {
                        #[serde(rename = "type")]
                        kind: &'static str,
                        image_url: &'a str,
                        #[serde(skip_serializing_if = "Option::is_none")]
                        detail: Option<&'a str>,
                    }
                    seq.serialize_element(&InputImage {
                        kind: "input_image",
                        image_url: &img.data_url,
                        detail: img.detail.as_deref(),
                    })?;
                }
            }
        }
        seq.end()
    }
}

struct QuestionsOut<'a>(&'a [Question]);

impl Serialize for QuestionsOut<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for q in self.0 {
            seq.serialize_element(&QuestionOut(q))?;
        }
        seq.end()
    }
}

/// `instructions` plus TypeSafe's yes/no meanings as one sentence.
fn predicate_instructions(q: &Question, criteria: Option<&PredicateCriteria>) -> String {
    let base = q.instructions.as_ref().map(|t| t.as_prompt().into_owned());
    let mut clauses = Vec::new();
    if let Some(c) = criteria {
        if let Some(t) = &c.when_true {
            clauses.push(format!("Answer true if: {}.", t.as_prompt()));
        }
        if let Some(f) = &c.when_false {
            clauses.push(format!("Answer false if: {}.", f.as_prompt()));
        }
    }
    match (base, clauses.is_empty()) {
        (Some(b), true) => b,
        (Some(b), false) => format!("{b}\n{}", clauses.join(" ")),
        (None, _) => clauses.join(" "),
    }
}

struct QuestionOut<'a>(&'a Question);

impl Serialize for QuestionOut<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let q = self.0;
        let mut map = s.serialize_map(None)?;
        let instructions = || {
            q.instructions
                .as_ref()
                .map(|t| t.as_prompt().into_owned())
                .unwrap_or_default()
        };
        match &q.kind {
            QuestionKind::Predicate { criteria } => {
                map.serialize_entry("type", "predicate")?;
                if let Some(name) = &q.name {
                    map.serialize_entry("name", name)?;
                }
                map.serialize_entry(
                    "instructions",
                    &predicate_instructions(q, criteria.as_ref()),
                )?;
            }
            QuestionKind::Choice { choices } => {
                map.serialize_entry("type", "choice")?;
                if let Some(name) = &q.name {
                    map.serialize_entry("name", name)?;
                }
                map.serialize_entry("instructions", &instructions())?;
                map.serialize_entry("choices", &ChoicesOut(choices))?;
            }
            QuestionKind::Score { levels } => {
                map.serialize_entry("type", "score")?;
                if let Some(name) = &q.name {
                    map.serialize_entry("name", name)?;
                }
                map.serialize_entry("instructions", &instructions())?;
                map.serialize_entry("levels", &LevelsOut(levels))?;
            }
        }
        map.end()
    }
}

struct ChoicesOut<'a>(&'a [lumen_core::decisions::ChoiceOption]);

impl Serialize for ChoicesOut<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for c in self.0 {
            seq.serialize_element(&ChoiceOut(c))?;
        }
        seq.end()
    }
}

struct ChoiceOut<'a>(&'a lumen_core::decisions::ChoiceOption);

impl Serialize for ChoiceOut<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(None)?;
        match &self.0.value {
            ChoiceValue::Str(v) => map.serialize_entry("value", v)?,
            ChoiceValue::Bool(b) => map.serialize_entry("value", b)?,
        }
        if let Some(d) = &self.0.description {
            map.serialize_entry("description", &d.as_prompt())?;
        }
        map.end()
    }
}

struct LevelsOut<'a>(&'a [lumen_core::decisions::Level]);

impl Serialize for LevelsOut<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for level in self.0 {
            #[derive(Serialize)]
            struct LevelOut {
                label: String,
                #[serde(skip_serializing_if = "Option::is_none")]
                description: Option<String>,
            }
            seq.serialize_element(&LevelOut {
                label: level.label.as_prompt().into_owned(),
                description: level
                    .description
                    .as_ref()
                    .map(|d| d.as_prompt().into_owned()),
            })?;
        }
        seq.end()
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WireValue {
    Bool(bool),
    Str(String),
}

impl WireValue {
    fn into_choice(self) -> ChoiceValue {
        match self {
            WireValue::Bool(b) => ChoiceValue::Bool(b),
            WireValue::Str(s) => ChoiceValue::Str(s),
        }
    }
}

#[derive(Deserialize)]
struct WireChoiceProb {
    value: WireValue,
    probability: f64,
}

#[derive(Deserialize)]
struct WireScoreProb {
    value: usize,
    probability: f64,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireAnswer {
    Predicate {
        probability: f64,
    },
    Choice {
        choice: WireValue,
        #[serde(default)]
        probabilities: Vec<WireChoiceProb>,
        #[serde(default)]
        confidence: Option<f64>,
    },
    Score {
        score: f64,
        #[serde(default)]
        probabilities: Vec<WireScoreProb>,
        #[serde(default)]
        confidence: Option<f64>,
    },
    Refusal,
}

#[derive(Deserialize, Default)]
struct Details {
    #[serde(default)]
    cached_tokens: u32,
    #[serde(default)]
    reasoning_tokens: u32,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    input_tokens_details: Option<Details>,
    #[serde(default)]
    output_tokens_details: Option<Details>,
}

#[derive(Deserialize)]
struct WireResponse {
    #[serde(default)]
    model: Option<String>,
    answers: Vec<Box<RawValue>>,
    #[serde(default)]
    usage: Option<Box<RawValue>>,
}

/// Decode OpenAI's response; answers match questions by position.
///
/// # Errors
/// [`ProviderError::Translation`] (`LM-3002`) for a malformed body, an
/// answer count that differs from the question count, an answer whose type
/// does not match its question, or a choice outside the request's options.
pub fn decode(bytes: &[u8], req: &DecisionRequest) -> Result<DecisionResponse, ProviderError> {
    // Serde messages can quote the offending value, which is answer content
    // (spec 12): every decode error here is value-free.
    let wire: WireResponse = serde_json::from_slice(bytes)
        .map_err(|_| translation("openai decisions response is malformed"))?;
    let questions = req.questions();
    if wire.answers.len() != questions.len() {
        return Err(translation(format!(
            "openai decisions response: {} answers for {} questions",
            wire.answers.len(),
            questions.len()
        )));
    }
    let answers = wire
        .answers
        .into_iter()
        .zip(questions)
        .enumerate()
        .map(|(i, (raw, q))| {
            let a: WireAnswer = serde_json::from_str(raw.get())
                .map_err(|_| translation(format!("openai answer #{i}: malformed answer")))?;
            convert(a, q, i)
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Lenient: a malformed usage never fails a billed answer (ADR 003).
    let usage = wire
        .usage
        .and_then(|v| serde_json::from_str::<WireUsage>(v.get()).ok())
        .map(|u| DecisionUsage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cached_tokens: u.input_tokens_details.unwrap_or_default().cached_tokens,
            reasoning_tokens: u.output_tokens_details.unwrap_or_default().reasoning_tokens,
            estimated: None,
        });
    Ok(DecisionResponse {
        model: wire.model.unwrap_or_else(|| req.model.clone()),
        answers,
        usage,
        upstream: None,
    })
}

fn convert(a: WireAnswer, q: &Question, i: usize) -> Result<Answer, ProviderError> {
    let bad = |what: &str| translation(format!("openai answer #{i}: {what}"));
    match (a, &q.kind) {
        (WireAnswer::Refusal, _) => Ok(Answer::Refusal),
        (WireAnswer::Predicate { probability }, QuestionKind::Predicate { .. }) => {
            Ok(Answer::Predicate { probability })
        }
        (
            WireAnswer::Choice {
                choice,
                probabilities,
                confidence,
            },
            QuestionKind::Choice { choices },
        ) => {
            let choice = choice.into_choice();
            if !choices.iter().any(|c| c.value == choice) {
                return Err(bad("`choice` is not one of the request's options"));
            }
            let got: Vec<(ChoiceValue, f64)> = probabilities
                .into_iter()
                .map(|p| (p.value.into_choice(), p.probability))
                .collect();
            let probabilities = choices
                .iter()
                .map(|c| {
                    (
                        c.value.clone(),
                        got.iter()
                            .find(|(v, _)| *v == c.value)
                            .map_or(0.0, |(_, p)| *p),
                    )
                })
                .collect();
            Ok(Answer::Choice {
                choice,
                probabilities,
                confidence,
            })
        }
        (
            WireAnswer::Score {
                score,
                probabilities,
                confidence,
            },
            QuestionKind::Score { levels },
        ) => {
            let mut probabilities: Vec<(usize, f64)> = probabilities
                .into_iter()
                .filter(|p| p.value < levels.len())
                .map(|p| (p.value, p.probability))
                .collect();
            probabilities.sort_unstable_by_key(|(i, _)| *i);
            Ok(Answer::Score {
                score,
                probabilities,
                confidence,
            })
        }
        _ => Err(bad("type does not match the question")),
    }
}

/// OpenAI's decision models (`gpt-6-luna`).
pub struct OpenAiDecisionProvider {
    client: reqwest::Client,
    provider_name: String,
    /// `{base}/decisions`, `base` defaulting to `https://api.openai.com/v1`.
    url: String,
    /// Bearer key; redacted from `Debug`, never logged.
    api_key: Option<String>,
}

impl OpenAiDecisionProvider {
    /// Construct; `base_url` is the OpenAI API base including `/v1`.
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        name: impl Into<String>,
        base_url: Option<String>,
        api_key: Option<String>,
    ) -> Self {
        let base = base_url.unwrap_or_else(|| crate::openai::DEFAULT_BASE_URL.to_owned());
        Self {
            client,
            provider_name: name.into(),
            url: format!("{}/decisions", base.trim_end_matches('/')),
            api_key,
        }
    }
}

impl std::fmt::Debug for OpenAiDecisionProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiDecisionProvider")
            .field("provider_name", &self.provider_name)
            .field("url", &self.url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl DecisionProvider for OpenAiDecisionProvider {
    async fn decide(
        &self,
        req: DecisionRequest,
        cancel: CancellationToken,
    ) -> Result<DecisionResponse, ProviderError> {
        let body = encode(&req, &req.model)?;
        let bytes = post_json_bytes(
            &self.client,
            &self.url,
            body,
            self.api_key.as_deref(),
            &self.provider_name,
            &cancel,
        )
        .await?;
        decode(&bytes, &req)
    }

    fn limits(&self) -> &DecisionLimits {
        &LIMITS
    }

    fn provider_name(&self) -> &str {
        &self.provider_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_defaults_to_openai_and_trims_trailing_slash() {
        let c = reqwest::Client::new();
        let d = OpenAiDecisionProvider::new(c.clone(), "o", None, None);
        assert_eq!(d.url, "https://api.openai.com/v1/decisions");
        let p = OpenAiDecisionProvider::new(c, "o", Some("https://proxy/v1/".into()), None);
        assert_eq!(p.url, "https://proxy/v1/decisions");
    }
    use lumen_core::decisions::format::{parse, Format};

    fn enc(body: &str, forced: Option<Format>) -> String {
        let req = parse(body.as_bytes(), forced).unwrap().1;
        String::from_utf8(encode(&req, "gpt-6-luna").unwrap()).unwrap()
    }

    #[test]
    fn typesafe_requests_translate_to_openai_wire() {
        let out = enc(
            r#"{"model":"x","state":{"ticket":"help"},"questions":{
            "urgent":{"type":"noul","instructions":"Urgent?","criteria":{"true":"time-sensitive","false":"can wait"}},
            "only":{"type":"noul","criteria":{"true":"mentions money"}},
            "team":{"type":"choice","instructions":{"q":"team"},"criteria":{"tech":"Bugs","billing":null}},
            "mood":{"type":"score","instructions":"Mood?","criteria":["calm","angry"]}},"future":1}"#,
            Some(Format::TypeSafe),
        );
        assert_eq!(
            out,
            concat!(
                r#"{"model":"gpt-6-luna","input":"{\"ticket\":\"help\"}","questions":["#,
                r#"{"type":"predicate","name":"urgent","instructions":"Urgent?\nAnswer true if: time-sensitive. Answer false if: can wait."},"#,
                r#"{"type":"predicate","name":"only","instructions":"Answer true if: mentions money."},"#,
                r#"{"type":"choice","name":"team","instructions":"{\"q\":\"team\"}","choices":[{"value":"tech","description":"Bugs"},{"value":"billing"}]},"#,
                r#"{"type":"score","name":"mood","instructions":"Mood?","levels":[{"label":"calm"},{"label":"angry"}]}]}"#
            )
        );
    }

    #[test]
    fn a_string_state_is_sent_as_its_text() {
        let out = enc(
            r#"{"model":"x","state":"café","questions":{"q":{"type":"noul","instructions":"i"}}}"#,
            Some(Format::TypeSafe),
        );
        assert!(out.contains("\"input\":\"caf\u{e9}\""), "{out}");
    }

    #[test]
    fn openai_requests_round_trip_with_images_and_safety_identifier() {
        let out = enc(
            r#"{"model":"x","input":[{"role":"user","content":[
            {"type":"input_text","text":"look"},{"type":"input_image","image_url":"data:image/png;base64,AA","detail":"low"}]}],
            "questions":[{"type":"choice","instructions":"i","choices":[{"value":true},{"value":"true","description":"d"}]}],
            "safety_identifier":"u1"}"#,
            None,
        );
        assert_eq!(
            out,
            concat!(
                r#"{"model":"gpt-6-luna","input":[{"role":"user","content":[{"type":"input_text","text":"look"},{"type":"input_image","image_url":"data:image/png;base64,AA","detail":"low"}]}],"#,
                r#""questions":[{"type":"choice","instructions":"i","choices":[{"value":true},{"value":"true","description":"d"}]}],"#,
                r#""safety_identifier":"u1"}"#
            )
        );
    }

    fn req3() -> DecisionRequest {
        parse(
            br#"{"model":"m","input":"x","questions":[
            {"type":"predicate","name":"u","instructions":"i"},
            {"type":"choice","instructions":"i","choices":[{"value":"a"},{"value":false}]},
            {"type":"score","instructions":"i","levels":[{"label":"lo"},{"label":"hi"}]}]}"#,
            None,
        )
        .unwrap()
        .1
    }

    const OUT: &str = r#"{"model":"gpt-6-luna","answers":[
        {"type":"predicate","name":"u","probability":0.8},
        {"type":"choice","name":null,"choice":false,"probabilities":[{"value":false,"probability":0.9},{"value":"a","probability":0.1}],"confidence":0.9},
        {"type":"refusal","name":null}],
        "usage":{"input_tokens":20,"input_tokens_details":{"cached_tokens":4,"cache_write_tokens":0},"output_tokens":3,"output_tokens_details":{"reasoning_tokens":1},"total_tokens":23}}"#;

    #[test]
    fn decode_reads_answers_by_position() {
        let resp = decode(OUT.as_bytes(), &req3()).unwrap();
        assert!(matches!(resp.answers[0], Answer::Predicate { .. }));
        match &resp.answers[1] {
            Answer::Choice {
                choice,
                probabilities,
                ..
            } => {
                assert!(*choice == ChoiceValue::Bool(false));
                // Reordered to the request's option order.
                assert!(probabilities[0].0 == ChoiceValue::Str("a".into()));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(resp.answers[2], Answer::Refusal));
        let u = resp.usage.unwrap();
        assert_eq!(
            (
                u.input_tokens,
                u.output_tokens,
                u.cached_tokens,
                u.reasoning_tokens
            ),
            (20, 3, 4, 1)
        );
        assert!(resp.upstream.is_none());
    }

    #[test]
    fn answer_count_or_type_mismatch_is_a_translation_error() {
        let short = OUT.replace(
            r#",
        {"type":"refusal","name":null}"#,
            "",
        );
        let mistyped = OUT.replace(
            r#"{"type":"predicate","name":"u","probability":0.8}"#,
            r#"{"type":"score","name":"u","score":1,"probabilities":[],"confidence":1}"#,
        );
        let foreign = OUT.replace(r#""choice":false,"#, r#""choice":"zzz","#);
        for body in [short, mistyped, foreign] {
            assert!(
                matches!(
                    decode(body.as_bytes(), &req3()),
                    Err(ProviderError::Translation(_))
                ),
                "{body}"
            );
        }
    }

    #[test]
    fn decode_errors_do_not_quote_answer_content() {
        for body in [
            r#"{"model":"m","answers":[{"type":"LEAKME"},{"type":"refusal"},{"type":"refusal"}]}"#,
            r#"{"model":"m","answers":[{"type":"predicate","probability":"LEAKME"},{"type":"refusal"},{"type":"refusal"}]}"#,
            r#"{"model":"m","answers":"LEAKME"}"#,
            r#"{"model":"m","answers":[{"type":"refusal"},{"type":"choice","choice":"LEAKME"},{"type":"refusal"}]}"#,
            r#"{"model":"m","answers":[{"type":"refusal"},{"type":"refusal"},{"type":"score","score":"LEAKME"}]}"#,
        ] {
            let err = decode(body.as_bytes(), &req3()).unwrap_err();
            assert!(matches!(err, ProviderError::Translation(_)), "{err}");
            assert!(!err.to_string().contains("LEAKME"), "{err}");
        }
    }
}
