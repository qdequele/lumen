//! The TypeSafe-family wire codec (spec 7.1, 8). One encoder for every
//! vendor that speaks the TypeSafe format; what differs (unknown fields,
//! image placement, limits, response envelope) is a [`FamilyProfile`].

use std::collections::HashSet;

use async_trait::async_trait;
use lumen_core::decisions::format::object_entries;
use lumen_core::decisions::{
    Answer, DecisionLimits, DecisionRequest, DecisionResponse, DecisionUsage, Input, PackLimits,
    Part, Question, QuestionKind, Text,
};
use lumen_core::{DecisionProvider, ProviderError};
use tokio_util::sync::CancellationToken;

use crate::http::post_json_bytes;
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::value::RawValue;

/// Where images go on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagePlacement {
    /// Not accepted (the profile's `max_images` is 0).
    None,
    /// Inside a `state` array as `{"type": "image_url", ...}` parts (Perplexity).
    StateParts,
    /// Text joined into `state`, images in a top-level `images` array
    /// (Ollama: raw base64, data-URL prefix stripped; Cloudflare: data URLs).
    TopLevel {
        /// Strip `data:<mime>;base64,`.
        raw_base64: bool,
    },
}

/// The response wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Envelope {
    /// The TypeSafe response object itself.
    Bare,
    /// Workers AI `{result, success, errors, messages}`.
    Cloudflare,
}

/// What one TypeSafe-format vendor needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilyProfile {
    /// The provider kind, for logs.
    pub kind: &'static str,
    /// Forward unknown top-level request fields.
    pub forward_unknown_fields: bool,
    /// Image placement.
    pub images: ImagePlacement,
    /// Response envelope.
    pub envelope: Envelope,
    /// Accepted shapes and rerank packing.
    pub limits: DecisionLimits,
}

impl FamilyProfile {
    /// TypeSafe (Jev) and the generic TypeSafe-format kind.
    #[must_use]
    pub fn typesafe(forward_unknown_fields: bool) -> Self {
        Self {
            kind: "typesafe",
            forward_unknown_fields,
            images: ImagePlacement::None,
            envelope: Envelope::Bare,
            limits: DecisionLimits {
                predicate_needs_instructions: true,
                max_images: Some(0),
                ..DecisionLimits::TYPESAFE
            },
        }
    }

    /// Perplexity (`pplx-decider-*`).
    #[must_use]
    pub fn perplexity() -> Self {
        Self {
            kind: "perplexity",
            forward_unknown_fields: false,
            images: ImagePlacement::StateParts,
            envelope: Envelope::Bare,
            limits: DecisionLimits {
                max_questions: Some(128),
                ..DecisionLimits::TYPESAFE
            },
        }
    }

    /// Ollama's `/v1/systemone` (local Nimble, Clef, Kev).
    #[must_use]
    pub fn ollama() -> Self {
        Self {
            kind: "ollama",
            forward_unknown_fields: false,
            images: ImagePlacement::TopLevel { raw_base64: true },
            envelope: Envelope::Bare,
            limits: DecisionLimits {
                min_choice_options: 2,
                max_choice_options: 26,
                min_score_levels: 2,
                max_score_levels: 26,
                pack: PackLimits {
                    max_call_tokens: 16_000,
                    max_docs_per_call: 100,
                    concurrency: 1,
                },
                ..DecisionLimits::TYPESAFE
            },
        }
    }

    /// Cloudflare Workers AI Clef.
    #[must_use]
    pub fn cloudflare() -> Self {
        Self {
            kind: "cloudflare",
            forward_unknown_fields: false,
            images: ImagePlacement::TopLevel { raw_base64: false },
            envelope: Envelope::Cloudflare,
            limits: DecisionLimits {
                max_questions: Some(64),
                max_images: Some(4),
                pack: PackLimits {
                    max_docs_per_call: 64,
                    ..DecisionLimits::TYPESAFE.pack
                },
                ..DecisionLimits::TYPESAFE
            },
        }
    }
}

/// The wire id of each question: its name, or `q{index}` prefixed with `_`
/// until it collides with no name and no earlier id (spec 7.1).
#[must_use]
pub fn wire_ids(questions: &[Question]) -> Vec<String> {
    let names: HashSet<&str> = questions.iter().filter_map(|q| q.name.as_deref()).collect();
    let mut used: HashSet<String> = HashSet::new();
    questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let id = if let Some(name) = &q.name {
                name.clone()
            } else {
                let mut id = format!("q{i}");
                while names.contains(id.as_str()) || used.contains(&id) {
                    id.insert(0, '_');
                }
                id
            };
            used.insert(id.clone());
            id
        })
        .collect()
}

