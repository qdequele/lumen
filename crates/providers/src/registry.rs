//! Provider registry.
//!
//! Builds concrete provider instances from resolved specs and resolves
//! `(capability, model id)` to a provider plus the upstream model id to send.
//!
//! The inner table lives behind an [`ArcSwap`] so a future hot reload (M7) can
//! atomically swap the whole routing table without locking the request path.
//! API keys are resolved from the environment by the caller (the server) and
//! passed in already - the registry never reads env vars or holds config.

use arc_swap::ArcSwap;
use lumen_core::{Capability, ChatProvider, EmbeddingProvider, RerankProvider};
use std::collections::HashMap;
use std::sync::Arc;

use crate::anthropic::AnthropicProvider;
use crate::cohere::CohereProvider;
use crate::google::GoogleProvider;
use crate::jina::JinaProvider;
use crate::kind::ProviderKind;
use crate::mistral::MistralProvider;
use crate::ollama::OllamaProvider;
use crate::openai::OpenAiProvider;
use crate::tei::TeiProvider;
use crate::voyage::VoyageProvider;

/// A model exposed by a provider, with its upstream id and capabilities.
#[derive(Debug, Clone)]
pub struct ModelSpec {
    /// Client-facing model id.
    pub id: String,
    /// Upstream model id to send.
    pub upstream_id: String,
    /// Declared capabilities.
    pub capabilities: Vec<Capability>,
    /// Declared input modalities (e.g. `["text","image"]`).
    pub modalities: Vec<String>,
}

/// A provider instance to build. `api_key` is already resolved from the
/// environment (or, since M5, decrypted from the store) by the caller -
/// `None` for keyless providers.
#[derive(Clone)]
pub struct ProviderSpec {
    /// Unique provider name (used to attribute upstream errors).
    pub name: String,
    /// Which implementation backs it.
    pub kind: ProviderKind,
    /// Resolved API key value, or `None`.
    pub api_key: Option<String>,
    /// Base URL override.
    pub base_url: Option<String>,
    /// Models this provider serves.
    pub models: Vec<ModelSpec>,
}

// Manual Debug: the spec carries a RESOLVED key value, and a future
// `debug!(?spec)` must never be able to leak it (CLAUDE.md rule 5).
impl std::fmt::Debug for ProviderSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSpec")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("api_key", &self.api_key.as_ref().map(|_| "REDACTED"))
            .field("base_url", &self.base_url)
            .field("models", &self.models)
            .finish()
    }
}

/// Failure while building the registry.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// A provider that requires a base URL did not have one configured.
    #[error("provider '{name}' (kind '{kind}') requires a base_url")]
    MissingBaseUrl {
        /// The offending provider's name.
        name: String,
        /// Its kind, for the operator's benefit.
        kind: &'static str,
    },

    /// Two providers declared the same model id. The registry is the last line
    /// of defence: the server config validates this at boot, but any other
    /// caller (e.g. M7 hot reload building specs directly) must not be able to
    /// silently shadow a route. Names both conflicting providers.
    #[error(
        "duplicate model id '{id}': declared by both provider '{first_provider}' \
         and provider '{second_provider}'"
    )]
    DuplicateModelId {
        /// The colliding model id.
        id: String,
        /// The provider that first declared it.
        first_provider: String,
        /// The provider that redeclared it.
        second_provider: String,
    },
}

/// A resolved embedding route: the provider to call and the upstream model id.
#[derive(Clone)]
pub struct EmbeddingRoute {
    /// The provider serving the model.
    pub provider: Arc<dyn EmbeddingProvider>,
    /// The configured provider name (for attributing upstream errors).
    pub provider_name: String,
    /// The upstream model id to send (already alias-resolved).
    pub upstream_id: String,
}

impl std::fmt::Debug for EmbeddingRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingRoute")
            .field("provider_name", &self.provider_name)
            .field("upstream_id", &self.upstream_id)
            .field("provider", &"<dyn EmbeddingProvider>")
            .finish()
    }
}

/// A resolved rerank route: the provider to call and the upstream model id.
#[derive(Clone)]
pub struct RerankRoute {
    /// The provider serving the model.
    pub provider: Arc<dyn RerankProvider>,
    /// The configured provider name (for attributing upstream errors).
    pub provider_name: String,
    /// The upstream model id to send (already alias-resolved).
    pub upstream_id: String,
}

