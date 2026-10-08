//! Resolving a [`Decision`] against the provider [`Registry`] (ADR 014).
//! Leaves resolve by foundation id at request time, exactly as fallback
//! chains always have; a remap leaf wraps the SystemOne route in a
//! [`TypesafeRerankProvider`] carrying the compiled template.

use std::sync::Arc;

use lumen_core::{Capability, GatewayError};
use lumen_providers::typesafe::rerank::TypesafeRerankProvider;
use lumen_providers::{Registry, RerankRoute};

use crate::executor::Link;
use crate::virtual_models::decide::{Attempt, Decision};
use crate::{ChatChainLink, EmbeddingChainLink, RerankChainLink, SystemOneChainLink};

/// Resolve every attempt; a missing primary is the routing error, a missing
/// later attempt (a hot-reload race) is skipped with a warning.
fn resolve<R>(
    registry: &Registry,
    decision: &mut Decision,
    capability: Capability,
    lookup: impl Fn(&Registry, &Attempt) -> Option<R>,
) -> Result<Vec<R>, GatewayError> {
    let routes: Vec<Option<R>> = decision
        .attempts
        .iter()
        .map(|a| lookup(registry, a))
        .collect();
    let Some(primary) = decision.attempts.first() else {
        return Err(GatewayError::Internal("empty routing decision".to_owned()));
    };
    if routes.first().is_none_or(Option::is_none) {
        let wanted = if primary.remap.is_some() {
            Capability::Decisions
        } else {
            capability
        };
        return Err(crate::miss(registry, &primary.model_id, wanted));
    }
    // The common case (every route resolved) skips the keep/warn/retain pass.
    if routes.iter().any(Option::is_none) {
        let keep: Vec<bool> = routes.iter().map(Option::is_some).collect();
        for (attempt, ok) in decision.attempts.iter().zip(&keep) {
            if !ok {
                crate::warn_skipped_fallback(&attempt.model_id, capability.as_str());
            }
        }
        decision.retain_mask(&keep);
    }
    Ok(routes.into_iter().flatten().collect())
}

/// Resolve a chat decision.
///
/// # Errors
/// The primary's routing miss (`LM-2001` / `LM-2002`).
pub fn resolve_chat_decision(
    registry: &Registry,
    decision: &mut Decision,
) -> Result<Vec<ChatChainLink>, GatewayError> {
    let routes = resolve(registry, decision, Capability::Chat, |r, a| {
        r.chat_route(&a.model_id)
    })?;
    Ok(routes
        .into_iter()
        .zip(&decision.attempts)
        .map(|(route, a)| ChatChainLink {
            route,
            model_id: a.model_id.clone(),
        })
        .collect())
}

/// Resolve an embedding decision.
///
/// # Errors
/// The primary's routing miss.
pub fn resolve_embedding_decision(
    registry: &Registry,
    decision: &mut Decision,
) -> Result<Vec<EmbeddingChainLink>, GatewayError> {
    let routes = resolve(registry, decision, Capability::Embed, |r, a| {
        r.embedding_route(&a.model_id)
    })?;
    Ok(routes
        .into_iter()
        .zip(&decision.attempts)
        .map(|(route, a)| EmbeddingChainLink {
            route,
            model_id: a.model_id.clone(),
        })
        .collect())
}

/// Resolve a SystemOne decision.
///
/// # Errors
/// The primary's routing miss.
pub fn resolve_systemone_decision(
    registry: &Registry,
    decision: &mut Decision,
) -> Result<Vec<SystemOneChainLink>, GatewayError> {
    let routes = resolve(registry, decision, Capability::Decisions, |r, a| {
        r.systemone_route(&a.model_id)
    })?;
    Ok(routes
        .into_iter()
        .zip(&decision.attempts)
        .map(|(route, a)| SystemOneChainLink {
            route,
            model_id: a.model_id.clone(),
        })
        .collect())
}

