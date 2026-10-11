//! Resolving a [`Decision`] against the provider [`Registry`] (ADR 014).
//! Leaves resolve by foundation id at request time, exactly as fallback
//! chains always have; a remap leaf wraps the decision route in a
//! [`DecisionRerankProvider`] carrying the compiled template.

use std::sync::Arc;

use lumen_core::{Capability, GatewayError};
use lumen_providers::decisions::rerank::DecisionRerankProvider;
use lumen_providers::{Registry, RerankRoute};
use lumen_telemetry::DecisionMetrics;

use crate::executor::Link;
use crate::virtual_models::decide::{Attempt, Decision};
use crate::{ChatChainLink, DecisionChainLink, EmbeddingChainLink, RerankChainLink};

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

/// Resolve a decision request (ADR 017): every attempt must serve
/// `decisions`, then attempts that cannot take this request (an image to a
/// text-only model, a choice of one option to OpenAI, ...) are skipped
/// before any upstream call. A skipped attempt is not an attempt: it never
/// reaches the executor, `usage_log` or a circuit breaker.
///
/// # Errors
/// The primary's routing miss (`LM-2001` / `LM-2002`), or, when every
/// attempt is incompatible, the first incompatibility (`LM-2003` / `LM-1001`).
pub fn resolve_decisions(
    registry: &Registry,
    decision: &mut Decision,
    req: &lumen_core::DecisionRequest,
) -> Result<Vec<DecisionChainLink>, GatewayError> {
    let routes = resolve(registry, decision, Capability::Decisions, |r, a| {
        r.decision_route(&a.model_id)
    })?;
    let has_images = req.has_images();
    let verdicts: Vec<Result<(), GatewayError>> = routes
        .iter()
        .zip(&decision.attempts)
        .map(|(route, a)| {
            if has_images
                && !registry
                    .modalities(&a.model_id)
                    .is_some_and(|m| m.iter().any(|x| x == "image"))
            {
                return Err(GatewayError::ImageInputNotSupported {
                    model: a.model_id.clone(),
                });
            }
            route.provider.check(req)
        })
        .collect();
    if verdicts.iter().all(Result::is_err) {
        let first = verdicts.into_iter().find_map(Result::err);
        return Err(
            first.unwrap_or_else(|| GatewayError::Internal("empty routing decision".to_owned()))
        );
    }
    // The common case (every attempt compatible) skips the keep/log/retain pass.
    if verdicts.iter().all(Result::is_ok) {
        return Ok(routes
            .into_iter()
            .zip(&decision.attempts)
            .map(|(route, a)| DecisionChainLink {
                route,
                model_id: a.model_id.clone(),
            })
            .collect());
    }
    for ((verdict, a), route) in verdicts.iter().zip(&decision.attempts).zip(&routes) {
        if let Err(e) = verdict {
            // The code only: the message can name a question (request content).
            tracing::debug!(
                model = %a.model_id,
                provider = %route.provider_name,
                code = e.code(),
                "skipping an incompatible decisions target"
            );
        }
    }
    let keep: Vec<bool> = verdicts.iter().map(Result::is_ok).collect();
    let links: Vec<DecisionChainLink> = routes
        .into_iter()
        .zip(&decision.attempts)
        .zip(&keep)
        .filter(|(_, k)| **k)
        .map(|((route, a), _)| DecisionChainLink {
            route,
            model_id: a.model_id.clone(),
        })
        .collect();
    decision.retain_compatible(&keep);
    Ok(links)
}

