//! Model capabilities.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A capability that a model (and the provider backing it) can serve.
///
/// A single provider may implement one to three of the capability traits;
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
}

impl Capability {
    /// The stable string identifier exposed in `GET /v1/models`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Capability::Chat => "chat",
            Capability::Embed => "embed",
            Capability::Rerank => "rerank",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
