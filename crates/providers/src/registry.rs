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
use std::time::Duration;

use crate::anthropic::AnthropicProvider;
use crate::azure::AzureProvider;
use crate::bedrock::{self, BedrockProvider};
use crate::cloudflare::CloudflareRerankProvider;
use crate::cohere::CohereProvider;
use crate::google::vertex::VertexProvider;
use crate::google::GoogleProvider;
use crate::jina::JinaProvider;
use crate::kind::ProviderKind;
use crate::mistral::MistralProvider;
use crate::mixedbread::MixedbreadProvider;
use crate::nvidia::NvidiaProvider;
use crate::ollama::OllamaProvider;
use crate::openai::OpenAiProvider;
use crate::pinecone::PineconeProvider;
use crate::tei::TeiProvider;
use crate::together::TogetherRerankProvider;
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
    /// Azure OpenAI `api-version` (honored by the `azure` kind only; other
    /// kinds ignore it with a warning). Wins over an `?api-version=...` query
    /// string on `base_url` (kept for back-compat), which wins over the
    /// provider's built-in default (issue #65).
    pub api_version: Option<String>,
    /// Reject requests that set an unsupported-but-meaningful field (rather than
    /// silently dropping it). Currently honored by Ollama for `dimensions`
    /// (issue #25). Defaults to `false` (lenient).
    pub strict: bool,
    /// Per-provider connection-establishment timeout, in ms. When set, this
    /// provider is given its OWN [`reqwest::Client`] (built at registry
    /// construction) with this connect timeout; the trade-off is that such a
    /// provider no longer shares the process-wide connection pool (ADR 005,
    /// 2026-07-15 amendment). `None` (the common case) keeps the provider on
    /// the shared, pooled client.
    pub connect_timeout_ms: Option<u64>,
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
            .field("api_version", &self.api_version)
            .field("strict", &self.strict)
            .field("connect_timeout_ms", &self.connect_timeout_ms)
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

    /// A Bedrock provider whose AWS region could not be determined. Signing
    /// with a guessed default region would only fail later with an opaque
    /// upstream 403, so this is surfaced at build time instead.
    #[error(
        "provider '{name}' (kind 'bedrock') needs an AWS region: set base_url to a \
         bedrock-runtime.<region> endpoint, or export AWS_REGION / AWS_DEFAULT_REGION"
    )]
    MissingRegion {
        /// The offending provider's name.
        name: String,
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

    /// A model declared `embed` on a hosted kind whose upstream has no
    /// embeddings API (see [`ProviderKind::supports_embeddings`]), while the
    /// provider points at the vendor's default base URL. Such a request could
    /// only ever 404 upstream, so it is rejected at build time (config load
    /// or hot reload) instead of failing on the first request. A custom
    /// `base_url` (an operator-run proxy in front of the host) bypasses the
    /// check.
    #[error(
        "model '{model}' on provider '{name}' declares the 'embed' capability, \
         but kind '{kind}' has no upstream embeddings API; remove the \
         capability, or, to front this host with an embedding-capable \
         endpoint, set a base_url (or use kind = \"openai\")"
    )]
    NoUpstreamEmbeddings {
        /// The offending provider's name.
        name: String,
        /// Its kind, for the operator's benefit.
        kind: &'static str,
        /// The model that declared `embed`.
        model: String,
    },

    /// A provider's own configuration was rejected by its implementation (e.g.
    /// Vertex AI service-account credentials that were missing or unparseable).
    /// The message is secret-free.
    #[error("provider '{name}' configuration error: {message}")]
    ProviderConfig {
        /// The offending provider's name.
        name: String,
        /// A secret-free description of what was wrong.
        message: String,
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
    /// Overall-timeout backstop applied to any dedicated per-provider client
    /// (built when a spec overrides its connect timeout). Kept identical to the
    /// shared client's backstop so an overriding provider only changes its
    /// connect timeout, nothing else. Retained for hot-reload rebuilds.
    overall_backstop: Duration,
}