fn translation(msg: impl Into<String>) -> ProviderError {
    ProviderError::Translation(msg.into())
}

/// Encode `req` for a TypeSafe-family upstream.
///
/// # Errors
/// [`ProviderError::Translation`] if serialization fails or images reach a
/// profile that takes none (the router's `check` prevents the latter).
pub fn encode(
    req: &DecisionRequest,
    upstream_id: &str,
    profile: &FamilyProfile,
    ids: &[String],
) -> Result<Vec<u8>, ProviderError> {
    if req.has_images() && profile.images == ImagePlacement::None {
        return Err(translation(format!(
            "{}: images are not supported",
            profile.kind
        )));
    }
    serde_json::to_vec(&WireRequest {
        req,
        upstream_id,
        profile,
        ids,
    })
    .map_err(|e| translation(format!("{} decisions request: {e}", profile.kind)))
}

struct WireRequest<'a> {
    req: &'a DecisionRequest,
    upstream_id: &'a str,
    profile: &'a FamilyProfile,
    ids: &'a [String],
}

impl Serialize for WireRequest<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(None)?;
        map.serialize_entry("model", self.upstream_id)?;
        map.serialize_entry("state", &State(self.req.input(), self.profile.images))?;
        map.serialize_entry("questions", &Questions(self.req.questions(), self.ids))?;
        if let (Input::Messages(parts), ImagePlacement::TopLevel { raw_base64 }) =
            (self.req.input(), self.profile.images)
        {
            if parts.iter().any(|p| matches!(p, Part::Image(_))) {
                map.serialize_entry("images", &Images(parts, raw_base64))?;
            }
        }
        if self.profile.forward_unknown_fields {
            for (key, value) in self.req.extra() {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

/// Text parts joined with a blank line (spec 7.1).
fn joined_text(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.as_prompt().into_owned()),
            Part::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

struct State<'a>(&'a Input, ImagePlacement);

impl Serialize for State<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match (self.0, self.1) {
            (Input::Text(text), _) => s.serialize_str(text),
            (Input::Structured(raw), _) => raw.serialize(s),
            (Input::Messages(parts), ImagePlacement::StateParts)
                if parts.iter().any(|p| matches!(p, Part::Image(_))) =>
            {
                let mut seq = s.serialize_seq(Some(parts.len()))?;
                for part in parts {
                    match part {
                        Part::Text(t) => seq.serialize_element(&TextOut(t))?,
                        Part::Image(img) => seq.serialize_element(&ImageUrlPart(&img.data_url))?,
                    }
                }
                seq.end()
            }
            (Input::Messages(parts), _) => s.serialize_str(&joined_text(parts)),
        }
    }
}

struct ImageUrlPart<'a>(&'a str);

impl Serialize for ImageUrlPart<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Url<'a> {
            url: &'a str,
        }
        let mut map = s.serialize_map(Some(2))?;
        map.serialize_entry("type", "image_url")?;
        map.serialize_entry("image_url", &Url { url: self.0 })?;
        map.end()
    }
}

struct Images<'a>(&'a [Part], bool);

impl Serialize for Images<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(None)?;
        for part in self.0 {
            if let Part::Image(img) = part {
                let url = img.data_url.as_str();
                let value = if self.1 {
                    url.split_once(";base64,").map_or(url, |(_, b64)| b64)
                } else {
                    url
                };
                seq.serialize_element(value)?;
            }
        }
        seq.end()
    }
}

/// A [`Text`] as JSON: plain strings as strings, structured JSON verbatim.
struct TextOut<'a>(&'a Text);

impl Serialize for TextOut<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Text::Plain(text) => s.serialize_str(text),
            Text::Json(raw) => raw.serialize(s),
        }
    }
}

struct Questions<'a>(&'a [Question], &'a [String]);

impl Serialize for Questions<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for (q, id) in self.0.iter().zip(self.1) {
            match &q.raw {
                Some(raw) => map.serialize_entry(id, raw)?,
                None => map.serialize_entry(id, &QuestionOut(q))?,
            }
        }
        map.end()
    }
}

