//! The per-request decide phase (ADR 014): walk a compiled virtual model and
//! flatten it into an ordered attempt list. Pure: no I/O, no locks; the only
//! input besides the table is the request's [`FactSource`] and one random
//! draw per `split`.

use std::sync::Arc;

use lumen_core::{Capability, GatewayError};
use lumen_providers::typesafe::rerank::RerankTemplate;

use super::condition::FactSource;
use super::overrides::{apply_chain, effective_field, Overridable, Overrides, Preset};
use super::table::{Node, RoutingTable, Strategy, Target, VirtualModel};
use crate::triggers::{linear_escapes, Escape, Triggers};

/// One attempt: a foundation model plus what to do to the request and where
/// to go on failure.
#[derive(Clone)]
pub struct Attempt {
    /// The foundation model id (breaker key, `model_used`).
    pub model_id: String,
    /// The route to this leaf, e.g. `acme/chat>acme/eu>mistral-large`.
    pub path: String,
    /// Override levels, outermost first.
    pub overrides: Vec<Arc<Overrides>>,
    /// Rerank remap through a SystemOne model, if any.
    pub remap: Option<Arc<RerankTemplate>>,
    /// Where to continue on failure, innermost first.
    pub escapes: Vec<Escape>,
}

// Manual Debug: never prints remap text.
impl std::fmt::Debug for Attempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attempt")
            .field("model_id", &self.model_id)
            .field("path", &self.path)
            .field("overrides", &self.overrides.len())
            .field("remap", &self.remap.is_some())
            .field("escapes", &self.escapes)
            .finish()
    }
}

impl Attempt {
    /// A bare attempt on `model_id`. Its `path` stays empty: a route is only
    /// reported for a virtual-model request ([`Decision::route_of`]).
    fn plain(model_id: String) -> Self {
        Self {
            path: String::new(),
            model_id,
            overrides: Vec::new(),
            remap: None,
            escapes: Vec::new(),
        }
    }

    /// Apply this attempt's overrides to its own copy of the request.
    pub fn apply<R: Overridable>(&self, req: &mut R) {
        apply_chain(&self.overrides, req);
    }

    /// The value field `name` takes in this attempt's request, given its
    /// `current` value in the client request, without cloning the request.
    #[must_use]
    pub fn field(
        &self,
        name: &str,
        current: Option<serde_json::Value>,
    ) -> Option<serde_json::Value> {
        effective_field(&self.overrides, name, current)
    }
}

/// The outcome of the decide phase.
#[derive(Debug, Clone)]
pub struct Decision {
    /// The requested id when it is a virtual model.
    pub virtual_model: Option<String>,
    /// The requested virtual model's preset.
    pub preset: Option<Arc<Preset>>,
    /// Ordered attempts; never empty.
    pub attempts: Vec<Attempt>,
}

impl Decision {
    /// A foundation id called directly.
    #[must_use]
    pub fn direct(model: &str) -> Self {
        Self {
            virtual_model: None,
            preset: None,
            attempts: vec![Attempt::plain(model.to_owned())],
        }
    }

    /// A plain chain with the default triggers.
    #[must_use]
    pub fn linear(ids: impl IntoIterator<Item = String>) -> Self {
        let mut attempts: Vec<Attempt> = ids.into_iter().map(Attempt::plain).collect();
        let n = attempts.len();
        for (i, a) in attempts.iter_mut().enumerate() {
            a.escapes = linear_escapes(i, n);
        }
        Self {
            virtual_model: None,
            preset: None,
            attempts,
        }
    }

    /// The first attempt's foundation id (pricing, timeouts, image checks).
    #[must_use]
    pub fn primary_model(&self) -> &str {
        self.attempts.first().map_or("", |a| a.model_id.as_str())
    }

    /// The route reported for the attempt that served, when a virtual model
    /// was requested.
    #[must_use]
    pub fn route_of(&self, index: usize) -> Option<&str> {
        self.virtual_model.as_ref()?;
        self.attempts.get(index).map(|a| a.path.as_str())
    }

