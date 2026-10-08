//! Decisions: typed answers with calibrated probabilities (ADR 016).
//!
//! The neutral request and response every edge format parses into and every
//! upstream codec encodes from. The core follows OpenAI's schema (ordered
//! questions, optional names, images, refusals), widened so text fields may
//! hold structured JSON ([`Text::Json`]) as the TypeSafe format allows.
//!
//! Content (input, images, questions, answers, `safety_identifier`) is never
//! logged: every `Debug` impl here prints counts and kinds only.

pub mod format;
pub mod limits;

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use serde_json::value::RawValue;

pub use limits::{DecisionLimits, PackLimits};

/// An ordered JSON object whose values are kept as raw, unparsed JSON.
pub type RawEntries = Vec<(String, Box<RawValue>)>;

/// A text field that may hold structured JSON (the TypeSafe format allows an
/// object as `instructions`, a criterion or a level).
#[derive(Clone)]
pub enum Text {
    /// A plain string.
    Plain(String),
    /// Any JSON value other than a string, kept verbatim.
    Json(Box<RawValue>),
}

impl Text {
    /// Build from a raw JSON value: a JSON string becomes [`Text::Plain`]
    /// (decoded), anything else [`Text::Json`].
    #[must_use]
    pub fn from_raw(raw: Box<RawValue>) -> Self {
        match serde_json::from_str::<String>(raw.get()) {
            Ok(s) => Text::Plain(s),
            Err(_) => Text::Json(raw),
        }
    }

    /// The text as a model reads it in a plain-text field: the string itself,
    /// or the JSON text (compact when the gateway built it).
    #[must_use]
    pub fn as_prompt(&self) -> Cow<'_, str> {
        match self {
            Text::Plain(s) => Cow::Borrowed(s),
            Text::Json(raw) => match serde_json::from_str::<String>(raw.get()) {
                Ok(s) => Cow::Owned(s),
                Err(_) => Cow::Borrowed(raw.get()),
            },
        }
    }

    /// Byte length of the text as sent (for token estimates).
    #[must_use]
    pub fn byte_len(&self) -> usize {
        match self {
            Text::Plain(s) => s.len(),
            Text::Json(raw) => raw.get().len(),
        }
    }
}

/// An inline image (`data:` URL only; remote URLs are rejected at the edge).
#[derive(Clone)]
pub struct Image {
    /// The `data:<mime>;base64,...` URL.
    pub data_url: String,
    /// OpenAI's `detail` hint (`low`, `high`, `auto`, `original`), if given.
    pub detail: Option<String>,
}

/// One part of a multimodal input, in order.
#[derive(Clone)]
pub enum Part {
    /// Text (or a structured JSON element of a TypeSafe `state` array).
    Text(Text),
    /// An inline image.
    Image(Image),
}

/// The content every question is evaluated against.
#[derive(Clone)]
pub enum Input {
    /// An OpenAI string `input`.
    Text(String),
    /// A TypeSafe `state` without images (string, object or array), verbatim.
    Structured(Box<RawValue>),
    /// Text and image parts, flattened in order (OpenAI messages, or a
    /// TypeSafe `state` array carrying `image_url` parts).
    Messages(Vec<Part>),
}

/// A `choice` option value: OpenAI types them, so `"true"` and `true` differ.
#[derive(Clone, PartialEq, Eq)]
pub enum ChoiceValue {
    /// A string value (every TypeSafe option is one).
    Str(String),
    /// A boolean value (OpenAI only).
    Bool(bool),
}

impl ChoiceValue {
    /// The spelling on a string-keyed (TypeSafe-family) wire.
    #[must_use]
    pub fn wire_key(&self) -> Cow<'_, str> {
        match self {
            ChoiceValue::Str(s) => Cow::Borrowed(s),
            ChoiceValue::Bool(true) => Cow::Borrowed("true"),
            ChoiceValue::Bool(false) => Cow::Borrowed("false"),
        }
    }
}

/// One option of a `choice` question.
#[derive(Clone)]
pub struct ChoiceOption {
    /// The value answered back.
    pub value: ChoiceValue,
    /// What the option means, if described.
    pub description: Option<Text>,
}

/// One level of a `score` question; its index is the score.
#[derive(Clone)]
pub struct Level {
    /// The level's label.
    pub label: Text,
    /// What the level means, if described.
    pub description: Option<Text>,
}

/// TypeSafe's optional meanings of yes and no for a predicate.
#[derive(Clone)]
pub struct PredicateCriteria {
    /// What a yes means.
    pub when_true: Option<Text>,
    /// What a no means.
    pub when_false: Option<Text>,
}

/// What a question asks for.
#[derive(Clone)]
pub enum QuestionKind {
    /// Yes or no; the answer is the probability of yes (TypeSafe `noul`).
    Predicate {
        /// TypeSafe `criteria.true` / `criteria.false`, if any.
        criteria: Option<PredicateCriteria>,
    },
    /// Pick one option.
    Choice {
        /// The options, in request order.
        choices: Vec<ChoiceOption>,
    },
    /// Rate against ordered levels.
    Score {
        /// The levels, worst first.
        levels: Vec<Level>,
    },
}