/// Resolve a rerank decision; remap attempts go through the SystemOne route.
///
/// # Errors
/// The primary's routing miss.
pub fn resolve_rerank_decision(
    registry: &Registry,
    decision: &mut Decision,
) -> Result<Vec<RerankChainLink>, GatewayError> {
    let routes = resolve(registry, decision, Capability::Rerank, |r, a| {
        match &a.remap {
            Some(template) => r.systemone_route(&a.model_id).map(|so| {
                let provider_name = so.provider_name.clone();
                RerankRoute {
                    provider: Arc::new(TypesafeRerankProvider::new(
                        so.provider,
                        provider_name.clone(),
                        template.clone(),
                    )),
                    provider_name,
                    upstream_id: so.upstream_id,
                }
            }),
            None => r.rerank_route(&a.model_id),
        }
    })?;
    Ok(routes
        .into_iter()
        .zip(&decision.attempts)
        .map(|(route, a)| RerankChainLink {
            route,
            model_id: a.model_id.clone(),
        })
        .collect())
}

/// Executor links for a resolved decision: one per attempt, carrying its
/// escapes. `provider_names` yields each resolved link's provider name, in
/// attempt order.
#[must_use]
pub fn decision_links<'a>(
    decision: &Decision,
    provider_names: impl Iterator<Item = &'a str>,
) -> Vec<Link> {
    decision
        .attempts
        .iter()
        .zip(provider_names)
        .map(|(a, provider)| Link {
            provider_name: provider.to_owned(),
            model_id: a.model_id.clone(),
            escapes: a.escapes.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_core::Capability;
    use lumen_providers::typesafe::rerank::RerankTemplate;
    use lumen_providers::{ModelSpec, ProviderKind, ProviderSpec};

    fn registry(models: &[(&str, &[Capability])], kind: ProviderKind) -> Registry {
        Registry::build(
            vec![ProviderSpec {
                name: "p".into(),
                kind,
                api_key: Some("k".into()),
                base_url: Some("http://127.0.0.1:9".into()),
                api_version: None,
                strict: false,
                connect_timeout_ms: None,
                models: models
                    .iter()
                    .map(|(id, caps)| ModelSpec {
                        id: (*id).into(),
                        upstream_id: (*id).into(),
                        capabilities: caps.to_vec(),
                        modalities: vec!["text".into()],
                        release_date: None,
                    })
                    .collect(),
                decisions_path: None,
                forward_unknown_fields: None,
            }],
            reqwest::Client::new(),
            std::time::Duration::from_secs(300),
        )
        .unwrap()
    }

    #[test]
    fn a_missing_primary_is_the_routing_error() {
        let reg = registry(&[("a", &[Capability::Chat])], ProviderKind::Openai);
        let mut d = Decision::direct("ghost");
        assert_eq!(
            resolve_chat_decision(&reg, &mut d).unwrap_err().code(),
            "LM-2001"
        );
    }

    #[test]
    fn a_missing_fallback_is_skipped_and_links_align() {
        let reg = registry(
            &[("a", &[Capability::Chat]), ("c", &[Capability::Chat])],
            ProviderKind::Openai,
        );
        let mut d = Decision::linear(["a", "b", "c"].map(str::to_owned));
        let chain = resolve_chat_decision(&reg, &mut d).unwrap();
        assert_eq!(chain.len(), 2);
        let links = decision_links(&d, chain.iter().map(|l| l.route.provider_name.as_str()));
        assert_eq!(links[1].model_id, "c");
        assert_eq!(links[0].escapes[0].next, 1);
    }

    #[test]
    fn a_remap_attempt_resolves_through_the_systemone_route() {
        let reg = registry(&[("jev", &[Capability::Decisions])], ProviderKind::Typesafe);
        let mut d = Decision::direct("jev");
        d.attempts[0].remap = Some(Arc::new(RerankTemplate::default()));
        let chain = resolve_rerank_decision(&reg, &mut d).unwrap();
        assert_eq!(chain[0].model_id, "jev");
        assert_eq!(chain[0].route.provider_name, "p");
        // Without the remap, a SystemOne-only model is not a reranker.
        let mut plain = Decision::direct("jev");
        assert_eq!(
            resolve_rerank_decision(&reg, &mut plain)
                .unwrap_err()
                .code(),
            "LM-2002"
        );
    }
}
