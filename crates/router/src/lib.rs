//! Routing layer for Ferrogate.
//!
//! Resolves a `(capability, model)` pair to a concrete provider. The routing
//! table itself lives in [`ferrogate_providers::Registry`]; this crate turns a
//! lookup miss into the right client-facing [`GatewayError`], distinguishing an
//! unknown model (`FG-2001`, 404) from a known model that does not serve the
//! requested capability (`FG-2002`, 400).
//!
//! Fallback chains, circuit breaking and load balancing arrive in M6.

#![forbid(unsafe_code)]

use ferrogate_core::{Capability, GatewayError};
use ferrogate_providers::{EmbeddingRoute, Registry};

/// Resolve a model id to an embedding route, or the appropriate routing error.
///
/// # Errors
/// * [`GatewayError::ModelNotFound`] (`FG-2001`) if no provider declares the model.
/// * [`GatewayError::UnsupportedCapability`] (`FG-2002`) if the model exists but
///   does not serve embeddings.
pub fn resolve_embedding(
    registry: &Registry,
    model_id: &str,
) -> Result<EmbeddingRoute, GatewayError> {
    if let Some(route) = registry.embedding_route(model_id) {
        Ok(route)
    } else if registry.knows_model(model_id) {
        Err(GatewayError::UnsupportedCapability {
            model: model_id.to_owned(),
            capability: Capability::Embed,
        })
    } else {
        Err(GatewayError::ModelNotFound(model_id.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrogate_providers::{ModelSpec, ProviderKind, ProviderSpec};

    fn registry_with(models: Vec<ModelSpec>) -> Registry {
        Registry::build(
            vec![ProviderSpec {
                name: "openai".to_owned(),
                kind: ProviderKind::Openai,
                api_key: Some("sk-test-xxx".to_owned()),
                base_url: None,
                models,
            }],
            reqwest::Client::new(),
        )
        .expect("registry builds")
    }

    fn model(id: &str, caps: &[Capability]) -> ModelSpec {
        ModelSpec {
            id: id.to_owned(),
            upstream_id: id.to_owned(),
            capabilities: caps.to_vec(),
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
        assert_eq!(err.code(), "FG-2001");
        assert_eq!(err.http_status(), 404);
    }

    #[test]
    fn chat_only_model_is_unsupported_capability_fg2002() {
        let reg = registry_with(vec![model("c", &[Capability::Chat])]);
        let err = resolve_embedding(&reg, "c").unwrap_err();
        assert_eq!(err.code(), "FG-2002");
        assert_eq!(err.http_status(), 400);
    }
}
