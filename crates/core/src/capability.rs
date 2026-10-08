//! Model capabilities.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A capability that a model (and the provider backing it) can serve.
///
/// A single provider may implement one to four of the capability traits;
/// the router dispatches by `(capability, model)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    /// Chat / text completion (`POST /v1/chat/completions`).
    Chat,
    /// Text embeddings (`POST /v1/embeddings`).
    Embed,
    /// Document reranking (`POST /v1/rerank`).
    Rerank,
    /// Typed decisions over an input (`POST /v1/decisions`, ADR 016).
    /// `"systemone"` (ADR 013) is accepted as an alias.
    #[serde(rename = "decisions", alias = "systemone")]
    Decisions,
}

impl Capability {
    /// The stable string identifier exposed in `GET /v1/models`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Capability::Chat => "chat",
            Capability::Embed => "embed",
            Capability::Rerank => "rerank",
            Capability::Decisions => "decisions",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_serializes_as_decisions_and_accepts_the_systemone_alias() {
        assert_eq!(Capability::Decisions.as_str(), "decisions");
        assert_eq!(
            serde_json::to_string(&Capability::Decisions).unwrap(),
            "\"decisions\""
        );
        let old: Capability = serde_json::from_str("\"systemone\"").unwrap();
        let new: Capability = serde_json::from_str("\"decisions\"").unwrap();
        assert_eq!(old, Capability::Decisions);
        assert_eq!(new, Capability::Decisions);
    }
}