/// One question. `answers[i]` of a response answers `questions[i]`.
#[derive(Clone)]
pub struct Question {
    /// OpenAI `name`, or the TypeSafe question id.
    pub name: Option<String>,
    /// The question text; `None` only for a criteria-only TypeSafe predicate.
    pub instructions: Option<Text>,
    /// The question type and its options or levels.
    pub kind: QuestionKind,
    /// The client's TypeSafe question body, verbatim (D9), re-sent as-is to
    /// TypeSafe-family upstreams.
    pub raw: Option<Box<RawValue>>,
}

impl Question {
    /// How errors name this question: `` `name` `` or `#index`.
    #[must_use]
    pub fn label(&self, index: usize) -> String {
        match &self.name {
            Some(name) => format!("`{name}`"),
            None => format!("#{index}"),
        }
    }
}

/// A decision request. Everything but `model` is shared behind an [`Arc`],
/// so the per-attempt clone of the retry/fallback executor is a refcount
/// bump, not a copy of an input that can weigh megabytes.
#[derive(Clone)]
pub struct DecisionRequest {
    /// Client-facing model id (rewritten to the upstream id per attempt).
    pub model: String,
    body: Arc<DecisionBody>,
}

struct DecisionBody {
    input: Input,
    questions: Vec<Question>,
    safety_identifier: Option<String>,
    extra: RawEntries,
}

impl DecisionRequest {
    /// Build a request (edge codecs, the rerank remap, tests).
    #[must_use]
    pub fn new(model: String, input: Input, questions: Vec<Question>) -> Self {
        Self {
            model,
            body: Arc::new(DecisionBody {
                input,
                questions,
                safety_identifier: None,
                extra: Vec::new(),
            }),
        }
    }

    /// Set OpenAI's `safety_identifier` (consumes a not-yet-shared request).
    #[must_use]
    pub fn with_safety_identifier(self, id: Option<String>) -> Self {
        self.map_body(|b| b.safety_identifier = id)
    }

    /// Set the unknown TypeSafe top-level fields to forward.
    #[must_use]
    pub fn with_extra(self, extra: RawEntries) -> Self {
        self.map_body(|b| b.extra = extra)
    }

    fn map_body(self, f: impl FnOnce(&mut DecisionBody)) -> Self {
        let model = self.model;
        let mut body = Arc::try_unwrap(self.body).unwrap_or_else(|shared| DecisionBody {
            input: shared.input.clone(),
            questions: shared.questions.clone(),
            safety_identifier: shared.safety_identifier.clone(),
            extra: shared.extra.clone(),
        });
        f(&mut body);
        Self {
            model,
            body: Arc::new(body),
        }
    }

    /// The input.
    #[must_use]
    pub fn input(&self) -> &Input {
        &self.body.input
    }

    /// The questions, in request order.
    #[must_use]
    pub fn questions(&self) -> &[Question] {
        &self.body.questions
    }

    /// OpenAI's `safety_identifier`; never logged.
    #[must_use]
    pub fn safety_identifier(&self) -> Option<&str> {
        self.body.safety_identifier.as_deref()
    }

    /// Unknown TypeSafe top-level fields, in client order.
    #[must_use]
    pub fn extra(&self) -> &[(String, Box<RawValue>)] {
        &self.body.extra
    }

    /// Number of image parts.
    #[must_use]
    pub fn image_count(&self) -> usize {
        match &self.body.input {
            Input::Messages(parts) => parts.iter().filter(|p| matches!(p, Part::Image(_))).count(),
            _ => 0,
        }
    }

    /// Whether any image part is present.
    #[must_use]
    pub fn has_images(&self) -> bool {
        self.image_count() > 0
    }
}

/// Counts only: the request is content (spec 12).
impl fmt::Debug for DecisionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecisionRequest")
            .field("model", &self.model)
            .field("questions", &self.body.questions.len())
            .field("images", &self.image_count())
            .finish_non_exhaustive()
    }
}

/// One answer; `answers[i]` answers `questions[i]`.
#[derive(Clone)]
pub enum Answer {
    /// Probability of yes.
    Predicate {
        /// In `[0, 1]`.
        probability: f64,
    },
    /// The chosen option and the distribution, in the request's option order.
    Choice {
        /// The most likely option.
        choice: ChoiceValue,
        /// `(option, probability)` per request option.
        probabilities: Vec<(ChoiceValue, f64)>,
        /// Upstream confidence, if reported.
        confidence: Option<f64>,
    },
    /// The expected level index and the distribution over level indices.
    Score {
        /// Probability-weighted mean of the level indices.
        score: f64,
        /// `(level index, probability)`, ascending index.
        probabilities: Vec<(usize, f64)>,
        /// Upstream confidence, if reported.
        confidence: Option<f64>,
    },
    /// The upstream declined this question (OpenAI only).
    Refusal,
}