impl std::fmt::Debug for RerankRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RerankRoute")
            .field("provider_name", &self.provider_name)
            .field("upstream_id", &self.upstream_id)
            .field("provider", &"<dyn RerankProvider>")
            .finish()
    }
}

/// A resolved chat route: the provider to call and the upstream model id.
#[derive(Clone)]
pub struct ChatRoute {
    /// The provider serving the model.
    pub provider: Arc<dyn ChatProvider>,
    /// The configured provider name (for attributing upstream errors).
    pub provider_name: String,
    /// The upstream model id to send (already alias-resolved).
    pub upstream_id: String,
}

impl std::fmt::Debug for ChatRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatRoute")
            .field("provider_name", &self.provider_name)
            .field("upstream_id", &self.upstream_id)
            .field("provider", &"<dyn ChatProvider>")
            .finish()
    }
}

/// A secret-free summary of one exposed model, for `GET /v1/models`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedModelSummary {
    /// Client-facing model id.
    pub id: String,
    /// The provider that owns it.
    pub owned_by: String,
    /// Capabilities it exposes.
    pub capabilities: Vec<Capability>,
    /// Declared input modalities.
    pub modalities: Vec<String>,
}

#[derive(Default)]
struct Inner {
    /// model id -> chat route.
    chat: HashMap<String, ChatRoute>,
    /// model id -> embedding route.
    embedding: HashMap<String, EmbeddingRoute>,
    /// model id -> rerank route.
    rerank: HashMap<String, RerankRoute>,
    /// model id -> declared capabilities (all of them, even not-yet-served
    /// ones like chat). Lets the router tell "unknown model" apart from
    /// "known model, wrong capability".
    model_capabilities: HashMap<String, Vec<Capability>>,
    /// model id -> declared modalities.
    model_modalities: HashMap<String, Vec<String>>,
    /// Every exposed model, in configuration order, for `GET /v1/models`.
    models: Vec<LoadedModelSummary>,
}

/// Concrete provider instances built for one spec. A single provider type may
/// implement several capability traits; those are the same instance behind
/// distinct trait-object pointers.
struct BuiltProviders {
    chat: Option<Arc<dyn ChatProvider>>,
    embed: Option<Arc<dyn EmbeddingProvider>>,
    rerank: Option<Arc<dyn RerankProvider>>,
}

/// The process-wide provider registry.
pub struct Registry {
    inner: ArcSwap<Inner>,
    /// Shared HTTP client, retained for hot-reload rebuilds (M7).
    client: reqwest::Client,
}

impl Registry {
    /// Build the registry from provider specs, sharing `client` across all
    /// provider instances. Takes ownership of the spec set (a full description
    /// of the routing table), even though today it only borrows to construct.
    #[allow(clippy::needless_pass_by_value)]
    pub fn build(specs: Vec<ProviderSpec>, client: reqwest::Client) -> Result<Self, RegistryError> {
        let inner = build_inner(&specs, &client)?;
        Ok(Self {
            inner: ArcSwap::from_pointee(inner),
            client,
        })
    }

    /// Atomically replace the routing table (hot reload - M7).
    #[allow(clippy::needless_pass_by_value)]
    pub fn reload(&self, specs: Vec<ProviderSpec>) -> Result<(), RegistryError> {
        let inner = build_inner(&specs, &self.client)?;
        self.inner.store(Arc::new(inner));
        Ok(())
    }

    /// Resolve a model id to a chat route, if one serves it.
    #[must_use]
    pub fn chat_route(&self, model_id: &str) -> Option<ChatRoute> {
        self.inner.load().chat.get(model_id).cloned()
    }

    /// Resolve a model id to an embedding route, if one serves it.
    #[must_use]
    pub fn embedding_route(&self, model_id: &str) -> Option<EmbeddingRoute> {
        self.inner.load().embedding.get(model_id).cloned()
    }

    /// Resolve a model id to a rerank route, if one serves it.
    #[must_use]
    pub fn rerank_route(&self, model_id: &str) -> Option<RerankRoute> {
        self.inner.load().rerank.get(model_id).cloned()
    }

