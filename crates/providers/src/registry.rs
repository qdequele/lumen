//! Provider registry.
//!
//! Builds concrete provider instances from resolved specs and resolves
//! `(capability, model id)` to a provider plus the upstream model id to send.
//!
//! The inner table lives behind an [`ArcSwap`] so a future hot reload (M7) can
//! atomically swap the whole routing table without locking the request path.
//! API keys are resolved from the environment by the caller (the server) and
//! passed in already — the registry never reads env vars or holds config.

use arc_swap::ArcSwap;
use ferrogate_core::{Capability, EmbeddingProvider};
use std::collections::HashMap;
use std::sync::Arc;

use crate::kind::ProviderKind;
use crate::ollama::OllamaProvider;
use crate::openai::OpenAiProvider;

/// A model exposed by a provider, with its upstream id and capabilities.
#[derive(Debug, Clone)]
pub struct ModelSpec {
    /// Client-facing model id.
    pub id: String,
    /// Upstream model id to send.
    pub upstream_id: String,
    /// Declared capabilities.
    pub capabilities: Vec<Capability>,
}

/// A provider instance to build. `api_key` is already resolved from the
/// environment by the caller (or `None` for keyless providers).
#[derive(Debug, Clone)]
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

/// Failure while building the registry.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// A provider that requires a base URL did not have one configured.
    #[error("provider '{name}' (kind '{kind}') requires a base_url")]
    MissingBaseUrl { name: String, kind: &'static str },
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

#[derive(Default)]
struct Inner {
    /// model id -> embedding route.
    embedding: HashMap<String, EmbeddingRoute>,
    /// model id -> declared capabilities (all of them, even not-yet-served
    /// ones like chat in M2). Lets the router tell "unknown model" apart from
    /// "known model, wrong capability".
    model_capabilities: HashMap<String, Vec<Capability>>,
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

    /// Atomically replace the routing table (hot reload — M7).
    #[allow(clippy::needless_pass_by_value)]
    pub fn reload(&self, specs: Vec<ProviderSpec>) -> Result<(), RegistryError> {
        let inner = build_inner(&specs, &self.client)?;
        self.inner.store(Arc::new(inner));
        Ok(())
    }

    /// Resolve a model id to an embedding route, if one serves it.
    #[must_use]
    pub fn embedding_route(&self, model_id: &str) -> Option<EmbeddingRoute> {
        self.inner.load().embedding.get(model_id).cloned()
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
}

fn build_inner(specs: &[ProviderSpec], client: &reqwest::Client) -> Result<Inner, RegistryError> {
    let mut inner = Inner::default();

    for spec in specs {
        // One embedding provider instance per configured provider, shared
        // across all of its embedding models via `Arc`.
        let embed_provider = build_embedding_provider(spec, client)?;

        for model in &spec.models {
            inner
                .model_capabilities
                .entry(model.id.clone())
                .or_default()
                .extend(model.capabilities.iter().copied());

            if model.capabilities.contains(&Capability::Embed) {
                if let Some(provider) = &embed_provider {
                    inner.embedding.insert(
                        model.id.clone(),
                        EmbeddingRoute {
                            provider: provider.clone(),
                            provider_name: spec.name.clone(),
                            upstream_id: model.upstream_id.clone(),
                        },
                    );
                } else {
                    tracing::warn!(
                        provider = %spec.name,
                        kind = %spec.kind.as_str(),
                        model = %model.id,
                        "model declares 'embed' but this provider kind has no embedding \
                         implementation yet; it will not resolve for embeddings"
                    );
                }
            }
        }
    }

    Ok(inner)
}

/// Build an embedding provider for the given spec, or `None` if this kind does
/// not implement embeddings yet.
fn build_embedding_provider(
    spec: &ProviderSpec,
    client: &reqwest::Client,
) -> Result<Option<Arc<dyn EmbeddingProvider>>, RegistryError> {
    match spec.kind {
        ProviderKind::Openai => Ok(Some(Arc::new(OpenAiProvider::new(
            client.clone(),
            spec.name.clone(),
            spec.base_url.clone(),
            spec.api_key.clone(),
        )))),
        ProviderKind::Ollama => {
            let base_url = spec.base_url.clone().ok_or(RegistryError::MissingBaseUrl {
                name: spec.name.clone(),
                kind: spec.kind.as_str(),
            })?;
            Ok(Some(Arc::new(OllamaProvider::new(
                client.clone(),
                spec.name.clone(),
                base_url,
            ))))
        }
        // Embeddings for these kinds arrive in later milestones (M3+).
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