/// Kinds only: answers are content (spec 12).
impl fmt::Debug for Answer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Answer::Predicate { .. } => "Predicate",
            Answer::Choice { .. } => "Choice",
            Answer::Score { .. } => "Score",
            Answer::Refusal => "Refusal",
        })
    }
}

/// Token usage. Upstream-reported when available, else estimated by the
/// gateway and flagged (ADR 003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecisionUsage {
    /// Input tokens (the billed unit).
    pub input_tokens: u32,
    /// Output tokens (reported, not billed).
    pub output_tokens: u32,
    /// OpenAI cached input tokens (passed through, not billed).
    pub cached_tokens: u32,
    /// OpenAI reasoning tokens (passed through, not billed).
    pub reasoning_tokens: u32,
    /// `Some(true)` when the gateway estimated the counts.
    pub estimated: Option<bool>,
}

/// A decision response.
#[derive(Clone)]
pub struct DecisionResponse {
    /// The model that answered, as reported upstream.
    pub model: String,
    /// One answer per request question, in request order.
    pub answers: Vec<Answer>,
    /// Upstream usage, parsed leniently (`None` when missing or malformed).
    pub usage: Option<DecisionUsage>,
    /// The whole TypeSafe-family response object, verbatim, so a
    /// TypeSafe-format client receives the upstream bytes (D9).
    pub upstream: Option<RawEntries>,
}

/// Counts only: the response is content (spec 12).
impl fmt::Debug for DecisionResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecisionResponse")
            .field("model", &self.model)
            .field("answers", &self.answers)
            .field("usage", &self.usage)
            .field("upstream", &self.upstream.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::value::to_raw_value;

    fn raw(json: &str) -> Box<RawValue> {
        RawValue::from_string(json.to_owned()).unwrap()
    }

    fn predicate(name: Option<&str>) -> Question {
        Question {
            name: name.map(str::to_owned),
            instructions: Some(Text::Plain("urgent?".into())),
            kind: QuestionKind::Predicate { criteria: None },
            raw: None,
        }
    }

    #[test]
    fn text_as_prompt_decodes_json_strings_and_keeps_objects() {
        assert_eq!(Text::Plain("a".into()).as_prompt(), "a");
        assert_eq!(Text::from_raw(raw(r#""café""#)).as_prompt(), "caf\u{e9}");
        assert!(matches!(Text::from_raw(raw(r#""x""#)), Text::Plain(_)));
        let obj = Text::from_raw(raw(r#"{"document":"d","question":"q"}"#));
        assert!(matches!(obj, Text::Json(_)));
        assert_eq!(obj.as_prompt(), r#"{"document":"d","question":"q"}"#);
    }

    #[test]
    fn clones_share_the_body() {
        let req = DecisionRequest::new(
            "m".into(),
            Input::Structured(raw(r#"{"big":"state"}"#)),
            vec![predicate(Some("a"))],
        );
        let mut attempt = req.clone();
        "upstream".clone_into(&mut attempt.model);
        assert!(std::ptr::eq(req.questions(), attempt.questions()));
        assert_eq!(req.model, "m");
    }

    #[test]
    fn images_are_counted_across_parts() {
        let img = |d: &str| {
            Part::Image(Image {
                data_url: d.into(),
                detail: None,
            })
        };
        let req = DecisionRequest::new(
            "m".into(),
            Input::Messages(vec![
                Part::Text(Text::Plain("t".into())),
                img("data:a"),
                img("data:b"),
            ]),
            vec![predicate(None)],
        );
        assert_eq!(req.image_count(), 2);
        assert!(req.has_images());
    }

    #[test]
    fn debug_never_prints_content() {
        let req = DecisionRequest::new(
            "m".into(),
            Input::Text("SECRET-INPUT".into()),
            vec![Question {
                name: Some("SECRET-NAME".into()),
                instructions: Some(Text::Plain("SECRET-Q".into())),
                kind: QuestionKind::Predicate { criteria: None },
                raw: None,
            }],
        )
        .with_safety_identifier(Some("SECRET-USER".into()));
        let resp = DecisionResponse {
            model: "m".into(),
            answers: vec![Answer::Choice {
                choice: ChoiceValue::Str("SECRET-CHOICE".into()),
                probabilities: vec![],
                confidence: None,
            }],
            usage: None,
            upstream: Some(vec![(
                "answers".into(),
                to_raw_value("SECRET-RAW").unwrap(),
            )]),
        };
        let printed = format!("{req:?} {resp:?}");
        assert!(!printed.contains("SECRET"), "{printed}");
    }

    #[test]
    fn choice_wire_keys_spell_booleans() {
        assert_eq!(ChoiceValue::Bool(true).wire_key(), "true");
        assert_eq!(ChoiceValue::Str("billing".into()).wire_key(), "billing");
    }

    #[test]
    fn question_label_prefers_the_name() {
        assert_eq!(predicate(Some("urgent")).label(3), "`urgent`");
        assert_eq!(predicate(None).label(3), "#3");
    }
}