    /// Drop the attempts whose `keep` entry is false (the primary is always
    /// kept) and rewire escapes to the next surviving attempt.
    pub fn retain_mask(&mut self, keep: &[bool]) {
        let n = self.attempts.len();
        if (1..n).all(|i| keep.get(i).copied().unwrap_or(true)) {
            return;
        }
        let kept: Vec<bool> = (0..n)
            .map(|i| i == 0 || keep.get(i).copied().unwrap_or(true))
            .collect();
        let mut new_index = vec![usize::MAX; n];
        let mut count = 0;
        for i in 0..n {
            if kept[i] {
                new_index[i] = count;
                count += 1;
            }
        }
        let resolve = |k: usize| (k..n).find(|&j| kept[j]).map(|j| new_index[j]);
        let old = std::mem::take(&mut self.attempts);
        for (i, mut attempt) in old.into_iter().enumerate() {
            if !kept[i] {
                continue;
            }
            let me = new_index[i];
            attempt.escapes = attempt
                .escapes
                .into_iter()
                .filter_map(|e| {
                    resolve(e.next)
                        .filter(|&next| next > me)
                        .map(|next| Escape { on: e.on, next })
                })
                .collect();
            self.attempts.push(attempt);
        }
    }
}

impl RoutingTable {
    /// Decide the attempts for `model`. A foundation id (or an unknown one,
    /// which the registry then rejects) is a direct single attempt.
    ///
    /// # Errors
    /// [`GatewayError::UnsupportedCapability`] when `model` is a virtual
    /// model serving another capability.
    pub fn decide(
        &self,
        capability: Capability,
        model: &str,
        facts: &dyn FactSource,
        rng: &mut dyn FnMut() -> u64,
    ) -> Result<Decision, GatewayError> {
        match self.models.get(model) {
            Some(vm) => vm.decide(capability, facts, rng),
            None => Ok(Decision::direct(model)),
        }
    }
}

impl VirtualModel {
    /// Decide the attempts of a request to this virtual model (the lookup
    /// half of [`RoutingTable::decide`] already done by the caller).
    ///
    /// # Errors
    /// [`GatewayError::UnsupportedCapability`] when it serves another
    /// capability.
    pub fn decide(
        &self,
        capability: Capability,
        facts: &dyn FactSource,
        rng: &mut dyn FnMut() -> u64,
    ) -> Result<Decision, GatewayError> {
        if self.capability() != capability {
            return Err(GatewayError::UnsupportedCapability {
                model: self.id().to_owned(),
                capability,
            });
        }
        let base: Vec<Arc<Overrides>> = self
            .preset
            .as_ref()
            .and_then(|p| p.overrides().cloned())
            .into_iter()
            .collect();
        let attempts = flatten_model(self, facts, rng, "", &base);
        Ok(Decision {
            virtual_model: Some(self.id().to_owned()),
            preset: self.preset.clone(),
            attempts,
        })
    }
}

fn flatten_model(
    vm: &VirtualModel,
    facts: &dyn FactSource,
    rng: &mut dyn FnMut() -> u64,
    prefix: &str,
    overrides: &[Arc<Overrides>],
) -> Vec<Attempt> {
    let path = if prefix.is_empty() {
        vm.id().to_owned()
    } else {
        format!("{prefix}>{}", vm.id())
    };
    match &vm.strategy {
        Strategy::Single => vm
            .targets
            .first()
            .map(|t| flatten_target(t, facts, rng, &path, overrides))
            .unwrap_or_default(),
        Strategy::Switch => vm
            .targets
            .iter()
            .find(|t| t.when.as_ref().is_none_or(|c| c.matches(facts)))
            .map(|t| flatten_target(t, facts, rng, &path, overrides))
            .unwrap_or_default(),
        Strategy::Fallback(on) => chain(
            vm.targets.iter().collect(),
            *on,
            facts,
            rng,
            &path,
            overrides,
        ),
        Strategy::Split { triggers, total } => {
            let r = rng() % (*total).max(1);
            chain(
                split_order(&vm.targets, r),
                *triggers,
                facts,
                rng,
                &path,
                overrides,
            )
        }
    }
}

/// The split pick for draw `r` (in `0..total`), then the other targets by
/// descending weight (ties keep declaration order).
fn split_order(targets: &[Target], r: u64) -> Vec<&Target> {
    let mut acc = 0;
    let picked = targets
        .iter()
        .position(|t| {
            acc += t.weight;
            r < acc
        })
        .unwrap_or(0);
    let mut rest: Vec<&Target> = targets
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != picked)
        .map(|(_, t)| t)
        .collect();
    rest.sort_by_key(|t| std::cmp::Reverse(t.weight));
    let mut order = Vec::with_capacity(targets.len());
    order.extend(targets.get(picked));
    order.extend(rest);
    order
}

fn chain(
    targets: Vec<&Target>,
    on: Triggers,
    facts: &dyn FactSource,
    rng: &mut dyn FnMut() -> u64,
    path: &str,
    overrides: &[Arc<Overrides>],
) -> Vec<Attempt> {
    let n = targets.len();
    let mut out = Vec::new();
    for (i, target) in targets.into_iter().enumerate() {
        let list = flatten_target(target, facts, rng, path, overrides);
        let base = out.len();
        let next = base + list.len();
        for mut attempt in list {
            for escape in &mut attempt.escapes {
                escape.next += base;
            }
            if i + 1 < n {
                attempt.escapes.push(Escape { on, next });
            }
            out.push(attempt);
        }
    }
    out
}