/// Resolve a rerank decision; remap attempts go through the decision route.
/// With `refusals`, a remap attempt that fails on a refusal (a refused
/// listwise `choice`) counts it against the attempt's model, even when a
/// later target serves the request.
///
/// # Errors
/// The primary's routing miss.
pub fn resolve_rerank_decision(
    registry: &Registry,
    decision: &mut Decision,
    refusals: Option<&DecisionMetrics>,
) -> Result<Vec<RerankChainLink>, GatewayError> {
    let routes = resolve(registry, decision, Capability::Rerank, |r, a| {
        match &a.remap {
            Some(template) => r.decision_route(&a.model_id).map(|route| {
                let provider_name = route.provider_name.clone();
                let mut provider = DecisionRerankProvider::new(
                    route.provider,
                    provider_name.clone(),
                    template.clone(),
                );
                if let Some(metrics) = refusals {
                    let metrics = metrics.clone();
                    let model = a.model_id.clone();
                    provider = provider
                        .with_refusal_hook(Arc::new(move |n| metrics.add_refusals(&model, n)));
                }
                RerankRoute {
                    provider: Arc::new(provider),
                    provider_name,
                    upstream_id: route.upstream_id,
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
    use lumen_providers::decisions::rerank::RerankTemplate;
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
    fn a_remap_attempt_resolves_through_the_decision_route() {
        let reg = registry(&[("jev", &[Capability::Decisions])], ProviderKind::Typesafe);
        let mut d = Decision::direct("jev");
        d.attempts[0].remap = Some(Arc::new(RerankTemplate::default()));
        let chain = resolve_rerank_decision(&reg, &mut d, None).unwrap();
        assert_eq!(chain[0].model_id, "jev");
        assert_eq!(chain[0].route.provider_name, "p");
        // Without the remap, a decisions-only model is not a reranker.
        let mut plain = Decision::direct("jev");
        assert_eq!(
            resolve_rerank_decision(&reg, &mut plain, None)
                .unwrap_err()
                .code(),
            "LM-2002"
        );
    }

    fn decision_registry() -> Registry {
        let model = |id: &str, modalities: &[&str]| ModelSpec {
            id: id.into(),
            upstream_id: id.into(),
            capabilities: vec![Capability::Decisions],
            modalities: modalities.iter().map(|m| (*m).to_owned()).collect(),
            release_date: None,
        };
        let provider = |name: &str, kind: ProviderKind, models: Vec<ModelSpec>| ProviderSpec {
            name: name.into(),
            kind,
            api_key: Some("k".into()),
            base_url: Some("http://127.0.0.1:9".into()),
            api_version: None,
            strict: false,
            connect_timeout_ms: None,
            decisions_path: None,
            forward_unknown_fields: None,
            models,
        };
        Registry::build(
            vec![
                provider(
                    "typesafe",
                    ProviderKind::Typesafe,
                    vec![model("jev", &["text"])],
                ),
                provider(
                    "openai",
                    ProviderKind::Openai,
                    vec![model("luna", &["text", "image"])],
                ),
            ],
            reqwest::Client::new(),
            std::time::Duration::from_secs(300),
        )
        .unwrap()
    }

    fn parse_req(body: &str) -> lumen_core::DecisionRequest {
        lumen_core::decisions::format::parse(body.as_bytes())
            .unwrap()
            .1
    }

    const IMAGE_BODY: &str = r#"{"model":"vm","input":[{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,AA"}]}],"questions":[{"type":"predicate","instructions":"i"}]}"#;

    #[test]
    fn an_image_request_skips_the_text_only_target() {
        let reg = decision_registry();
        let mut d = Decision::linear(["jev", "luna"].map(str::to_owned));
        let chain = resolve_decisions(&reg, &mut d, &parse_req(IMAGE_BODY)).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].model_id, "luna");
        assert_eq!(
            d.attempts.len(),
            1,
            "the skipped attempt leaves the decision too"
        );
        let links = decision_links(&d, chain.iter().map(|l| l.route.provider_name.as_str()));
        assert_eq!(links.len(), 1);
    }

    #[test]
    fn when_nothing_is_left_the_first_incompatibility_is_returned() {
        let reg = decision_registry();
        let mut d = Decision::direct("jev");
        assert!(matches!(
            resolve_decisions(&reg, &mut d, &parse_req(IMAGE_BODY)),
            Err(GatewayError::ImageInputNotSupported { model }) if model == "jev"
        ));
    }

    #[test]
    fn a_choice_of_one_skips_openai() {
        let reg = decision_registry();
        let mut d = Decision::linear(["luna", "jev"].map(str::to_owned));
        let req = parse_req(
            r#"{"model":"vm","state":"s","questions":{"c":{"type":"choice","instructions":"i","criteria":{"only":null}}}}"#,
        );
        let chain = resolve_decisions(&reg, &mut d, &req).unwrap();
        assert_eq!(chain[0].model_id, "jev");
    }
}
