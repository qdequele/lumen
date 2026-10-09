//! Routing layer for LUMEN.
//!
//! Resolves a `(capability, model)` pair to a concrete provider. The routing
//! table itself lives in [`lumen_providers::Registry`]; this crate turns a
//! lookup miss into the right client-facing [`GatewayError`], distinguishing an
//! unknown model (`LM-2001`, 404) from a known model that does not serve the
//! requested capability (`LM-2002`, 400).
//!
//! Fallback chains, weighted splits and conditional routing are virtual models
//! (ADR 014, [`virtual_models`]). The decisions capability (ADR 017) resolves
//! exactly like the other three.

#![forbid(unsafe_code)]

pub mod attempts;
pub mod circuit;
pub mod executor;
pub mod peek;
pub mod retry;
pub mod triggers;
pub mod virtual_models;

pub use attempts::{
    decision_links, resolve_chat_decision, resolve_decisions, resolve_embedding_decision,
    resolve_rerank_decision,
};

use lumen_core::{Capability, GatewayError};
use lumen_providers::{ChatRoute, DecisionRoute, EmbeddingRoute, Registry, RerankRoute};

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

/// One resolved attempt of a chat [`Decision`](virtual_models::Decision).
#[derive(Debug, Clone)]
pub struct ChatChainLink {
    /// The resolved route (provider instance + upstream model id).
    pub route: ChatRoute,
    /// The foundation model id of *this* attempt (the primary or a fallback).
    pub model_id: String,
}

/// One resolved attempt of an embedding decision.
#[derive(Debug, Clone)]
pub struct EmbeddingChainLink {
    /// The resolved route.
    pub route: EmbeddingRoute,
    /// The foundation model id of this attempt.
    pub model_id: String,
}

/// One resolved attempt of a rerank decision.
#[derive(Debug, Clone)]
pub struct RerankChainLink {
    /// The resolved route.
    pub route: RerankRoute,
    /// The foundation model id of this attempt.
    pub model_id: String,
}

/// One resolved attempt of a decisions request (ADR 017).
#[derive(Debug, Clone)]
pub struct DecisionChainLink {
    /// The resolved route.
    pub route: DecisionRoute,
    /// The foundation model id of this attempt.
    pub model_id: String,
}

/// Warn that a configured fallback no longer resolves for `capability` and is skipped.
pub(crate) fn warn_skipped_fallback(model_id: &str, capability: &str) {
    tracing::warn!(
        model = %model_id,
        capability,
        "configured fallback no longer resolves for this capability; skipping it"
    );
}

/// Turn a routing miss into the right client-facing error: a known model that
/// does not serve `capability` is `LM-2002`; an unknown model is `LM-2001`.
pub(crate) fn miss(registry: &Registry, model_id: &str, capability: Capability) -> GatewayError {
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
    use crate::virtual_models::Decision;
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
                decisions_path: None,
                forward_unknown_fields: None,
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
                decisions_path: None,
                forward_unknown_fields: None,
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
            release_date: None,
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
    fn chat_decision_resolves_primary_and_fallbacks_in_order() {
        let reg = registry_with(vec![
            model("gpt", &[Capability::Chat]),
            model("gpt-mini", &[Capability::Chat]),
        ]);
        let mut d = Decision::linear(["gpt", "gpt-mini"].map(str::to_owned));
        let chain = resolve_chat_decision(&reg, &mut d).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].model_id, "gpt");
        assert_eq!(chain[1].model_id, "gpt-mini");
        let links = decision_links(&d, chain.iter().map(|l| l.route.provider_name.as_str()));
        assert_eq!(links[0].provider_name, "openai");
        assert_eq!(links[1].model_id, "gpt-mini");
    }

    #[test]
    fn chat_decision_primary_miss_is_the_client_error() {
        let reg = registry_with(vec![model("gpt", &[Capability::Chat])]);
        let mut d = Decision::linear(["nope".to_owned()]);
        let err = resolve_chat_decision(&reg, &mut d).unwrap_err();
        assert_eq!(err.code(), "LM-2001");
    }

    #[test]
    fn chat_decision_skips_an_unresolvable_fallback() {
        let reg = registry_with(vec![model("gpt", &[Capability::Chat])]);
        // The fallback "ghost" does not exist; it is skipped, not fatal.
        let mut d = Decision::linear(["gpt", "ghost"].map(str::to_owned));
        let chain = resolve_chat_decision(&reg, &mut d).unwrap();
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
