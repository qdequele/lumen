//! The set of built-in provider implementations.

use serde::{Deserialize, Serialize};

/// Selects which built-in provider implementation backs a configured provider.
///
/// An unknown `kind` in the config is a hard error at load time (the config
/// deserializes into this enum with `deny_unknown_fields` upstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Openai,
    Anthropic,
    Cohere,
    Ollama,
    Tei,
    Jina,
    Voyage,
    Mistral,
    Google,
}

impl ProviderKind {
    /// A stable, human-readable identifier for logs and errors.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Openai => "openai",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Cohere => "cohere",
            ProviderKind::Ollama => "ollama",
            ProviderKind::Tei => "tei",
            ProviderKind::Jina => "jina",
            ProviderKind::Voyage => "voyage",
            ProviderKind::Mistral => "mistral",
            ProviderKind::Google => "google",
        }
    }

    /// Whether this provider requires an API key to be configured.
    ///
    /// Local, self-hosted providers (Ollama, TEI) are keyless.
    #[must_use]
    pub const fn requires_api_key(self) -> bool {
        !matches!(self, ProviderKind::Ollama | ProviderKind::Tei)
    }
}