fn flatten_target(
    target: &Target,
    facts: &dyn FactSource,
    rng: &mut dyn FnMut() -> u64,
    path: &str,
    parent: &[Arc<Overrides>],
) -> Vec<Attempt> {
    let mut overrides = parent.to_vec();
    if let Some(level) = &target.overrides {
        overrides.push(level.clone());
    }
    match &target.node {
        Node::Foundation(id) => vec![Attempt {
            model_id: id.clone(),
            path: format!("{path}>{id}"),
            overrides,
            remap: target.remap.clone(),
            escapes: Vec::new(),
        }],
        Node::Virtual(child) => flatten_model(child, facts, rng, path, &overrides),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::triggers::{Trigger, Triggers};
    use crate::virtual_models::table::FoundationIndex;
    use crate::virtual_models::VirtualModelConfig;
    use serde_json::Value;

    struct NoFacts {
        group: Option<&'static str>,
    }
    impl FactSource for NoFacts {
        fn group(&self) -> Option<&str> {
            self.group
        }
        fn metadata(&self, _: &str) -> Option<&Value> {
            None
        }
        fn has_images(&self) -> bool {
            false
        }
        fn has_tools(&self) -> bool {
            false
        }
        fn stream(&self) -> bool {
            false
        }
        fn input_tokens(&self) -> u64 {
            0
        }
        fn documents(&self) -> Option<u64> {
            None
        }
    }

    fn table(toml_text: &str) -> RoutingTable {
        #[derive(serde::Deserialize)]
        struct Doc {
            virtual_models: Vec<VirtualModelConfig>,
        }
        let doc: Doc = toml::from_str(toml_text).unwrap();
        let mut f = FoundationIndex::default();
        for id in ["a", "b", "c", "d"] {
            f.insert(id, vec![Capability::Chat], vec!["text".into()]);
        }
        RoutingTable::compile(&doc.virtual_models, &f).unwrap().0
    }

    fn decide(t: &RoutingTable, model: &str, group: Option<&'static str>, r: u64) -> Decision {
        let mut rng = || r;
        t.decide(Capability::Chat, model, &NoFacts { group }, &mut rng)
            .unwrap()
    }

    fn ids(d: &Decision) -> Vec<&str> {
        d.attempts.iter().map(|a| a.model_id.as_str()).collect()
    }

    #[test]
    fn a_foundation_id_is_a_direct_single_attempt() {
        let d = decide(&RoutingTable::default(), "gpt-4o", None, 0);
        assert_eq!(ids(&d), vec!["gpt-4o"]);
        assert!(d.virtual_model.is_none());
        assert_eq!(d.route_of(0), None);
    }

    #[test]
    fn fallback_is_a_linear_chain_with_paths() {
        let t = table(
            r#"
            [[virtual_models]]
            id = "v"
            capability = "chat"
            strategy = "fallback"
            targets = [{ model = "a" }, { model = "b" }]
        "#,
        );
        let d = decide(&t, "v", None, 0);
        assert_eq!(ids(&d), vec!["a", "b"]);
        assert_eq!(
            d.attempts[0].escapes,
            vec![Escape {
                on: Triggers::DEFAULT,
                next: 1
            }]
        );
        assert!(d.attempts[1].escapes.is_empty());
        assert_eq!(d.route_of(1), Some("v>b"));
    }

    #[test]
    fn split_failover_order_is_pick_then_weight_desc() {
        let t = table(
            r#"
            [[virtual_models]]
            id = "s"
            capability = "chat"
            strategy = "split"
            targets = [{ model = "a", weight = 10 }, { model = "b", weight = 60 }, { model = "c", weight = 30 }]
        "#,
        );
        // r in [0,10) picks a, [10,70) picks b, [70,100) picks c.
        assert_eq!(ids(&decide(&t, "s", None, 5)), vec!["a", "b", "c"]);
        assert_eq!(ids(&decide(&t, "s", None, 15)), vec!["b", "c", "a"]);
        assert_eq!(ids(&decide(&t, "s", None, 99)), vec!["c", "b", "a"]);
        assert_eq!(
            ids(&decide(&t, "s", None, 199)),
            vec!["c", "b", "a"],
            "r is taken modulo the total"
        );
    }

    #[test]
    fn switch_picks_the_first_match_or_the_default() {
        let t = table(
            r#"
            [[virtual_models]]
            id = "w"
            capability = "chat"
            strategy = "switch"
            targets = [{ when = { group = "eu" }, model = "a" }, { model = "b" }]
        "#,
        );
        assert_eq!(ids(&decide(&t, "w", Some("eu"), 0)), vec!["a"]);
        assert_eq!(ids(&decide(&t, "w", Some("us"), 0)), vec!["b"]);
        assert_eq!(ids(&decide(&t, "w", None, 0)), vec!["b"]);
    }

    #[test]
    fn nested_fallback_escapes_to_outer_level() {
        // outer: fallback(default) [inner, d]; inner: fallback(rate_limited) [a, b]
        let t = table(
            r#"
            [[virtual_models]]
            id = "outer"
            capability = "chat"
            strategy = "fallback"
            targets = [{ model = "inner" }, { model = "d" }]
            [[virtual_models]]
            id = "inner"
            capability = "chat"
            strategy = "fallback"
            fallback_on = ["rate_limited"]
            targets = [{ model = "a" }, { model = "b" }]
        "#,
        );
        let d = decide(&t, "outer", None, 0);
        assert_eq!(ids(&d), vec!["a", "b", "d"]);
        let rl = Triggers::from_list(&[Trigger::RateLimited]);
        // a: inner escape (rate limited -> b) first, then outer escape (default -> d).
        assert_eq!(
            d.attempts[0].escapes,
            vec![
                Escape { on: rl, next: 1 },
                Escape {
                    on: Triggers::DEFAULT,
                    next: 2
                }
            ]
        );
        assert_eq!(
            d.attempts[1].escapes,
            vec![Escape {
                on: Triggers::DEFAULT,
                next: 2
            }]
        );
        assert_eq!(d.route_of(0), Some("outer>inner>a"));
    }

    #[test]
    fn wrong_capability_is_lm_2002() {
        let t = table(
            r#"
            [[virtual_models]]
            id = "v"
            capability = "chat"
            strategy = "single"
            targets = [{ model = "a" }]
        "#,
        );
        let mut rng = || 0;
        let err = t
            .decide(Capability::Embed, "v", &NoFacts { group: None }, &mut rng)
            .unwrap_err();
        assert_eq!(err.code(), "LM-2002");
    }

    #[test]
    fn overrides_chain_outer_to_inner_with_the_preset_first() {
        let t = table(
            r#"
            [[virtual_models]]
            id = "outer"
            capability = "chat"
            strategy = "single"
            preset = { overrides = { set = { max_tokens = 1 } } }
            targets = [{ model = "inner", overrides = { set = { max_tokens = 2 } } }]
            [[virtual_models]]
            id = "inner"
            capability = "chat"
            strategy = "single"
            targets = [{ model = "a", overrides = { set = { max_tokens = 3 } } }]
        "#,
        );
        let d = decide(&t, "outer", None, 0);
        assert_eq!(d.attempts[0].overrides.len(), 3);
        let mut req: lumen_core::ChatRequest =
            serde_json::from_value(serde_json::json!({ "model": "m", "messages": [] })).unwrap();
        d.attempts[0].apply(&mut req);
        assert_eq!(req.max_tokens, Some(3));
        assert!(d.preset.is_some());
    }

    #[test]
    fn retain_keeping_everything_changes_nothing() {
        let mut d = Decision::linear(["a", "b", "c"].map(str::to_owned));
        let before: Vec<Vec<Escape>> = d.attempts.iter().map(|a| a.escapes.clone()).collect();
        d.retain_mask(&[true, true, true]);
        d.retain_mask(&[]);
        assert_eq!(ids(&d), vec!["a", "b", "c"]);
        let after: Vec<Vec<Escape>> = d.attempts.iter().map(|a| a.escapes.clone()).collect();
        assert_eq!(after, before);
    }

    #[test]
    fn retain_drops_attempts_and_rewires_escapes() {
        let mut d = Decision::linear(["a", "b", "c"].map(str::to_owned));
        d.retain_mask(&[true, false, true]);
        assert_eq!(ids(&d), vec!["a", "c"]);
        assert_eq!(
            d.attempts[0].escapes,
            vec![Escape {
                on: Triggers::DEFAULT,
                next: 1
            }]
        );
        let mut d = Decision::linear(["a", "b"].map(str::to_owned));
        d.retain_mask(&[true, false]);
        assert!(
            d.attempts[0].escapes.is_empty(),
            "an escape past the end is dropped"
        );
        let mut d = Decision::linear(["a", "b"].map(str::to_owned));
        d.retain_mask(&[false, true]);
        assert_eq!(ids(&d), vec!["a", "b"], "the primary is never removed");
    }
}