impl Registry {
    /// Build the registry from provider specs, sharing `client` across all
    /// provider instances that do not override their connect timeout. A spec
    /// with `connect_timeout_ms` set is given its own client (built here, once)
    /// with that connect timeout and `overall_backstop` as the overall cap - so
    /// pooling is preserved for every provider that does not override. Takes
    /// ownership of the spec set (a full description of the routing table).
    #[allow(clippy::needless_pass_by_value)]
    pub fn build(
        specs: Vec<ProviderSpec>,
        client: reqwest::Client,
        overall_backstop: Duration,
    ) -> Result<Self, RegistryError> {
        let inner = build_inner(&specs, &client, overall_backstop)?;
        Ok(Self {
            inner: ArcSwap::from_pointee(inner),
            client,
            overall_backstop,
        })
    }

    /// Atomically replace the routing table (hot reload - M7). Dedicated
    /// per-provider clients are rebuilt from the new specs, so a changed (or
    /// newly added/removed) `connect_timeout_ms` override takes effect on reload.
    #[allow(clippy::needless_pass_by_value)]
    pub fn reload(&self, specs: Vec<ProviderSpec>) -> Result<(), RegistryError> {
        let inner = build_inner(&specs, &self.client, self.overall_backstop)?;
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

fn build_inner(
    specs: &[ProviderSpec],
    client: &reqwest::Client,
    overall_backstop: Duration,
) -> Result<Inner, RegistryError> {
    let mut inner = Inner::default();
    // model id -> the provider that first declared it, so a collision names both.
    let mut owner: HashMap<&str, &str> = HashMap::new();

    for spec in specs {
        // A provider that overrides its connect timeout gets a dedicated client
        // (built once, here) with that timeout; every other provider stays on
        // the shared, pooled client. Owned locally so its lifetime spans the
        // `build_providers` call below.
        let dedicated = spec
            .connect_timeout_ms
            .map(|ms| crate::http::build_client_with(Duration::from_millis(ms), overall_backstop));
        let provider_client = dedicated.as_ref().unwrap_or(client);

        // One instance per provider, shared across all of its models via `Arc`.
        let built = build_providers(spec, provider_client)?;

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
                // A hosted kind with no upstream embeddings API would accept
                // the model here (its OpenAI-compatible wiring builds an embed
                // provider) and then 404 on every request: reject at build
                // time instead (issue #74). Only when the provider points at
                // the vendor's own default base URL, though: a custom
                // `base_url` means the operator fronts the kind with their own
                // endpoint (a proxy or gateway), which may well serve
                // embeddings, so the override is the escape hatch.
                if spec.base_url.is_none() && !spec.kind.supports_embeddings() {
                    return Err(RegistryError::NoUpstreamEmbeddings {
                        name: spec.name.clone(),
                        kind: spec.kind.as_str(),
                        model: model.id.clone(),
                    });
                }
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

    // `api_version` is an Azure-only knob. The server config rejects it on
    // other kinds at boot; this warning covers callers that build specs
    // directly (mirrors `warn_unsupported` for capabilities).
    if spec.api_version.is_some() && spec.kind != ProviderKind::Azure {
        tracing::warn!(
            provider = %spec.name,
            kind = %spec.kind.as_str(),
            "api_version is only honored by kind 'azure'; ignoring it"
        );
    }

    match spec.kind {
        // OpenAI + every OpenAI-compatible host (Groq, Fireworks, DeepSeek,
        // OpenRouter, Perplexity, xAI, DeepInfra, Hugging Face router,
        // self-hosted vLLM/llama.cpp/LM Studio) share the OpenAI provider;
        // only the base URL differs. The base is the explicit override, else
        // the kind's built-in default. Kinds with no built-in default (vLLM)
        // must not silently fall through to api.openai.com, so a missing URL
        // is a build error. Cloudflare Workers AI and Together each have
        // their own arm below: they share this chat/embed wiring but also
        // build a native rerank provider from the same `base_url`.
        ProviderKind::Openai
        | ProviderKind::Groq
        | ProviderKind::Fireworks
        | ProviderKind::Deepseek
        | ProviderKind::Openrouter
        | ProviderKind::Perplexity
        | ProviderKind::Xai
        | ProviderKind::Deepinfra
        | ProviderKind::Huggingface
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
        // Cloudflare Workers AI: chat + embed via the same OpenAI-compatible
        // wiring as above (its `base_url` carries the account id, so it is
        // always required - never falls through to a built-in default), plus
        // rerank via the native `/ai/run/{model}` endpoint (bge-reranker-*),
        // which is not OpenAI-shaped (see `crate::cloudflare`).
        ProviderKind::Cloudflare => {
            let base_url = require_base_url()?;
            let chat: Arc<dyn ChatProvider> = Arc::new(OpenAiProvider::new(
                client.clone(),
                spec.name.clone(),
                Some(base_url.clone()),
                spec.api_key.clone(),
            ));
            let embed: Arc<dyn EmbeddingProvider> = Arc::new(OpenAiProvider::new(
                client.clone(),
                spec.name.clone(),
                Some(base_url.clone()),
                spec.api_key.clone(),
            ));
            let rerank: Arc<dyn RerankProvider> = Arc::new(CloudflareRerankProvider::new(
                client.clone(),
                spec.name.clone(),
                base_url,
                spec.api_key.clone(),
            ));
            Ok(BuiltProviders {
                chat: Some(chat),
                embed: Some(embed),
                rerank: Some(rerank),
            })
        }
        // Together AI: chat + embed via the same OpenAI-compatible wiring as
        // above (built-in default base URL, overridable), plus native rerank
        // (LlamaRank) via Together's own `/rerank` endpoint, which is
        // Cohere-shaped (see `crate::together`). Mirrors the Cloudflare arm.
        ProviderKind::Together => {
            let base_url = spec
                .base_url
                .clone()
                .or_else(|| spec.kind.default_base_url().map(str::to_owned));
            let chat: Arc<dyn ChatProvider> = Arc::new(OpenAiProvider::new(
                client.clone(),
                spec.name.clone(),
                base_url.clone(),
                spec.api_key.clone(),
            ));
            let embed: Arc<dyn EmbeddingProvider> = Arc::new(OpenAiProvider::new(
                client.clone(),
                spec.name.clone(),
                base_url.clone(),
                spec.api_key.clone(),
            ));
            let rerank: Arc<dyn RerankProvider> = Arc::new(TogetherRerankProvider::new(
                client.clone(),
                spec.name.clone(),
                base_url,
                spec.api_key.clone(),
            ));
            Ok(BuiltProviders {
                chat: Some(chat),
                embed: Some(embed),
                rerank: Some(rerank),
            })
        }
        ProviderKind::Mixedbread => {
            let rerank: Arc<dyn RerankProvider> = Arc::new(MixedbreadProvider::new(
                client.clone(),
                spec.name.clone(),
                spec.base_url.clone(),
                spec.api_key.clone(),
            ));
            Ok(BuiltProviders {
                chat: None,
                embed: None,
                rerank: Some(rerank),
            })
        }
        ProviderKind::Pinecone => {
            let rerank: Arc<dyn RerankProvider> = Arc::new(PineconeProvider::new(
                client.clone(),
                spec.name.clone(),
                spec.base_url.clone(),
                spec.api_key.clone(),
            ));
            Ok(BuiltProviders {
                chat: None,
                embed: None,
                rerank: Some(rerank),
            })
        }
        // NVIDIA NIM: rerank via `/v1/ranking`. `base_url` is required (the NIM
        // root); the key is optional (self-hosted NIMs are keyless).
        ProviderKind::Nvidia => {
            let base_url = require_base_url()?;
            let rerank: Arc<dyn RerankProvider> = Arc::new(NvidiaProvider::new(
                client.clone(),
                spec.name.clone(),
                base_url,
                spec.api_key.clone(),
            ));
            Ok(BuiltProviders {
                chat: None,
                embed: None,
                rerank: Some(rerank),
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
                spec.strict,
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
            let chat: Arc<dyn ChatProvider> = provider.clone();
            let embed: Arc<dyn EmbeddingProvider> = provider.clone();
            let rerank: Arc<dyn RerankProvider> = provider;
            Ok(BuiltProviders {
                chat: Some(chat),
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
        ProviderKind::Azure => {
            // Every Azure resource endpoint is operator-specific - there is no
            // shared public default (unlike `openai`).
            let base_url = require_base_url()?;
            let provider = Arc::new(AzureProvider::new(
                client.clone(),
                spec.name.clone(),
                &base_url,
                spec.api_version.clone(),
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
        // Vertex AI carries its config in the existing spec fields: `base_url`
        // holds the GCP region, `api_key` holds the inline service-account JSON
        // (the provider secret). The project id comes from the credentials.
        ProviderKind::VertexAi => {
            let location = require_base_url()?;
            let provider = VertexProvider::new(
                client.clone(),
                spec.name.clone(),
                spec.api_key.as_deref(),
                None,
                Some(location),
                None,
            )
            .map_err(|e| RegistryError::ProviderConfig {
                name: spec.name.clone(),
                message: e.to_string(),
            })?;
            let chat: Arc<dyn ChatProvider> = Arc::new(provider);
            Ok(BuiltProviders {
                chat: Some(chat),
                embed: None,
                rerank: None,
            })
        }
        ProviderKind::Bedrock => {
            // The signing region comes from the endpoint host (standard or VPC
            // shapes) or from AWS_REGION / AWS_DEFAULT_REGION; a region that
            // cannot be determined is a BUILD error - silently signing for a
            // default region would just 403 at request time. Credentials are
            // re-read from the AWS environment variables on every request (with
            // the optional api_key override for the secret), so a missing key
            // here is not a build error: the provider reports it at request
            // time, and rotated values are picked up without a reload.
            let region = bedrock::resolve_region(spec.base_url.as_deref()).ok_or_else(|| {
                RegistryError::MissingRegion {
                    name: spec.name.clone(),
                }
            })?;
            let chat: Arc<dyn ChatProvider> = Arc::new(BedrockProvider::new_with_env_credentials(
                client.clone(),
                spec.name.clone(),
                region,
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
            api_version: None,
            strict: false,
            connect_timeout_ms: None,
            models,
        }
    }

    /// A provider whose connect timeout is overridden gets a dedicated client
    /// with that timeout, so a connect to an unroutable host fails fast well
    /// before the shared client's default (10 s) connect timeout would. The
    /// non-routable 10.255.255.1 address swallows the SYN, so the only thing
    /// that can end the call is the connect timeout: a fast return proves the
    /// per-provider override took effect. Kept deterministic (no wiremock) by
    /// asserting a generous upper bound far below the default.
    #[tokio::test]
    async fn overriding_provider_uses_its_own_fast_connect_timeout() {
        use lumen_core::{ChatRequest, ProviderError};
        use std::time::{Duration, Instant};
        use tokio_util::sync::CancellationToken;

        let mut overriding = spec(
            ProviderKind::Openai,
            "slow-host",
            Some("http://10.255.255.1:81/v1"),
            vec![model("m", &[Capability::Chat])],
        );
        overriding.connect_timeout_ms = Some(150);

        let reg = Registry::build(
            vec![overriding],
            reqwest::Client::new(),
            Duration::from_secs(300),
        )
        .expect("registry builds");

        let route = reg.chat_route("m").expect("chat route present");
        let started = Instant::now();
        let result = route
            .provider
            .chat(
                ChatRequest {
                    model: "m".to_owned(),
                    messages: Vec::new(),
                    temperature: None,
                    top_p: None,
                    max_tokens: None,
                    n: None,
                    stop: None,
                    stream: false,
                    extra: serde_json::Map::new(),
                },
                CancellationToken::new(),
            )
            .await;
        let elapsed = started.elapsed();

        assert!(
            matches!(result, Err(ProviderError::ConnectTimeout { .. })),
            "expected a connect timeout, got {result:?}"
        );
        // Default connect timeout is 10 s; a return under 5 s can only mean the
        // 150 ms per-provider override was applied to a dedicated client.
        assert!(
            elapsed < Duration::from_secs(5),
            "connect should have failed fast under the override, took {elapsed:?}"
        );
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
            Duration::from_secs(300),
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
            Duration::from_secs(300),
        );
        assert!(matches!(result, Err(RegistryError::MissingBaseUrl { .. })));
    }

    #[test]
    fn openai_compatible_kinds_build_with_their_default_base_url() {
        // A hosted OpenAI-compatible kind resolves for chat (and, when the
        // upstream serves one, embed) with no base_url configured (its
        // built-in default is used).
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
            let mut caps = vec![Capability::Chat];
            if kind.supports_embeddings() {
                caps.push(Capability::Embed);
            }
            let reg = Registry::build(
                vec![spec(kind, "p", None, vec![model("m", &caps)])],
                reqwest::Client::new(),
                Duration::from_secs(300),
            )
            .unwrap_or_else(|e| panic!("{kind:?} should build: {e}"));
            assert!(reg.chat_route("m").is_some(), "{kind:?} chat");
            assert_eq!(
                reg.embedding_route("m").is_some(),
                kind.supports_embeddings(),
                "{kind:?} embed"
            );
        }
    }

    #[test]
    fn embed_on_a_hosted_kind_with_no_upstream_embeddings_api_is_a_build_error() {
        // Groq, DeepSeek, OpenRouter, Perplexity and xAI have no upstream
        // /embeddings endpoint: an embed model there (with no base_url
        // override, i.e. pointing at the vendor's own API) could only ever
        // 404 at request time, so the registry rejects it at build time
        // (issue #74).
        for kind in [
            ProviderKind::Groq,
            ProviderKind::Deepseek,
            ProviderKind::Openrouter,
            ProviderKind::Perplexity,
            ProviderKind::Xai,
        ] {
            let result = Registry::build(
                vec![spec(
                    kind,
                    "p",
                    None,
                    vec![model("emb-model", &[Capability::Embed])],
                )],
                reqwest::Client::new(),
                Duration::from_secs(300),
            );
            match result {
                Err(RegistryError::NoUpstreamEmbeddings {
                    name,
                    kind: kind_str,
                    model,
                }) => {
                    assert_eq!(name, "p");
                    assert_eq!(kind_str, kind.as_str());
                    assert_eq!(model, "emb-model");
                }
                Err(other) => {
                    panic!("{kind:?} embed model must be NoUpstreamEmbeddings, got {other:?}")
                }
                Ok(_) => panic!("{kind:?} embed model must not build"),
            }
        }
    }

    #[test]
    fn embed_on_an_embeddingless_kind_with_a_custom_base_url_builds() {
        // A custom base_url means the operator fronts the host with their own
        // endpoint (a proxy or gateway) that may serve embeddings, so the
        // upstream-capability rejection must not fire.
        let reg = Registry::build(
            vec![spec(
                ProviderKind::Groq,
                "groq-proxy",
                Some("http://gateway.internal:8080/v1"),
                vec![model("emb-model", &[Capability::Embed])],
            )],
            reqwest::Client::new(),
            Duration::from_secs(300),
        )
        .expect("a base_url override is the escape hatch: the build must pass");
        assert!(reg.embedding_route("emb-model").is_some());
    }

    #[test]
    fn embed_on_hosted_kinds_that_serve_embeddings_still_resolves() {
        // Fireworks, Together, DeepInfra and the Hugging Face router genuinely
        // serve embeddings, so their embed models must keep building and
        // resolving (issue #74 acceptance criterion).
        for kind in [
            ProviderKind::Fireworks,
            ProviderKind::Together,
            ProviderKind::Deepinfra,
            ProviderKind::Huggingface,
        ] {
            let reg = Registry::build(
                vec![spec(
                    kind,
                    "p",
                    None,
                    vec![model("emb-model", &[Capability::Embed])],
                )],
                reqwest::Client::new(),
                Duration::from_secs(300),
            )
            .unwrap_or_else(|e| panic!("{kind:?} embed model should build: {e}"));
            assert!(reg.embedding_route("emb-model").is_some(), "{kind:?}");
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
                Duration::from_secs(300),
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
            Duration::from_secs(300),
        )
        .expect("vllm with base_url builds");
        assert!(reg.chat_route("m").is_some());
    }

    #[test]
    fn azure_without_base_url_is_a_build_error() {
        // Azure has no shared public default endpoint (every resource is
        // operator-specific), unlike `openai`.
        let result = Registry::build(
            vec![spec(
                ProviderKind::Azure,
                "azure",
                None,
                vec![model("m", &[Capability::Chat])],
            )],
            reqwest::Client::new(),
            Duration::from_secs(300),
        );
        assert!(matches!(result, Err(RegistryError::MissingBaseUrl { .. })));
    }

    #[test]
    fn azure_with_base_url_resolves_chat_and_embed() {
        let reg = Registry::build(
            vec![spec(
                ProviderKind::Azure,
                "azure",
                Some("https://my-resource.openai.azure.com"),
                vec![model("m", &[Capability::Chat, Capability::Embed])],
            )],
            reqwest::Client::new(),
            Duration::from_secs(300),
        )
        .unwrap();
        assert!(reg.chat_route("m").is_some());
        assert!(reg.embedding_route("m").is_some());
    }

    #[test]
    fn bedrock_with_regional_endpoint_builds_and_serves_chat() {
        let reg = Registry::build(
            vec![spec(
                ProviderKind::Bedrock,
                "bedrock",
                Some("https://bedrock-runtime.eu-west-1.amazonaws.com"),
                vec![model("claude", &[Capability::Chat])],
            )],
            reqwest::Client::new(),
            Duration::from_secs(300),
        )
        .unwrap();
        assert!(reg.chat_route("claude").is_some());
    }

    #[test]
    fn bedrock_with_undeterminable_region_is_a_build_error() {
        // Only this test (and the resolve_region fallback) touches these vars;
        // clearing them makes the custom endpoint's region undeterminable.
        std::env::remove_var("AWS_REGION");
        std::env::remove_var("AWS_DEFAULT_REGION");
        let result = Registry::build(
            vec![spec(
                ProviderKind::Bedrock,
                "bedrock",
                Some("http://127.0.0.1:8080"),
                vec![model("m", &[Capability::Chat])],
            )],
            reqwest::Client::new(),
            Duration::from_secs(300),
        );
        assert!(
            matches!(result, Err(RegistryError::MissingRegion { .. })),
            "a custom endpoint with no region source must fail the build"
        );
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
            Duration::from_secs(300),
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
            Duration::from_secs(300),
        )
        .unwrap();
        assert_eq!(
            reg.embedding_route("friendly").unwrap().upstream_id,
            "text-embedding-3-small"
        );
    }

    #[test]
    fn vertex_ai_resolves_chat_and_validates_config() {
        let creds = serde_json::json!({
            "type": "service_account",
            "project_id": "proj",
            "client_email": "svc@proj.iam.gserviceaccount.com",
            "private_key": include_str!("google/vertex/testdata/test_private_key.pem"),
            "token_uri": "https://oauth2.googleapis.com/token",
        })
        .to_string();

        // With region (in base_url) + credentials: the chat route resolves.
        let reg = Registry::build(
            vec![ProviderSpec {
                name: "vertex".to_owned(),
                kind: ProviderKind::VertexAi,
                api_key: Some(creds.clone()),
                base_url: Some("us-central1".to_owned()),
                api_version: None,
                strict: false,
                connect_timeout_ms: None,
                models: vec![model("gemini-flash", &[Capability::Chat])],
            }],
            reqwest::Client::new(),
            Duration::from_secs(300),
        )
        .expect("vertex_ai builds");
        assert!(reg.chat_route("gemini-flash").is_some());

        // Without a region there is nothing to route to: a clear build error.
        let no_region = Registry::build(
            vec![ProviderSpec {
                name: "vertex".to_owned(),
                kind: ProviderKind::VertexAi,
                api_key: Some(creds),
                base_url: None,
                api_version: None,
                strict: false,
                connect_timeout_ms: None,
                models: vec![model("m", &[Capability::Chat])],
            }],
            reqwest::Client::new(),
            Duration::from_secs(300),
        );
        assert!(matches!(
            no_region,
            Err(RegistryError::MissingBaseUrl { .. })
        ));

        // A missing credentials env var must NOT fail the boot (parity with
        // every other provider whose key env is unset).
        let keyless = Registry::build(
            vec![ProviderSpec {
                name: "vertex".to_owned(),
                kind: ProviderKind::VertexAi,
                api_key: None,
                base_url: Some("us-central1".to_owned()),
                api_version: None,
                strict: false,
                connect_timeout_ms: None,
                models: vec![model("m", &[Capability::Chat])],
            }],
            reqwest::Client::new(),
            Duration::from_secs(300),
        );
        assert!(keyless.is_ok(), "unset creds env must still boot");

        // Garbage credentials JSON is deterministic misconfiguration: build error.
        let garbage = Registry::build(
            vec![ProviderSpec {
                name: "vertex".to_owned(),
                kind: ProviderKind::VertexAi,
                api_key: Some("sk-test-xxx".to_owned()),
                base_url: Some("us-central1".to_owned()),
                api_version: None,
                strict: false,
                connect_timeout_ms: None,
                models: vec![model("m", &[Capability::Chat])],
            }],
            reqwest::Client::new(),
            Duration::from_secs(300),
        );
        assert!(matches!(garbage, Err(RegistryError::ProviderConfig { .. })));
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
            Duration::from_secs(300),
        )
        .unwrap();
        assert!(reg.embedding_route("multi").is_some());
        assert!(reg.rerank_route("multi").is_some());
    }

    #[test]
    fn cloudflare_model_resolves_for_chat_embed_and_native_rerank() {
        // A single `cloudflare` provider entry serves all three capabilities:
        // chat + embed via the OpenAI-compatible path, rerank via the native
        // `/ai/run/{model}` endpoint - all against the same `base_url`.
        let reg = Registry::build(
            vec![spec(
                ProviderKind::Cloudflare,
                "cf",
                Some("https://api.cloudflare.com/client/v4/accounts/acct123/ai/v1"),
                vec![model(
                    "cf-multi",
                    &[Capability::Chat, Capability::Embed, Capability::Rerank],
                )],
            )],
            reqwest::Client::new(),
            Duration::from_secs(300),
        )
        .unwrap();
        assert!(reg.chat_route("cf-multi").is_some());
        assert!(reg.embedding_route("cf-multi").is_some());
        assert!(reg.rerank_route("cf-multi").is_some());
    }

    #[test]
    fn cohere_model_resolves_for_chat_embed_and_rerank() {
        let reg = Registry::build(
            vec![spec(
                ProviderKind::Cohere,
                "cohere",
                None,
                vec![model(
                    "command-r-plus",
                    &[Capability::Chat, Capability::Embed, Capability::Rerank],
                )],
            )],
            reqwest::Client::new(),
            Duration::from_secs(300),
        )
        .unwrap();
        assert!(reg.chat_route("command-r-plus").is_some());
        assert!(reg.embedding_route("command-r-plus").is_some());
        assert!(reg.rerank_route("command-r-plus").is_some());
    }

    #[test]
    fn mixedbread_and_pinecone_resolve_for_rerank_only() {
        for kind in [ProviderKind::Mixedbread, ProviderKind::Pinecone] {
            let reg = Registry::build(
                vec![spec(
                    kind,
                    "p",
                    None,
                    vec![model("rr", &[Capability::Rerank])],
                )],
                reqwest::Client::new(),
                Duration::from_secs(300),
            )
            .unwrap_or_else(|e| panic!("{kind:?} should build: {e}"));
            assert!(reg.rerank_route("rr").is_some(), "{kind:?} rerank");
            assert!(reg.embedding_route("rr").is_none(), "{kind:?} no embed");
            assert!(reg.chat_route("rr").is_none(), "{kind:?} no chat");
        }
    }

    #[test]
    fn together_resolves_for_chat_embed_and_native_rerank() {
        let reg = Registry::build(
            vec![spec(
                ProviderKind::Together,
                "together",
                None,
                vec![model(
                    "multi",
                    &[Capability::Chat, Capability::Embed, Capability::Rerank],
                )],
            )],
            reqwest::Client::new(),
            Duration::from_secs(300),
        )
        .unwrap();
        assert!(reg.chat_route("multi").is_some());
        assert!(reg.embedding_route("multi").is_some());
        assert!(reg.rerank_route("multi").is_some());
    }

    #[test]
    fn nvidia_without_base_url_is_a_build_error() {
        let result = Registry::build(
            vec![spec(
                ProviderKind::Nvidia,
                "nvidia",
                None,
                vec![model("rr", &[Capability::Rerank])],
            )],
            reqwest::Client::new(),
            Duration::from_secs(300),
        );
        assert!(matches!(result, Err(RegistryError::MissingBaseUrl { .. })));
    }

    #[test]
    fn nvidia_with_base_url_resolves_for_rerank() {
        let reg = Registry::build(
            vec![spec(
                ProviderKind::Nvidia,
                "nvidia",
                Some("http://localhost:8000"),
                vec![model("rr", &[Capability::Rerank])],
            )],
            reqwest::Client::new(),
            Duration::from_secs(300),
        )
        .expect("nvidia with base_url builds");
        assert!(reg.rerank_route("rr").is_some());
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
            Duration::from_secs(300),
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
            Duration::from_secs(300),
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