    /// Whether any provider declares this model id (for any capability).
    #[must_use]
    pub fn knows_model(&self, model_id: &str) -> bool {
        self.inner.load().model_capabilities.contains_key(model_id)
    }

    /// The capabilities declared for a model id, if known.
    #[must_use]
    pub fn capabilities(&self, model_id: &str) -> Option<Vec<Capability>> {
        self.inner.load().model_capabilities.get(model_id).cloned()
    }

    /// The modalities declared for a model id, if known.
    #[must_use]
    pub fn modalities(&self, model_id: &str) -> Option<Vec<String>> {
        self.inner.load().model_modalities.get(model_id).cloned()
    }

    /// Every exposed model, in configuration order (for `GET /v1/models`).
    #[must_use]
    pub fn list_models(&self) -> Vec<LoadedModelSummary> {
        self.inner.load().models.clone()
    }
}

fn build_inner(specs: &[ProviderSpec], client: &reqwest::Client) -> Result<Inner, RegistryError> {
    let mut inner = Inner::default();
    // model id -> the provider that first declared it, so a collision names both.
    let mut owner: HashMap<&str, &str> = HashMap::new();

    for spec in specs {
        // One instance per provider, shared across all of its models via `Arc`.
        let built = build_providers(spec, client)?;

        for model in &spec.models {
            if let Some(first) = owner.insert(model.id.as_str(), spec.name.as_str()) {
                return Err(RegistryError::DuplicateModelId {
                    id: model.id.clone(),
                    first_provider: first.to_owned(),
                    second_provider: spec.name.clone(),
                });
            }

            inner
                .model_capabilities
                .entry(model.id.clone())
                .or_default()
                .extend(model.capabilities.iter().copied());

            inner
                .model_modalities
                .entry(model.id.clone())
                .or_default()
                .extend(model.modalities.iter().cloned());

            inner.models.push(LoadedModelSummary {
                id: model.id.clone(),
                owned_by: spec.name.clone(),
                capabilities: model.capabilities.clone(),
                modalities: model.modalities.clone(),
            });

            if model.capabilities.contains(&Capability::Chat) {
                if let Some(provider) = &built.chat {
                    inner.chat.insert(
                        model.id.clone(),
                        ChatRoute {
                            provider: provider.clone(),
                            provider_name: spec.name.clone(),
                            upstream_id: model.upstream_id.clone(),
                        },
                    );
                } else {
                    warn_unsupported(spec, &model.id, "chat");
                }
            }

            if model.capabilities.contains(&Capability::Embed) {
                if let Some(provider) = &built.embed {
                    inner.embedding.insert(
                        model.id.clone(),
                        EmbeddingRoute {
                            provider: provider.clone(),
                            provider_name: spec.name.clone(),
                            upstream_id: model.upstream_id.clone(),
                        },
                    );
                } else {
                    warn_unsupported(spec, &model.id, "embed");
                }
            }

            if model.capabilities.contains(&Capability::Rerank) {
                if let Some(provider) = &built.rerank {
                    inner.rerank.insert(
                        model.id.clone(),
                        RerankRoute {
                            provider: provider.clone(),
                            provider_name: spec.name.clone(),
                            upstream_id: model.upstream_id.clone(),
                        },
                    );
                } else {
                    warn_unsupported(spec, &model.id, "rerank");
                }
            }
        }
    }

    Ok(inner)
}

/// Warn that a model declares a capability its provider kind cannot serve yet.
fn warn_unsupported(spec: &ProviderSpec, model_id: &str, capability: &str) {
    tracing::warn!(
        provider = %spec.name,
        kind = %spec.kind.as_str(),
        model = %model_id,
        capability,
        "model declares a capability this provider kind has no implementation \
         for yet; it will not resolve for that capability"
    );
}

