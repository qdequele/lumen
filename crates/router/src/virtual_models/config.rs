//! Serde types of the `[[virtual_models]]` config array (ADR 014). Parsed by
//! the server's `Config`, validated and compiled by `RoutingTable::compile`.

use lumen_core::Capability;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::triggers::Trigger;

/// One `[[virtual_models]]` entry.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VirtualModelConfig {
    /// The id clients send, e.g. `acme/legal-rerank`.
    pub id: String,
    /// The one capability this virtual model serves.
    pub capability: Capability,
    /// How the targets are used.
    pub strategy: StrategyKind,
    /// Ordered targets.
    pub targets: Vec<TargetConfig>,
    /// Failures that move a `fallback` / `split` on; omitted = the defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_on: Option<Vec<Trigger>>,
    /// Chat only: stored system prompt and overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<PresetConfig>,
    /// Shown on `GET /v1/models`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `false` hides the model from `GET /v1/models` (still callable).
    #[serde(default = "default_listed", skip_serializing_if = "is_listed")]
    pub listed: bool,
}

const fn default_listed() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if signature
const fn is_listed(listed: &bool) -> bool {
    *listed
}

/// A virtual model's strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyKind {
    /// Exactly one target.
    Single,
    /// Targets in order.
    Fallback,
    /// One target by weight, then the others by descending weight.
    Split,
    /// The first target whose `when` matches; the last is the default.
    Switch,
}

impl StrategyKind {
    /// The config spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Fallback => "fallback",
            Self::Split => "split",
            Self::Switch => "switch",
        }
    }
}

/// One target of a virtual model.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    /// A foundation id or another virtual id.
    pub model: String,
    /// `split` only: integer weight > 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
    /// `switch` only: the condition table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<Map<String, Value>>,
    /// Request field overrides applied when this target is attempted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<OverridesConfig>,
    /// Rerank virtual models only: answer through a SystemOne model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remap: Option<RemapConfig>,
}

/// `overrides = { set = {...}, default = {...}, drop = [...] }`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OverridesConfig {
    /// Always applied.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub set: Map<String, Value>,
    /// Applied only when the client did not send the field.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub default: Map<String, Value>,
    /// Removed from the request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drop: Vec<String>,
}

/// A chat preset.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PresetConfig {
    /// Stored system prompt (at most 32 KiB, non-blank).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// How the prompt combines with client system messages.
    #[serde(default)]
    pub system_prompt_mode: SystemPromptMode,
    /// Overrides applied before any target override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<OverridesConfig>,
}

/// How a preset's system prompt combines with client system messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemPromptMode {
    /// Insert before any client system message.
    #[default]
    Prepend,
    /// Remove client system messages, then insert.
    Replace,
    /// Insert only when the client sent no system message.
    IfAbsent,
}

/// A SystemOne rerank remap on a target.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemapConfig {
    /// Question strategy.
    #[serde(default)]
    pub strategy: RemapStrategy,
    /// Static context added to the state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// `noul`, `score`, `choice`: the question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// `noul`: what yes and no mean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria: Option<CriteriaConfig>,
    /// `score`: levels, worst first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub levels: Option<Vec<String>>,
    /// `composite`: the weighted criteria.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub questions: Option<Vec<CompositeQuestionConfig>>,
}

/// Remap question strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemapStrategy {
    /// One noul per document.
    #[default]
    Noul,
    /// One graded score per document.
    Score,
    /// Several weighted nouls per document.
    Composite,
    /// One choice over all documents.
    Choice,
}

/// `criteria.true` / `criteria.false`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CriteriaConfig {
    /// What a yes means.
    #[serde(default, rename = "true", skip_serializing_if = "Option::is_none")]
    pub yes: Option<String>,
    /// What a no means.
    #[serde(default, rename = "false", skip_serializing_if = "Option::is_none")]
    pub no: Option<String>,
}

/// One weighted criterion of a `composite` remap.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompositeQuestionConfig {
    /// The question asked about each `document`.
    pub instructions: String,
    /// What yes and no mean (defaults to the generic relevance criteria).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria: Option<CriteriaConfig>,
    /// Weight in the mean, > 0.
    pub weight: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    struct Doc {
        virtual_models: Vec<VirtualModelConfig>,
    }

    #[test]
    fn a_full_virtual_model_parses_from_toml() {
        let doc: Doc = toml::from_str(
            r#"
            [[virtual_models]]
            id = "acme/legal-rerank"
            capability = "rerank"
            strategy = "fallback"
            fallback_on = ["rate_limited", "context_length"]
            description = "Legal reranker"
            targets = [
              { model = "jev", remap = { strategy = "composite", context = "US law", questions = [ { instructions = "on topic?", weight = 0.3, criteria.true = "y", criteria.false = "n" } ] } },
              { model = "rerank-english", overrides = { set = { top_n = 5 } } },
            ]
            "#,
        )
        .unwrap();
        let vm = &doc.virtual_models[0];
        assert_eq!(vm.capability, Capability::Rerank);
        assert_eq!(vm.strategy, StrategyKind::Fallback);
        assert!(vm.listed);
        let remap = vm.targets[0].remap.as_ref().unwrap();
        assert_eq!(remap.strategy, RemapStrategy::Composite);
        assert_eq!(
            remap.questions.as_ref().unwrap()[0]
                .criteria
                .as_ref()
                .unwrap()
                .yes
                .as_deref(),
            Some("y")
        );
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = toml::from_str::<Doc>(
            r#"
            [[virtual_models]]
            id = "a"
            capability = "chat"
            strategy = "single"
            targets = [{ model = "m", weigth = 3 }]
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("weigth"), "{err}");
    }
}