/// The TypeSafe wire body of one synthesized question (without its id), as
/// [`encode`] sends it. The rerank remap packs calls by its estimated
/// tokens, exactly as Jev's rerank always did (D7).
///
/// # Errors
/// [`ProviderError::Translation`] if serialization fails.
pub fn question_body(q: &Question) -> Result<Vec<u8>, ProviderError> {
    serde_json::to_vec(&QuestionOut(q)).map_err(|e| translation(format!("decisions question: {e}")))
}

/// A synthesized question, keys in TypeSafe order: `type`, `instructions`,
/// `criteria` (the order today's Jev rerank sends, D7).
struct QuestionOut<'a>(&'a Question);

impl Serialize for QuestionOut<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let q = self.0;
        let mut map = s.serialize_map(None)?;
        let kind = match q.kind {
            QuestionKind::Predicate { .. } => "noul",
            QuestionKind::Choice { .. } => "choice",
            QuestionKind::Score { .. } => "score",
        };
        map.serialize_entry("type", kind)?;
        if let Some(instructions) = &q.instructions {
            map.serialize_entry("instructions", &TextOut(instructions))?;
        }
        match &q.kind {
            QuestionKind::Predicate { criteria: Some(c) } => {
                map.serialize_entry("criteria", &Criteria(c))?;
            }
            QuestionKind::Predicate { criteria: None } => {}
            QuestionKind::Choice { choices } => {
                map.serialize_entry("criteria", &ChoiceCriteria(choices))?;
            }
            QuestionKind::Score { levels } => {
                map.serialize_entry("criteria", &LevelCriteria(levels))?;
            }
        }
        map.end()
    }
}

struct Criteria<'a>(&'a lumen_core::decisions::PredicateCriteria);

impl Serialize for Criteria<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(None)?;
        if let Some(t) = &self.0.when_true {
            map.serialize_entry("true", &TextOut(t))?;
        }
        if let Some(f) = &self.0.when_false {
            map.serialize_entry("false", &TextOut(f))?;
        }
        map.end()
    }
}

struct ChoiceCriteria<'a>(&'a [lumen_core::decisions::ChoiceOption]);

impl Serialize for ChoiceCriteria<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for option in self.0 {
            match &option.description {
                Some(d) => map.serialize_entry(&option.value.wire_key(), &TextOut(d))?,
                None => map.serialize_entry(&option.value.wire_key(), &())?,
            }
        }
        map.end()
    }
}

struct LevelCriteria<'a>(&'a [lumen_core::decisions::Level]);

impl Serialize for LevelCriteria<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for level in self.0 {
            match &level.description {
                None => seq.serialize_element(&TextOut(&level.label))?,
                Some(d) => seq.serialize_element(&format!(
                    "{}: {}",
                    level.label.as_prompt(),
                    d.as_prompt()
                ))?,
            }
        }
        seq.end()
    }
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

#[derive(Deserialize)]
struct WireAnswer {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    noul: Option<f64>,
    #[serde(default)]
    choice: Option<serde_json::Value>,
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    probabilities: Option<std::collections::HashMap<String, f64>>,
    #[serde(default)]
    confidence: Option<f64>,
}

#[derive(Deserialize)]
struct CloudflareEnvelope {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    result: Option<Box<RawValue>>,
}

