//! The set of built-in provider implementations.

use serde::{Deserialize, Serialize};

/// Selects which built-in provider implementation backs a configured provider.
///
/// An unknown `kind` in the config is a hard error at load time (the config
/// deserializes into this enum with `deny_unknown_fields` upstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    // --- Native integrations (own request/response translation) --------------
    Openai,
    Anthropic,
    Cohere,
    Ollama,
    Tei,
    Jina,
    Voyage,
    Mistral,
    Google,
    /// Azure OpenAI: deployment-routed URLs + `api-version`, `api-key` auth.
    /// Not OpenAI-compatible (own URL scheme), so it is NOT part of
    /// [`is_openai_compatible`](ProviderKind::is_openai_compatible).
    Azure,
    /// Google Vertex AI (regional endpoints, GCP service-account OAuth). Distinct
    /// from `Google`, which is the public Gemini Developer API.
    VertexAi,
    // --- OpenAI-compatible hosts (served by the OpenAI provider with a
    //     per-kind base URL; chat + embeddings). ------------------------------
    Groq,
    Together,
    Fireworks,
    Deepseek,
    Openrouter,
    Perplexity,
    Xai,
    Deepinfra,
    /// Hugging Face Inference (the OpenAI-compatible router endpoint).
    Huggingface,
    /// Cloudflare Workers AI (OpenAI-compatible endpoint; `base_url` carries the
    /// account id, so it is required).
    Cloudflare,
    /// A self-hosted OpenAI-compatible server (vLLM, llama.cpp, LM Studio, …);
    /// `base_url` required, API key optional.
    Vllm,
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
            ProviderKind::Azure => "azure",
            ProviderKind::VertexAi => "vertex_ai",
            ProviderKind::Groq => "groq",
            ProviderKind::Together => "together",
            ProviderKind::Fireworks => "fireworks",
            ProviderKind::Deepseek => "deepseek",
            ProviderKind::Openrouter => "openrouter",
            ProviderKind::Perplexity => "perplexity",
            ProviderKind::Xai => "xai",
            ProviderKind::Deepinfra => "deepinfra",
            ProviderKind::Huggingface => "huggingface",
            ProviderKind::Cloudflare => "cloudflare",
            ProviderKind::Vllm => "vllm",
        }
    }

    /// Whether this kind is served by the OpenAI provider (OpenAI-compatible
    /// `/chat/completions` + `/embeddings`).
    #[must_use]
    pub const fn is_openai_compatible(self) -> bool {
        matches!(
            self,
            ProviderKind::Openai
                | ProviderKind::Groq
                | ProviderKind::Together
                | ProviderKind::Fireworks
                | ProviderKind::Deepseek
                | ProviderKind::Openrouter
                | ProviderKind::Perplexity
                | ProviderKind::Xai
                | ProviderKind::Deepinfra
                | ProviderKind::Huggingface
                | ProviderKind::Cloudflare
                | ProviderKind::Vllm
        )
    }

    /// The built-in base URL for OpenAI-compatible hosts, or `None` when the
    /// operator must supply one (self-hosted vLLM, or Cloudflare whose URL
    /// embeds the account id). `None` for native kinds (they own their URLs).
    #[must_use]
    pub const fn default_base_url(self) -> Option<&'static str> {
        match self {
            ProviderKind::Groq => Some("https://api.groq.com/openai/v1"),
            ProviderKind::Together => Some("https://api.together.xyz/v1"),
            ProviderKind::Fireworks => Some("https://api.fireworks.ai/inference/v1"),
            ProviderKind::Deepseek => Some("https://api.deepseek.com/v1"),
            ProviderKind::Openrouter => Some("https://openrouter.ai/api/v1"),
            ProviderKind::Perplexity => Some("https://api.perplexity.ai"),
            ProviderKind::Xai => Some("https://api.x.ai/v1"),
            ProviderKind::Deepinfra => Some("https://api.deepinfra.com/v1/openai"),
            ProviderKind::Huggingface => Some("https://router.huggingface.co/v1"),
            _ => None,
        }
    }

    /// Whether this provider requires an API key to be configured.
    ///
    /// Local, self-hosted providers (Ollama, TEI, vLLM) are keyless.
    #[must_use]
    pub const fn requires_api_key(self) -> bool {
        !matches!(
            self,
            ProviderKind::Ollama | ProviderKind::Tei | ProviderKind::Vllm
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn azure_is_a_native_kind_requiring_its_own_base_url_and_api_key() {
        // Azure's URL scheme (deployment routing + api-version) is not the
        // generic OpenAI-compatible path, so it must stay out of both the
        // "compatible" set and the built-in default-base-URL table - every
        // Azure resource endpoint is operator-specific.
        assert!(!ProviderKind::Azure.is_openai_compatible());
        assert_eq!(ProviderKind::Azure.default_base_url(), None);
        assert!(ProviderKind::Azure.requires_api_key());
        assert_eq!(ProviderKind::Azure.as_str(), "azure");
    }
}