/// Build the capability-provider instances for one spec.
// A flat per-kind dispatch table; length scales with the number of providers,
// not complexity. Splitting it would only scatter the mapping.
#[allow(clippy::too_many_lines)]
fn build_providers(
    spec: &ProviderSpec,
    client: &reqwest::Client,
) -> Result<BuiltProviders, RegistryError> {
    let require_base_url = || {
        spec.base_url.clone().ok_or(RegistryError::MissingBaseUrl {
            name: spec.name.clone(),
            kind: spec.kind.as_str(),
        })
    };

    match spec.kind {
        // OpenAI + every OpenAI-compatible host (Groq, Together, Fireworks,
        // DeepSeek, OpenRouter, Perplexity, xAI, DeepInfra, Hugging Face router,
        // Cloudflare Workers AI, self-hosted vLLM/llama.cpp/LM Studio) share the
        // OpenAI provider; only the base URL differs. The base is the explicit
        // override, else the kind's built-in default. Kinds with neither (vLLM,
        // Cloudflare - its URL carries the account id) must not silently fall
        // through to api.openai.com, so a missing URL is a build error.
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
        | ProviderKind::Vllm => {
            let base_url = spec
                .base_url
                .clone()
                .or_else(|| spec.kind.default_base_url().map(str::to_owned));
            if base_url.is_none() && spec.kind != ProviderKind::Openai {
                return Err(RegistryError::MissingBaseUrl {
                    name: spec.name.clone(),
                    kind: spec.kind.as_str(),
                });
            }
            let chat: Arc<dyn ChatProvider> = Arc::new(OpenAiProvider::new(
                client.clone(),
                spec.name.clone(),
                base_url.clone(),
                spec.api_key.clone(),
            ));
            // Same instance shape behind the embedding trait object.
            let embed: Arc<dyn EmbeddingProvider> = Arc::new(OpenAiProvider::new(
                client.clone(),
                spec.name.clone(),
                base_url,
                spec.api_key.clone(),
            ));
            Ok(BuiltProviders {
                chat: Some(chat),
                embed: Some(embed),
                rerank: None,
            })
        }
        ProviderKind::Mistral => {
            let provider = Arc::new(MistralProvider::new(
                client.clone(),
                spec.name.clone(),
                spec.base_url.clone(),
                spec.api_key.clone(),
            ));
            let chat: Arc<dyn ChatProvider> = provider.clone();
            let embed: Arc<dyn EmbeddingProvider> = provider;
            Ok(BuiltProviders {
                chat: Some(chat),
                embed: Some(embed),
                rerank: None,
            })
        }
        ProviderKind::Anthropic => {
            let chat: Arc<dyn ChatProvider> = Arc::new(AnthropicProvider::new(
                client.clone(),
                spec.name.clone(),
                spec.base_url.clone(),
                spec.api_key.clone(),
            ));
            Ok(BuiltProviders {
                chat: Some(chat),
                embed: None,
                rerank: None,
            })
        }
        ProviderKind::Ollama => {
            let base_url = require_base_url()?;
            let embed: Arc<dyn EmbeddingProvider> = Arc::new(OllamaProvider::new(
                client.clone(),
                spec.name.clone(),
                base_url,
            ));
            Ok(BuiltProviders {
                chat: None,
                embed: Some(embed),
                rerank: None,
            })
        }
        ProviderKind::Cohere => {
            let provider = Arc::new(CohereProvider::new(
                client.clone(),
                spec.name.clone(),
                spec.base_url.clone(),
                spec.api_key.clone(),
            ));
            let embed: Arc<dyn EmbeddingProvider> = provider.clone();
            let rerank: Arc<dyn RerankProvider> = provider;
            Ok(BuiltProviders {
                chat: None,
                embed: Some(embed),
                rerank: Some(rerank),
            })
        }
        ProviderKind::Jina => {
            let provider = Arc::new(JinaProvider::new(
                client.clone(),
                spec.name.clone(),
                spec.base_url.clone(),
                spec.api_key.clone(),
            ));
            let embed: Arc<dyn EmbeddingProvider> = provider.clone();
            let rerank: Arc<dyn RerankProvider> = provider;
            Ok(BuiltProviders {
                chat: None,
                embed: Some(embed),
                rerank: Some(rerank),
            })
        }
        ProviderKind::Tei => {
            let base_url = require_base_url()?;
            let provider = Arc::new(TeiProvider::new(
                client.clone(),
                spec.name.clone(),
                base_url,
                spec.api_key.clone(),
            ));
            let embed: Arc<dyn EmbeddingProvider> = provider.clone();
            let rerank: Arc<dyn RerankProvider> = provider;
            Ok(BuiltProviders {
                chat: None,
                embed: Some(embed),
                rerank: Some(rerank),
            })
        }
        ProviderKind::Voyage => {
            let provider = Arc::new(VoyageProvider::new(
                client.clone(),
                spec.name.clone(),
                spec.base_url.clone(),
                spec.api_key.clone(),
            ));
            let embed: Arc<dyn EmbeddingProvider> = provider.clone();
            let rerank: Arc<dyn RerankProvider> = provider;
            Ok(BuiltProviders {
                chat: None,
                embed: Some(embed),
                rerank: Some(rerank),
            })
        }
        ProviderKind::Google => {
            let chat: Arc<dyn ChatProvider> = Arc::new(GoogleProvider::new(
                client.clone(),
                spec.name.clone(),
                spec.base_url.clone(),
                spec.api_key.clone(),
            ));
            Ok(BuiltProviders {
                chat: Some(chat),
                embed: None,
                rerank: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_spec_debug_never_shows_the_key() {
        let s = spec(ProviderKind::Openai, "openai", None, Vec::new());
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("sk-test-xxx"), "leaked: {dbg}");
        assert!(dbg.contains("REDACTED"));
    }

    fn spec(
        kind: ProviderKind,
        name: &str,
        base_url: Option<&str>,
        models: Vec<ModelSpec>,
    ) -> ProviderSpec {
        ProviderSpec {
            name: name.to_owned(),
            kind,
            api_key: Some("sk-test-xxx".to_owned()),
            base_url: base_url.map(str::to_owned),
            models,
        }
    }

    fn model(id: &str, caps: &[Capability]) -> ModelSpec {
        ModelSpec {
            id: id.to_owned(),
            upstream_id: id.to_owned(),
            capabilities: caps.to_vec(),
            modalities: vec!["text".to_owned()],
        }
    }

    #[test]
    fn resolves_embedding_model_and_reports_capabilities() {
        let reg = Registry::build(
            vec![spec(
                ProviderKind::Openai,
                "openai",
                None,
                vec![
                    model("embed-1", &[Capability::Embed]),
                    model("chat-1", &[Capability::Chat]),
                ],
            )],
            reqwest::Client::new(),
        )
        .unwrap();

        assert!(reg.embedding_route("embed-1").is_some());
        // chat-only model is known but not embeddable.
        assert!(reg.embedding_route("chat-1").is_none());
        assert!(reg.knows_model("chat-1"));
        assert!(!reg.knows_model("does-not-exist"));
    }

    #[test]
    fn ollama_without_base_url_is_a_build_error() {
        let result = Registry::build(
            vec![spec(
                ProviderKind::Ollama,
                "ollama",
                None,
                vec![model("e", &[Capability::Embed])],
            )],
            reqwest::Client::new(),
        );
        assert!(matches!(result, Err(RegistryError::MissingBaseUrl { .. })));
    }

    #[test]
    fn openai_compatible_kinds_build_with_their_default_base_url() {
        // A hosted OpenAI-compatible kind resolves for chat + embed with no
        // base_url configured (its built-in default is used).
        for kind in [
            ProviderKind::Groq,
            ProviderKind::Together,
            ProviderKind::Fireworks,
            ProviderKind::Deepseek,
            ProviderKind::Openrouter,
            ProviderKind::Perplexity,
            ProviderKind::Xai,
            ProviderKind::Deepinfra,
            ProviderKind::Huggingface,
        ] {
            assert!(
                kind.default_base_url().is_some(),
                "{kind:?} needs a default"
            );
            let reg = Registry::build(
                vec![spec(
                    kind,
                    "p",
                    None,
                    vec![model("m", &[Capability::Chat, Capability::Embed])],
                )],
                reqwest::Client::new(),
            )
            .unwrap_or_else(|e| panic!("{kind:?} should build: {e}"));
            assert!(reg.chat_route("m").is_some(), "{kind:?} chat");
            assert!(reg.embedding_route("m").is_some(), "{kind:?} embed");
        }
    }

    #[test]
    fn vllm_and_cloudflare_require_a_base_url() {
        // No built-in default and none configured → a clear build error rather
        // than silently pointing at api.openai.com.
        for kind in [ProviderKind::Vllm, ProviderKind::Cloudflare] {
            assert!(kind.default_base_url().is_none(), "{kind:?} has no default");
            let result = Registry::build(
                vec![spec(kind, "p", None, vec![model("m", &[Capability::Chat])])],
                reqwest::Client::new(),
            );
            assert!(
                matches!(result, Err(RegistryError::MissingBaseUrl { .. })),
                "{kind:?} without base_url must be MissingBaseUrl"
            );
        }
        // With a base_url they build fine.
        let reg = Registry::build(
            vec![spec(
                ProviderKind::Vllm,
                "p",
                Some("http://localhost:8000/v1"),
                vec![model("m", &[Capability::Chat])],
            )],
            reqwest::Client::new(),
        )
        .expect("vllm with base_url builds");
        assert!(reg.chat_route("m").is_some());
    }

    #[test]
    fn tei_without_base_url_is_a_build_error() {
        let result = Registry::build(
            vec![spec(
                ProviderKind::Tei,
                "tei",
                None,
                vec![model("e", &[Capability::Embed])],
            )],
            reqwest::Client::new(),
        );
        assert!(matches!(result, Err(RegistryError::MissingBaseUrl { .. })));
    }

    #[test]
    fn upstream_id_is_carried_on_the_route() {
        let reg = Registry::build(
            vec![spec(
                ProviderKind::Openai,
                "openai",
                None,
                vec![ModelSpec {
                    id: "friendly".to_owned(),
                    upstream_id: "text-embedding-3-small".to_owned(),
                    capabilities: vec![Capability::Embed],
                    modalities: vec!["text".to_owned()],
                }],
            )],
            reqwest::Client::new(),
        )
        .unwrap();
        assert_eq!(
            reg.embedding_route("friendly").unwrap().upstream_id,
            "text-embedding-3-small"
        );
    }

    #[test]
    fn cohere_model_resolves_for_both_embed_and_rerank() {
        let reg = Registry::build(
            vec![spec(
                ProviderKind::Cohere,
                "cohere",
                None,
                vec![model("multi", &[Capability::Embed, Capability::Rerank])],
            )],
            reqwest::Client::new(),
        )
        .unwrap();
        assert!(reg.embedding_route("multi").is_some());
        assert!(reg.rerank_route("multi").is_some());
    }

    #[test]
    fn duplicate_model_id_across_providers_is_a_build_error_naming_both() {
        let result = Registry::build(
            vec![
                spec(
                    ProviderKind::Openai,
                    "provider-one",
                    None,
                    vec![model("dup", &[Capability::Embed])],
                ),
                spec(
                    ProviderKind::Cohere,
                    "provider-two",
                    None,
                    vec![model("dup", &[Capability::Rerank])],
                ),
            ],
            reqwest::Client::new(),
        );
        match result {
            Err(RegistryError::DuplicateModelId {
                id,
                first_provider,
                second_provider,
            }) => {
                assert_eq!(id, "dup");
                assert_eq!(first_provider, "provider-one");
                assert_eq!(second_provider, "provider-two");
            }
            _ => panic!("expected DuplicateModelId build error"),
        }
    }

    #[test]
    fn list_models_reflects_config_with_owner_and_capabilities() {
        let reg = Registry::build(
            vec![
                spec(
                    ProviderKind::Cohere,
                    "cohere",
                    None,
                    vec![model("rr", &[Capability::Embed, Capability::Rerank])],
                ),
                spec(
                    ProviderKind::Openai,
                    "openai",
                    None,
                    vec![model("emb", &[Capability::Embed])],
                ),
            ],
            reqwest::Client::new(),
        )
        .unwrap();

        let models = reg.list_models();
        assert_eq!(models.len(), 2);
        let cohere = models.iter().find(|m| m.id == "rr").unwrap();
        assert_eq!(cohere.owned_by, "cohere");
        assert!(cohere.capabilities.contains(&Capability::Rerank));
        assert!(cohere.capabilities.contains(&Capability::Embed));
    }
}