/// Decode a TypeSafe-family response into answers in request order.
///
/// # Errors
/// [`ProviderError::Translation`] (`LM-3002`) for a malformed body, a
/// missing or extra answer, an answer whose type does not match its
/// question, or a choice outside the request's options;
/// [`ProviderError::Upstream`] (`LM-3003`, not retryable) for a Cloudflare
/// `success: false`.
pub fn decode(
    bytes: &[u8],
    req: &DecisionRequest,
    ids: &[String],
    profile: &FamilyProfile,
    provider: &str,
) -> Result<DecisionResponse, ProviderError> {
    let kind = profile.kind;
    let body: std::borrow::Cow<'_, [u8]> = match profile.envelope {
        Envelope::Bare => std::borrow::Cow::Borrowed(bytes),
        Envelope::Cloudflare => {
            let env: CloudflareEnvelope = serde_json::from_slice(bytes)
                .map_err(|_| translation(format!("{kind} envelope is malformed")))?;
            match (env.success, env.result) {
                (true, Some(result)) => std::borrow::Cow::Owned(result.get().as_bytes().to_vec()),
                // Workers AI's `errors[].message` stays out of the client
                // response (it is upstream text): only the status surfaces.
                _ => {
                    return Err(ProviderError::Upstream {
                        provider: provider.to_owned(),
                        status: 200,
                        retryable: false,
                    })
                }
            }
        }
    };
    let entries = object_entries(&body, "response")
        .map_err(|e| translation(format!("{kind} response: {e}")))?;
    let mut model = None;
    let mut usage = None;
    let mut answers_raw = None;
    for (key, value) in &entries {
        match key.as_str() {
            "model" => model = serde_json::from_str::<String>(value.get()).ok(),
            "usage" => {
                usage = serde_json::from_str::<WireUsage>(value.get())
                    .ok()
                    .map(|u| DecisionUsage {
                        input_tokens: u.input_tokens,
                        output_tokens: u.output_tokens,
                        ..DecisionUsage::default()
                    });
            }
            "answers" => answers_raw = Some(value),
            _ => {}
        }
    }
    let answers_raw =
        answers_raw.ok_or_else(|| translation(format!("{kind} response: missing `answers`")))?;
    let mut by_id: std::collections::HashMap<String, Box<RawValue>> =
        serde_json::from_str(answers_raw.get())
            .map_err(|_| translation(format!("{kind} `answers` is not an object of answers")))?;
    let mut answers = Vec::with_capacity(ids.len());
    for (i, (q, id)) in req.questions().iter().zip(ids).enumerate() {
        let raw = by_id
            .remove(id)
            .ok_or_else(|| translation(format!("{kind} response: no answer for question #{i}")))?;
        answers.push(decode_answer(&raw, q, i, kind)?);
    }
    if !by_id.is_empty() {
        return Err(translation(format!(
            "{kind} response: {} unexpected answers",
            by_id.len()
        )));
    }
    Ok(DecisionResponse {
        model: model.unwrap_or_else(|| req.model.clone()),
        answers,
        usage,
        upstream: Some(entries),
    })
}

fn decode_answer(
    raw: &RawValue,
    q: &Question,
    i: usize,
    kind: &str,
) -> Result<Answer, ProviderError> {
    let bad = |what: &str| translation(format!("{kind} answer #{i}: {what}"));
    let a: WireAnswer = serde_json::from_str(raw.get()).map_err(|_| bad("malformed answer"))?;
    match (a.kind.as_str(), &q.kind) {
        ("refusal", _) => Ok(Answer::Refusal),
        ("noul", QuestionKind::Predicate { .. }) => Ok(Answer::Predicate {
            probability: a.noul.ok_or_else(|| bad("missing `noul`"))?,
        }),
        ("choice", QuestionKind::Choice { choices }) => {
            let spelled = match a.choice.ok_or_else(|| bad("missing `choice`"))? {
                serde_json::Value::String(s) => s,
                serde_json::Value::Bool(b) => b.to_string(),
                _ => return Err(bad("`choice` must be a string")),
            };
            let choice = choices
                .iter()
                .find(|c| c.value.wire_key() == spelled)
                .ok_or_else(|| bad("`choice` is not one of the request's options"))?
                .value
                .clone();
            let probs = a.probabilities.unwrap_or_default();
            let probabilities = choices
                .iter()
                .map(|c| {
                    (
                        c.value.clone(),
                        probs
                            .get(c.value.wire_key().as_ref())
                            .copied()
                            .unwrap_or(0.0),
                    )
                })
                .collect();
            Ok(Answer::Choice {
                choice,
                probabilities,
                confidence: a.confidence,
            })
        }
        ("score", QuestionKind::Score { levels }) => {
            let mut probabilities: Vec<(usize, f64)> = a
                .probabilities
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(k, p)| {
                    k.parse::<usize>()
                        .ok()
                        .filter(|i| *i < levels.len())
                        .map(|i| (i, p))
                })
                .collect();
            probabilities.sort_unstable_by_key(|(i, _)| *i);
            Ok(Answer::Score {
                score: a.score.ok_or_else(|| bad("missing `score`"))?,
                probabilities,
                confidence: a.confidence,
            })
        }
        _ => Err(bad("type does not match the question")),
    }
}

/// Default TypeSafe API base.
pub const TYPESAFE_DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";

