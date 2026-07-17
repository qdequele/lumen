//! Routing layer for LUMEN.
//!
//! Resolves a `(capability, model)` pair to a concrete provider. The routing
//! table itself lives in [`lumen_providers::Registry`]; this crate turns a
//! lookup miss into the right client-facing [`GatewayError`], distinguishing an
//! unknown model (`LM-2001`, 404) from a known model that does not serve the
//! requested capability (`LM-2002`, 400).
//!
//! Fallback chains, circuit breaking and load balancing arrive in M6.

#![forbid(unsafe_code)]

pub mod circuit;
pub mod executor;
pub mod peek;
pub mod retry;

use lumen_core::{Capability, GatewayError};
use lumen_providers::{ChatRoute, EmbeddingRoute, Registry, RerankRoute};

/// Resolve a model id to a chat route, or the appropriate routing error.
///
/// # Errors
/// * [`GatewayError::ModelNotFound`] (`LM-2001`) if no provider declares the model.
/// * [`GatewayError::UnsupportedCapability`] (`LM-2002`) if the model exists but
///   does not serve chat.
pub fn resolve_chat(registry: &Registry, model_id: &str) -> Result<ChatRoute, GatewayError> {
    registry
        .chat_route(model_id)
        .ok_or_else(|| miss(registry, model_id, Capability::Chat))
}

/// Resolve a model id to an embedding route, or the appropriate routing error.
///
/// # Errors
/// * [`GatewayError::ModelNotFound`] (`LM-2001`) if no provider declares the model.
/// * [`GatewayError::UnsupportedCapability`] (`LM-2002`) if the model exists but
///   does not serve embeddings.
pub fn resolve_embedding(
    registry: &Registry,
    model_id: &str,
) -> Result<EmbeddingRoute, GatewayError> {
    registry
        .embedding_route(model_id)
        .ok_or_else(|| miss(registry, model_id, Capability::Embed))
}

/// Resolve a model id to a rerank route, or the appropriate routing error.
///
/// # Errors
/// * [`GatewayError::ModelNotFound`] (`LM-2001`) if no provider declares the model.
/// * [`GatewayError::UnsupportedCapability`] (`LM-2002`) if the model exists but
///   does not serve reranking.
pub fn resolve_rerank(registry: &Registry, model_id: &str) -> Result<RerankRoute, GatewayError> {
    registry
        .rerank_route(model_id)
        .ok_or_else(|| miss(registry, model_id, Capability::Rerank))
}

/// One resolved link of a chat fallback chain (M6).
#[derive(Debug, Clone)]
pub struct ChatChainLink {
    /// The resolved route (provider instance + upstream model id).
    pub route: ChatRoute,
    /// The client-facing model id of *this* link (the primary or a fallback).
    pub model_id: String,
}

/// One resolved link of an embedding fallback chain (M6).
#[derive(Debug, Clone)]
pub struct EmbeddingChainLink {
    /// The resolved route.
    pub route: EmbeddingRoute,
    /// The client-facing model id of this link.
    pub model_id: String,
}

/// One resolved link of a rerank fallback chain (M6).
#[derive(Debug, Clone)]
pub struct RerankChainLink {
    /// The resolved route.
    pub route: RerankRoute,
    /// The client-facing model id of this link.
    pub model_id: String,
}

/// Resolve an ordered list of model ids (primary first, then its fallbacks) to
/// a chat chain. The **primary** must resolve - its miss is the client-facing
/// error. A fallback that no longer resolves for chat is skipped with a warning
/// (boot validation makes this unreachable in practice; this is defence in
/// depth for a hot-reloaded table).
///
/// # Errors
/// The primary's routing miss ([`GatewayError::ModelNotFound`] /
/// [`GatewayError::UnsupportedCapability`]).
pub fn resolve_chat_chain(
    registry: &Registry,
    model_ids: &[String],
) -> Result<Vec<ChatChainLink>, GatewayError> {
    let mut chain = Vec::with_capacity(model_ids.len());
    for (position, id) in model_ids.iter().enumerate() {
        match registry.chat_route(id) {
            Some(route) => chain.push(ChatChainLink {
                route,
                model_id: id.clone(),
            }),
            None if position == 0 => return Err(miss(registry, id, Capability::Chat)),
            None => warn_skipped_fallback(id, "chat"),
        }
    }
    Ok(chain)
}