/// Where a family provider POSTs.
#[derive(Clone)]
enum Endpoint {
    /// One URL for every model.
    Fixed(String),
    /// `{prefix}{upstream_id}` (Cloudflare `/ai/run/@cf/cloudflare/{id}`).
    PerModel(String),
}

/// A TypeSafe-format decision provider (TypeSafe, Perplexity, Ollama,
/// Cloudflare, Liquid, Inception, Upstage, Kev).
pub struct FamilyDecisionProvider {
    client: reqwest::Client,
    provider_name: String,
    endpoint: Endpoint,
    /// Bearer key; redacted from `Debug`, never logged.
    api_key: Option<String>,
    profile: FamilyProfile,
}

// The constructors take the owned config values the registry holds, per the
// task interface; the by-value `base_url` is only read.
#[allow(clippy::needless_pass_by_value)]
impl FamilyDecisionProvider {
    /// The `typesafe` kind. `base_url` defaults to the public TypeSafe API.
    /// Without `decisions_path` the URL is `{base}/v1/systemone` (a trailing
    /// `/v1` on `base_url` is tolerated, as before); with it,
    /// `{base}{decisions_path}` verbatim (generic TypeSafe-format vendors).
    #[must_use]
    pub fn typesafe(
        client: reqwest::Client,
        name: impl Into<String>,
        base_url: Option<String>,
        decisions_path: Option<String>,
        forward_unknown_fields: bool,
        api_key: Option<String>,
    ) -> Self {
        let base = base_url.unwrap_or_else(|| TYPESAFE_DEFAULT_BASE_URL.to_owned());
        let base = base.trim_end_matches('/');
        let url = match decisions_path {
            Some(path) => format!("{base}{path}"),
            None => format!("{}/v1/systemone", base.strip_suffix("/v1").unwrap_or(base)),
        };
        Self::build(
            client,
            name,
            Endpoint::Fixed(url),
            api_key,
            FamilyProfile::typesafe(forward_unknown_fields),
        )
    }

    /// The `perplexity` kind: `{base}/v1/decisions`.
    #[must_use]
    pub fn perplexity(
        client: reqwest::Client,
        name: impl Into<String>,
        base_url: String,
        api_key: Option<String>,
    ) -> Self {
        let base = base_url.trim_end_matches('/');
        let base = base.strip_suffix("/v1").unwrap_or(base);
        Self::build(
            client,
            name,
            Endpoint::Fixed(format!("{base}/v1/decisions")),
            api_key,
            FamilyProfile::perplexity(),
        )
    }

    /// The `ollama` kind: `{root}/v1/systemone` (Ollama 0.35+).
    #[must_use]
    pub fn ollama(
        client: reqwest::Client,
        name: impl Into<String>,
        base_url: String,
        api_key: Option<String>,
    ) -> Self {
        let root = base_url.trim_end_matches('/');
        Self::build(
            client,
            name,
            Endpoint::Fixed(format!("{root}/v1/systemone")),
            api_key,
            FamilyProfile::ollama(),
        )
    }

    /// The `cloudflare` kind: `{account root}/ai/run/@cf/cloudflare/{model}`.
    #[must_use]
    pub fn cloudflare(
        client: reqwest::Client,
        name: impl Into<String>,
        base_url: String,
        api_key: Option<String>,
    ) -> Self {
        let root = crate::cloudflare::account_root(base_url.trim_end_matches('/')).to_owned();
        Self::build(
            client,
            name,
            Endpoint::PerModel(format!("{root}/ai/run/@cf/cloudflare/")),
            api_key,
            FamilyProfile::cloudflare(),
        )
    }

    fn build(
        client: reqwest::Client,
        name: impl Into<String>,
        endpoint: Endpoint,
        api_key: Option<String>,
        profile: FamilyProfile,
    ) -> Self {
        Self {
            client,
            provider_name: name.into(),
            endpoint,
            api_key,
            profile,
        }
    }

    fn url(&self, upstream_id: &str) -> String {
        match &self.endpoint {
            Endpoint::Fixed(url) => url.clone(),
            Endpoint::PerModel(prefix) => format!("{prefix}{upstream_id}"),
        }
    }
}