/// Resolve a primary + fallbacks to an embedding chain (see [`resolve_chat_chain`]).
///
/// # Errors
/// The primary's routing miss.
pub fn resolve_embedding_chain(
    registry: &Registry,
    model_ids: &[String],
) -> Result<Vec<EmbeddingChainLink>, GatewayError> {
    let mut chain = Vec::with_capacity(model_ids.len());
    for (position, id) in model_ids.iter().enumerate() {
        match registry.embedding_route(id) {
            Some(route) => chain.push(EmbeddingChainLink {
                route,
                model_id: id.clone(),
            }),
            None if position == 0 => return Err(miss(registry, id, Capability::Embed)),
            None => warn_skipped_fallback(id, "embed"),
        }
    }
    Ok(chain)
}

/// Resolve a primary + fallbacks to a rerank chain (see [`resolve_chat_chain`]).
///
/// # Errors
/// The primary's routing miss.
pub fn resolve_rerank_chain(
    registry: &Registry,
    model_ids: &[String],
) -> Result<Vec<RerankChainLink>, GatewayError> {
    let mut chain = Vec::with_capacity(model_ids.len());
    for (position, id) in model_ids.iter().enumerate() {
        match registry.rerank_route(id) {
            Some(route) => chain.push(RerankChainLink {
                route,
                model_id: id.clone(),
            }),
            None if position == 0 => return Err(miss(registry, id, Capability::Rerank)),
            None => warn_skipped_fallback(id, "rerank"),
        }
    }
    Ok(chain)
}

/// Build the executor-facing [`Link`](executor::Link) metadata for a chat chain.
#[must_use]
pub fn chat_links(chain: &[ChatChainLink]) -> Vec<executor::Link> {
    chain
        .iter()
        .map(|l| executor::Link {
            provider_name: l.route.provider_name.clone(),
            model_id: l.model_id.clone(),
        })
        .collect()
}

/// Build the executor-facing [`Link`](executor::Link) metadata for an embedding chain.
#[must_use]
pub fn embedding_links(chain: &[EmbeddingChainLink]) -> Vec<executor::Link> {
    chain
        .iter()
        .map(|l| executor::Link {
            provider_name: l.route.provider_name.clone(),
            model_id: l.model_id.clone(),
        })
        .collect()
}

/// Build the executor-facing [`Link`](executor::Link) metadata for a rerank chain.
#[must_use]
pub fn rerank_links(chain: &[RerankChainLink]) -> Vec<executor::Link> {
    chain
        .iter()
        .map(|l| executor::Link {
            provider_name: l.route.provider_name.clone(),
            model_id: l.model_id.clone(),
        })
        .collect()
}

fn warn_skipped_fallback(model_id: &str, capability: &str) {
    tracing::warn!(
        model = %model_id,
        capability,
        "configured fallback no longer resolves for this capability; skipping it"
    );
}