/// Redacted so the API key can never reach a log line via `{:?}`.
impl std::fmt::Debug for FamilyDecisionProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FamilyDecisionProvider")
            .field("provider_name", &self.provider_name)
            .field("kind", &self.profile.kind)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl DecisionProvider for FamilyDecisionProvider {
    async fn decide(
        &self,
        req: DecisionRequest,
        cancel: CancellationToken,
    ) -> Result<DecisionResponse, ProviderError> {
        let ids = wire_ids(req.questions());
        let body = encode(&req, &req.model, &self.profile, &ids)?;
        let bytes = post_json_bytes(
            &self.client,
            &self.url(&req.model),
            body,
            self.api_key.as_deref(),
            &self.provider_name,
            &cancel,
        )
        .await?;
        decode(&bytes, &req, &ids, &self.profile, &self.provider_name)
    }

    fn limits(&self) -> &DecisionLimits {
        &self.profile.limits
    }

    fn provider_name(&self) -> &str {
        &self.provider_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_core::decisions::format::{parse, Format};
    use lumen_core::decisions::ChoiceValue;

    fn ts(body: &str) -> DecisionRequest {
        parse(body.as_bytes(), Some(Format::TypeSafe)).unwrap().1
    }

    fn oa(body: &str) -> DecisionRequest {
        parse(body.as_bytes(), None).unwrap().1
    }

    fn enc(req: &DecisionRequest, profile: &FamilyProfile) -> String {
        let ids = wire_ids(req.questions());
        String::from_utf8(encode(req, "up", profile, &ids).unwrap()).unwrap()
    }

    #[test]
    fn typesafe_passthrough_is_byte_identical_to_the_golden_request() {
        const PASSTHROUGH: &str = r#"{"model":"jev","state":{"zeta":"café\n","alpha":[1,2]},"questions":{"urgent":{"type":"noul","instructions":{"q":"urgent?"},"criteria":{"true":"yes"}},"team":{"type":"choice","instructions":"Which team?","criteria":{"tech":"Bugs","billing":null}},"mood":{"type":"score","instructions":"Mood?","criteria":["calm","angry"]}},"future_flag":true}"#;
        let req = ts(PASSTHROUGH);
        let ids = wire_ids(req.questions());
        let bytes = encode(&req, "jev-latest", &FamilyProfile::typesafe(true), &ids).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            include_str!("../../tests/fixtures/decisions/passthrough_request.json")
        );
    }

    #[test]
    fn unknown_fields_are_stripped_unless_forwarded() {
        let req = ts(
            r#"{"model":"p","state":"s","questions":{"q":{"type":"noul","instructions":"i"}},"x":1}"#,
        );
        assert!(enc(&req, &FamilyProfile::typesafe(true)).ends_with(r#","x":1}"#));
        assert!(!enc(&req, &FamilyProfile::typesafe(false)).contains(r#""x""#));
        assert!(!enc(&req, &FamilyProfile::perplexity()).contains(r#""x""#));
    }

    #[test]
    fn openai_questions_translate_to_typesafe_wire() {
        let req = oa(r#"{"model":"m","input":"hello","questions":[
            {"type":"predicate","instructions":"urgent?"},
            {"type":"choice","name":"team","instructions":"which?","choices":[{"value":"billing","description":"Payments"},{"value":false}]},
            {"type":"score","name":"mood","instructions":"how?","levels":[{"label":"calm"},{"label":"angry","description":"shouting"}]}]}"#);
        assert_eq!(
            enc(&req, &FamilyProfile::perplexity()),
            concat!(
                r#"{"model":"up","state":"hello","questions":{"#,
                r#""q0":{"type":"noul","instructions":"urgent?"},"#,
                r#""team":{"type":"choice","instructions":"which?","criteria":{"billing":"Payments","false":null}},"#,
                r#""mood":{"type":"score","instructions":"how?","criteria":["calm","angry: shouting"]}}}"#
            )
        );
    }

    #[test]
    fn generated_ids_avoid_client_names() {
        let req = oa(r#"{"model":"m","input":"x","questions":[
            {"type":"predicate","instructions":"a"},
            {"type":"predicate","name":"q0","instructions":"b"},
            {"type":"predicate","name":"_q0","instructions":"c"}]}"#);
        let ids = wire_ids(req.questions());
        assert_eq!(ids, ["__q0", "q0", "_q0"]);
    }

    #[test]
    fn images_go_where_the_profile_says() {
        let req = oa(r#"{"model":"m","input":[{"role":"user","content":[
            {"type":"input_text","text":"one"},
            {"type":"input_image","image_url":"data:image/png;base64,QUJD"},
            {"type":"input_text","text":"two"}]}],
            "questions":[{"type":"predicate","instructions":"i"}]}"#);
        assert!(enc(&req, &FamilyProfile::perplexity()).contains(
            r#""state":["one",{"type":"image_url","image_url":{"url":"data:image/png;base64,QUJD"}},"two"]"#));
        let ollama = enc(&req, &FamilyProfile::ollama());
        assert!(ollama.contains(r#""state":"one\n\ntwo""#), "{ollama}");
        assert!(ollama.ends_with(r#""images":["QUJD"]}"#), "{ollama}");
        assert!(enc(&req, &FamilyProfile::cloudflare())
            .ends_with(r#""images":["data:image/png;base64,QUJD"]}"#));
    }

    #[test]
    fn perplexity_profile_rejects_129_questions_but_typesafe_accepts() {
        let qs: Vec<String> = (0..129)
            .map(|i| format!(r#""q{i}":{{"type":"noul","instructions":"i"}}"#))
            .collect();
        let req = ts(&format!(
            r#"{{"model":"p","state":"s","questions":{{{}}}}}"#,
            qs.join(",")
        ));
        match FamilyProfile::perplexity().limits.check(&req, "pplx") {
            Err(lumen_core::error::GatewayError::InvalidRequest(m)) => {
                assert!(m.contains("pplx") && m.contains("129"), "{m}");
            }
            other => panic!("expected LM-1001, got {other:?}"),
        }
        assert!(FamilyProfile::typesafe(true)
            .limits
            .check(&req, "jev")
            .is_ok());
    }

    #[test]
    fn perplexity_state_parts_pass_through_raw() {
        let req = ts(
            r#"{"model":"p","state":["café",{"type":"image_url","image_url":{"url":"data:image/png;base64,AA"}}],"questions":{"q":{"type":"noul","instructions":"i"}}}"#,
        );
        assert!(enc(&req, &FamilyProfile::perplexity()).contains(r#""state":["café","#));
    }

    const ANSWERS: &str = r#"{"model":"jev-1.13.0","answers":{
        "u":{"type":"noul","noul":0.9},
        "t":{"type":"choice","choice":"false","probabilities":{"false":0.6,"billing":0.4},"confidence":0.7},
        "m":{"type":"score","score":1.4,"legend":{"0":"calm","1":"angry"},"probabilities":{"1":0.7,"0":0.3},"confidence":0.5}},
        "usage":{"input_tokens":12,"output_tokens":3},"request_id":"r"}"#;

    fn three() -> DecisionRequest {
        oa(r#"{"model":"m","input":"x","questions":[
            {"type":"predicate","name":"u","instructions":"i"},
            {"type":"choice","name":"t","instructions":"i","choices":[{"value":"billing"},{"value":false}]},
            {"type":"score","name":"m","instructions":"i","levels":[{"label":"calm"},{"label":"angry"}]}]}"#)
    }

    #[test]
    fn decode_maps_answers_back_to_request_order_and_types() {
        let req = three();
        let ids = wire_ids(req.questions());
        let resp = decode(
            ANSWERS.as_bytes(),
            &req,
            &ids,
            &FamilyProfile::typesafe(true),
            "typesafe",
        )
        .unwrap();
        assert_eq!(resp.model, "jev-1.13.0");
        assert!(
            matches!(resp.answers[0], Answer::Predicate { probability } if (probability - 0.9).abs() < 1e-9)
        );
        match &resp.answers[1] {
            Answer::Choice {
                choice,
                probabilities,
                confidence,
            } => {
                assert!(*choice == ChoiceValue::Bool(false));
                assert!(probabilities[0].0 == ChoiceValue::Str("billing".into()));
                assert!((probabilities[1].1 - 0.6).abs() < 1e-9);
                assert_eq!(*confidence, Some(0.7));
            }
            other => panic!("{other:?}"),
        }
        match &resp.answers[2] {
            Answer::Score { probabilities, .. } => assert_eq!(
                probabilities.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
                [0, 1]
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(resp.usage.unwrap().input_tokens, 12);
        let upstream = resp.upstream.unwrap();
        assert_eq!(upstream.last().unwrap().0, "request_id");
    }

    #[test]
    fn missing_extra_or_mistyped_answers_are_translation_errors() {
        let req = three();
        let ids = wire_ids(req.questions());
        let p = FamilyProfile::typesafe(true);
        let missing = r#"{"model":"j","answers":{"u":{"type":"noul","noul":0.9}}}"#;
        let extra = ANSWERS.replace(
            r#""u":{"type":"noul","noul":0.9},"#,
            r#""u":{"type":"noul","noul":0.9},"zz":{"type":"noul","noul":0.1},"#,
        );
        let mistyped = ANSWERS.replace(
            r#""u":{"type":"noul","noul":0.9}"#,
            r#""u":{"type":"score","score":1}"#,
        );
        let unknown_choice = ANSWERS.replace(r#""choice":"false""#, r#""choice":"nope""#);
        for body in [missing.to_owned(), extra, mistyped, unknown_choice] {
            assert!(
                matches!(
                    decode(body.as_bytes(), &req, &ids, &p, "typesafe"),
                    Err(ProviderError::Translation(_))
                ),
                "{body}"
            );
        }
    }

    #[test]
    fn usage_is_lenient() {
        let req = oa(
            r#"{"model":"m","input":"x","questions":[{"type":"predicate","name":"u","instructions":"i"}]}"#,
        );
        let ids = wire_ids(req.questions());
        let p = FamilyProfile::typesafe(true);
        for usage in ["", r#","usage":null"#, r#","usage":"bad""#] {
            let body =
                format!(r#"{{"model":"j","answers":{{"u":{{"type":"noul","noul":0.5}}}}{usage}}}"#);
            assert!(
                decode(body.as_bytes(), &req, &ids, &p, "t")
                    .unwrap()
                    .usage
                    .is_none(),
                "{usage}"
            );
        }
    }

    #[test]
    fn cloudflare_envelope_is_unwrapped_and_failure_is_an_upstream_error() {
        let req = oa(
            r#"{"model":"m","input":"x","questions":[{"type":"predicate","name":"u","instructions":"i"}]}"#,
        );
        let ids = wire_ids(req.questions());
        let p = FamilyProfile::cloudflare();
        let ok = r#"{"result":{"model":"clef","answers":{"u":{"type":"noul","noul":0.2}}},"success":true,"errors":[],"messages":[]}"#;
        assert_eq!(
            decode(ok.as_bytes(), &req, &ids, &p, "cf").unwrap().model,
            "clef"
        );
        let bad = r#"{"result":null,"success":false,"errors":[{"code":5006,"message":"SECRET-ish upstream text"}],"messages":[]}"#;
        match decode(bad.as_bytes(), &req, &ids, &p, "cf") {
            Err(ProviderError::Upstream {
                provider,
                retryable: false,
                ..
            }) => assert_eq!(provider, "cf"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn decode_errors_do_not_quote_answer_content() {
        let req = oa(
            r#"{"model":"m","input":"x","questions":[{"type":"predicate","name":"u","instructions":"i"}]}"#,
        );
        let ids = wire_ids(req.questions());
        let p = FamilyProfile::typesafe(true);
        for body in [
            r#"{"model":"j","answers":{"u":{"type":"noul","noul":"LEAKME"}}}"#,
            r#"{"model":"j","answers":{"u":"LEAKME"}}"#,
            r#"{"model":"j","answers":{"u":{"type":"LEAKME"}}}"#,
            r#"{"model":"j","answers":"LEAKME"}"#,
        ] {
            let err = decode(body.as_bytes(), &req, &ids, &p, "t").unwrap_err();
            assert!(!err.to_string().contains("LEAKME"), "{err}");
        }
    }

    #[test]
    fn profiles_carry_the_spec_limits() {
        assert!(
            FamilyProfile::typesafe(true)
                .limits
                .predicate_needs_instructions
        );
        assert_eq!(FamilyProfile::typesafe(true).limits.max_images, Some(0));
        assert_eq!(FamilyProfile::perplexity().limits.max_questions, Some(128));
        assert_eq!(FamilyProfile::cloudflare().limits.max_questions, Some(64));
        assert_eq!(FamilyProfile::cloudflare().limits.max_images, Some(4));
        assert_eq!(
            FamilyProfile::cloudflare().limits.pack.max_docs_per_call,
            64
        );
        let o = FamilyProfile::ollama().limits;
        assert_eq!((o.min_choice_options, o.max_choice_options), (2, 26));
        assert_eq!((o.min_score_levels, o.max_score_levels), (2, 26));
        assert_eq!((o.pack.max_call_tokens, o.pack.concurrency), (16_000, 1));
    }
}