/// Turn a routing miss into the right client-facing error: a known model that
/// does not serve `capability` is `LM-2002`; an unknown model is `LM-2001`.
fn miss(registry: &Registry, model_id: &str, capability: Capability) -> GatewayError {
    if registry.knows_model(model_id) {
        GatewayError::UnsupportedCapability {
            model: model_id.to_owned(),
            capability,
        }
    } else {
        GatewayError::ModelNotFound(model_id.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_providers::{ModelSpec, ProviderKind, ProviderSpec};

    fn registry_with(models: Vec<ModelSpec>) -> Registry {
        Registry::build(
            vec![ProviderSpec {
                name: "openai".to_owned(),
                kind: ProviderKind::Openai,
                api_key: Some("sk-test-xxx".to_owned()),
                base_url: None,
                api_version: None,
                strict: false,
                connect_timeout_ms: None,
                models,
            }],
            reqwest::Client::new(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds")
    }

    fn cohere_registry(models: Vec<ModelSpec>) -> Registry {
        Registry::build(
            vec![ProviderSpec {
                name: "cohere".to_owned(),
                kind: ProviderKind::Cohere,
                api_key: Some("sk-test-xxx".to_owned()),
                base_url: None,
                api_version: None,
                strict: false,
                connect_timeout_ms: None,
                models,
            }],
            reqwest::Client::new(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds")
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
    fn resolves_a_known_embedding_model() {
        let reg = registry_with(vec![model("e", &[Capability::Embed])]);
        assert!(resolve_embedding(&reg, "e").is_ok());
    }

    #[test]
    fn unknown_model_is_model_not_found_fg2001() {
        let reg = registry_with(vec![model("e", &[Capability::Embed])]);
        let err = resolve_embedding(&reg, "nope").unwrap_err();
        assert_eq!(err.code(), "LM-2001");
        assert_eq!(err.http_status(), 404);
    }

    #[test]
    fn chat_only_model_is_unsupported_capability_fg2002() {
        let reg = registry_with(vec![model("c", &[Capability::Chat])]);
        let err = resolve_embedding(&reg, "c").unwrap_err();
        assert_eq!(err.code(), "LM-2002");
        assert_eq!(err.http_status(), 400);
    }

    #[test]
    fn resolves_a_known_rerank_model() {
        let reg = cohere_registry(vec![model("rr", &[Capability::Rerank])]);
        assert!(resolve_rerank(&reg, "rr").is_ok());
    }

    #[test]
    fn unknown_rerank_model_is_model_not_found_fg2001() {
        let reg = cohere_registry(vec![model("rr", &[Capability::Rerank])]);
        let err = resolve_rerank(&reg, "nope").unwrap_err();
        assert_eq!(err.code(), "LM-2001");
        assert_eq!(err.http_status(), 404);
    }

    #[test]
    fn chat_chain_resolves_primary_and_fallbacks_in_order() {
        let reg = registry_with(vec![
            model("gpt", &[Capability::Chat]),
            model("gpt-mini", &[Capability::Chat]),
        ]);
        let ids = vec!["gpt".to_owned(), "gpt-mini".to_owned()];
        let chain = resolve_chat_chain(&reg, &ids).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].model_id, "gpt");
        assert_eq!(chain[1].model_id, "gpt-mini");
        let links = chat_links(&chain);
        assert_eq!(links[0].provider_name, "openai");
        assert_eq!(links[1].model_id, "gpt-mini");
    }

    #[test]
    fn chat_chain_primary_miss_is_the_client_error() {
        let reg = registry_with(vec![model("gpt", &[Capability::Chat])]);
        let ids = vec!["nope".to_owned()];
        let err = resolve_chat_chain(&reg, &ids).unwrap_err();
        assert_eq!(err.code(), "LM-2001");
    }

    #[test]
    fn chat_chain_skips_an_unresolvable_fallback() {
        let reg = registry_with(vec![model("gpt", &[Capability::Chat])]);
        // The fallback "ghost" does not exist; it is skipped, not fatal.
        let ids = vec!["gpt".to_owned(), "ghost".to_owned()];
        let chain = resolve_chat_chain(&reg, &ids).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].model_id, "gpt");
    }

    #[test]
    fn embed_only_model_is_unsupported_for_rerank_fg2002() {
        let reg = cohere_registry(vec![model("emb", &[Capability::Embed])]);
        let err = resolve_rerank(&reg, "emb").unwrap_err();
        assert_eq!(err.code(), "LM-2002");
        assert_eq!(err.http_status(), 400);
        assert!(err.to_string().contains("rerank"));
    }
}
