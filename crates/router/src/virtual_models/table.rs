//! Compiling `[[virtual_models]]` into an immutable [`RoutingTable`]
//! (ADR 014). Every rule of the spec's load-time validation lives here, so a
//! boot, a hot reload and an admin write all reject the same configs with
//! the same messages.

use std::collections::HashMap;
use std::sync::Arc;

use lumen_core::systemone::MAX_SCORE_LEVELS;
use lumen_core::Capability;
use lumen_providers::typesafe::rerank::{
    CompositeQuestion, RerankStrategy, RerankTemplate, DEFAULT_CHOICE_INSTRUCTIONS,
    DEFAULT_CRITERIA_FALSE, DEFAULT_CRITERIA_TRUE, DEFAULT_INSTRUCTIONS,
    DEFAULT_SCORE_INSTRUCTIONS, MAX_COMPOSITE_QUESTIONS,
};
use serde_json::Value;

use super::condition::Condition;
use super::config::{
    CriteriaConfig, RemapConfig, RemapStrategy, StrategyKind, TargetConfig, VirtualModelConfig,
};
use super::overrides::{Overrides, Preset};
use crate::triggers::Triggers;

/// Most virtual models on one path from a requested id to a foundation leaf.
pub const MAX_DEPTH: usize = 8;

/// Most attempts one request to a virtual model can flatten into. A
/// `fallback` or `split` counts the attempts of every target, a `switch` only
/// its largest branch (one branch is taken per request) and a foundation
/// target counts 1. Bounds the per-request decide work of a DAG that lists
/// the same child several times per level.
pub const MAX_ATTEMPTS: usize = 64;

/// What compilation needs to know about the foundation models.
#[derive(Debug, Clone, Default)]
pub struct FoundationIndex {
    models: HashMap<String, FoundationModel>,
}

#[derive(Debug, Clone)]
struct FoundationModel {
    capabilities: Vec<Capability>,
    modalities: Vec<String>,
}

impl FoundationIndex {
    /// Record one foundation model.
    pub fn insert(
        &mut self,
        id: impl Into<String>,
        capabilities: Vec<Capability>,
        modalities: Vec<String>,
    ) {
        self.models.insert(
            id.into(),
            FoundationModel {
                capabilities,
                modalities,
            },
        );
    }

    /// Whether `id` is a foundation model.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.models.contains_key(id)
    }
}

/// A virtual-model config that failed validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("virtual model '{model}': {message}")]
pub struct RoutingConfigError {
    /// The virtual model at fault.
    pub model: String,
    /// What is wrong.
    pub message: String,
}

/// A compiled virtual model.
pub struct VirtualModel {
    pub(crate) config: VirtualModelConfig,
    pub(crate) strategy: Strategy,
    pub(crate) targets: Vec<Target>,
    pub(crate) preset: Option<Arc<Preset>>,
    modalities: Vec<String>,
    /// Most virtual models on any path from this one down to a foundation
    /// leaf, itself included (1 when every target is a foundation model).
    height: usize,
    /// Most attempts a request to this model flattens into (at most
    /// [`MAX_ATTEMPTS`]).
    attempts: usize,
}

impl std::fmt::Debug for VirtualModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualModel")
            .field("id", &self.config.id)
            .field("capability", &self.config.capability)
            .field("strategy", &self.config.strategy)
            .finish_non_exhaustive()
    }
}

impl VirtualModel {
    /// The chat preset of this virtual model, when it declares one. It depends
    /// only on the requested id, so a caller can read it before routing (its
    /// prompt counts toward the `input_tokens` fact).
    #[must_use]
    pub fn preset(&self) -> Option<&Arc<Preset>> {
        self.preset.as_ref()
    }

    /// The public id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.config.id
    }
    /// The one capability it serves.
    #[must_use]
    pub fn capability(&self) -> Capability {
        self.config.capability
    }
    /// Operator description, if any.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.config.description.as_deref()
    }
    /// Whether `GET /v1/models` lists it.
    #[must_use]
    pub fn listed(&self) -> bool {
        self.config.listed
    }
    /// Input modalities every reachable leaf accepts.
    #[must_use]
    pub fn modalities(&self) -> &[String] {
        &self.modalities
    }
}

pub(crate) enum Strategy {
    Single,
    Fallback(Triggers),
    Split { triggers: Triggers, total: u64 },
    Switch,
}

pub(crate) struct Target {
    pub(crate) node: Node,
    pub(crate) weight: u64,
    pub(crate) when: Option<Condition>,
    pub(crate) overrides: Option<Arc<Overrides>>,
    pub(crate) remap: Option<Arc<RerankTemplate>>,
}

pub(crate) enum Node {
    Foundation(String),
    Virtual(Arc<VirtualModel>),
}

/// Every virtual model, compiled.
#[derive(Debug, Default)]
pub struct RoutingTable {
    pub(crate) models: HashMap<String, Arc<VirtualModel>>,
    order: Vec<Arc<VirtualModel>>,
}

impl RoutingTable {
    /// Validate and compile `configs`. Returns the table and load warnings.
    ///
    /// # Errors
    /// The first [`RoutingConfigError`] found.
    pub fn compile(
        configs: &[VirtualModelConfig],
        foundation: &FoundationIndex,
    ) -> Result<(Self, Vec<String>), RoutingConfigError> {
        let mut by_id: HashMap<&str, &VirtualModelConfig> = HashMap::with_capacity(configs.len());
        for config in configs {
            let err = |message: String| RoutingConfigError {
                model: config.id.clone(),
                message,
            };
            validate_id(&config.id).map_err(err)?;
            if foundation.contains(&config.id) {
                return Err(err(
                    "collides with a foundation model of the same id (ids are one namespace)"
                        .to_owned(),
                ));
            }
            if by_id.insert(config.id.as_str(), config).is_some() {
                return Err(err("is declared twice".to_owned()));
            }
        }
        let mut compiler = Compiler {
            by_id: &by_id,
            foundation,
            done: HashMap::new(),
            stack: Vec::new(),
        };
        let mut order = Vec::with_capacity(configs.len());
        for config in configs {
            order.push(compiler.compile(&config.id)?);
        }
        let warnings = preset_warnings(&order);
        let models = order
            .iter()
            .map(|m| (m.config.id.clone(), m.clone()))
            .collect();
        Ok((Self { models, order }, warnings))
    }

    /// A virtual model by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Arc<VirtualModel>> {
        self.models.get(id)
    }

    /// Listed virtual models, in config order.
    pub fn listed(&self) -> impl Iterator<Item = &Arc<VirtualModel>> {
        self.order.iter().filter(|m| m.listed())
    }

    /// Whether no virtual model is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The fully resolved tree of `id`, for `GET /admin/config/virtual_models/{id}/plan`.
    #[must_use]
    pub fn plan(&self, id: &str) -> Option<Value> {
        self.models.get(id).map(|m| plan_of(m))
    }
}

struct Compiler<'a> {
    by_id: &'a HashMap<&'a str, &'a VirtualModelConfig>,
    foundation: &'a FoundationIndex,
    done: HashMap<String, Arc<VirtualModel>>,
    stack: Vec<String>,
}

impl Compiler<'_> {
    fn compile(&mut self, id: &str) -> Result<Arc<VirtualModel>, RoutingConfigError> {
        if let Some(done) = self.done.get(id) {
            return Ok(done.clone());
        }
        if let Some(pos) = self.stack.iter().position(|s| s == id) {
            let mut path = self.stack[pos..].to_vec();
            path.push(id.to_owned());
            return Err(RoutingConfigError {
                model: self.stack[pos].clone(),
                message: format!("cycle: {}", path.join(" -> ")),
            });
        }
        let err = |message: String| RoutingConfigError {
            model: id.to_owned(),
            message,
        };
        let Some(config) = self.by_id.get(id).copied() else {
            return Err(err("unknown virtual model".to_owned()));
        };
        validate_shape(config).map_err(err)?;

        // Bound the recursion before descending: a chain of thousands of
        // models would otherwise overflow the stack (an abort, not an error).
        // Reported on the outermost model of the path, as the height check
        // below does for memoised children.
        if self.stack.len() >= MAX_DEPTH {
            return Err(RoutingConfigError {
                model: self.stack[0].clone(),
                message: format!("nesting is deeper than {MAX_DEPTH} virtual models"),
            });
        }
        self.stack.push(id.to_owned());
        let mut targets = Vec::with_capacity(config.targets.len());
        let mut modalities: Option<Vec<String>> = None;
        for target in &config.targets {
            let (compiled, mods) = self.target(config, target)?;
            modalities = Some(match modalities {
                None => mods,
                Some(acc) => acc.into_iter().filter(|m| mods.contains(m)).collect(),
            });
            targets.push(compiled);
        }
        self.stack.pop();

        // The height comes from the compiled children, so the depth limit
        // does not depend on the order the models are declared in.
        let height = 1 + targets
            .iter()
            .filter_map(|t| match &t.node {
                Node::Virtual(child) => Some(child.height),
                Node::Foundation(_) => None,
            })
            .max()
            .unwrap_or(0);
        if height > MAX_DEPTH {
            return Err(err(format!(
                "nesting is deeper than {MAX_DEPTH} virtual models"
            )));
        }
        let attempts = attempt_count(config.strategy, &targets);
        if attempts > MAX_ATTEMPTS {
            return Err(err(format!(
                "expands to {attempts} attempts per request; the limit is {MAX_ATTEMPTS}"
            )));
        }

        let triggers = config
            .fallback_on
            .as_deref()
            .map_or(Triggers::DEFAULT, Triggers::from_list);
        let strategy = match config.strategy {
            StrategyKind::Single => Strategy::Single,
            StrategyKind::Fallback => Strategy::Fallback(triggers),
            StrategyKind::Split => Strategy::Split {
                triggers,
                total: targets.iter().map(|t| t.weight).sum(),
            },
            StrategyKind::Switch => Strategy::Switch,
        };
        let preset = config
            .preset
            .as_ref()
            .map(Preset::compile)
            .transpose()
            .map_err(err)?
            .map(Arc::new);
        let model = Arc::new(VirtualModel {
            config: config.clone(),
            strategy,
            targets,
            preset,
            modalities: modalities.unwrap_or_default(),
            height,
            attempts,
        });
        self.done.insert(id.to_owned(), model.clone());
        Ok(model)
    }

    fn target(
        &mut self,
        parent: &VirtualModelConfig,
        t: &TargetConfig,
    ) -> Result<(Target, Vec<String>), RoutingConfigError> {
        let capability = parent.capability;
        let err = |message: String| RoutingConfigError {
            model: parent.id.clone(),
            message: format!("target '{}': {message}", t.model),
        };
        let when = t
            .when
            .as_ref()
            .map(|w| Condition::compile(w, capability))
            .transpose()
            .map_err(err)?;
        let overrides = t
            .overrides
            .as_ref()
            .map(|o| Overrides::compile(o, capability))
            .transpose()
            .map_err(err)?
            .map(Arc::new);
        let weight = u64::from(t.weight.unwrap_or(0));

        if self.by_id.contains_key(t.model.as_str()) {
            if t.remap.is_some() {
                return Err(err(
                    "a remap must point directly at a foundation model".to_owned()
                ));
            }
            let child = self.compile(&t.model)?;
            if child.capability() != capability {
                return Err(err(format!(
                    "serves {} but this virtual model serves {capability}",
                    child.capability()
                )));
            }
            let mods = child.modalities.clone();
            return Ok((
                Target {
                    node: Node::Virtual(child),
                    weight,
                    when,
                    overrides,
                    remap: None,
                },
                mods,
            ));
        }

        let Some(found) = self.foundation.models.get(&t.model) else {
            return Err(RoutingConfigError {
                model: parent.id.clone(),
                message: format!(
                    "unknown model '{}' (neither a foundation nor a virtual model)",
                    t.model
                ),
            });
        };
        let (remap, mods) = if let Some(remap) = &t.remap {
            if capability != Capability::Rerank {
                return Err(err(
                    "`remap` is only valid on a rerank virtual model".to_owned()
                ));
            }
            if !found.capabilities.contains(&Capability::SystemOne) {
                return Err(err("a remap target must serve systemone".to_owned()));
            }
            let template = compile_remap(remap).map_err(err)?;
            (Some(Arc::new(template)), vec!["text".to_owned()])
        } else {
            if !found.capabilities.contains(&capability) {
                return Err(err(format!("does not serve {capability}")));
            }
            (None, found.modalities.clone())
        };
        Ok((
            Target {
                node: Node::Foundation(t.model.clone()),
                weight,
                when,
                overrides,
                remap,
            },
            mods,
        ))
    }
}

/// Attempts a request flattens into, from the compiled (memoised) children.
/// Saturates so a wide DAG cannot overflow before the limit check.
fn attempt_count(strategy: StrategyKind, targets: &[Target]) -> usize {
    let each = targets.iter().map(|t| match &t.node {
        Node::Foundation(_) => 1,
        Node::Virtual(child) => child.attempts,
    });
    match strategy {
        StrategyKind::Switch => each.max().unwrap_or(0),
        StrategyKind::Single | StrategyKind::Fallback | StrategyKind::Split => {
            each.fold(0, usize::saturating_add)
        }
    }
}

fn validate_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'));
    if ok {
        Ok(())
    } else {
        Err("id must be non-empty and use only the characters A-Z a-z 0-9 . _ : / -".to_owned())
    }
}

fn validate_shape(c: &VirtualModelConfig) -> Result<(), String> {
    let n = c.targets.len();
    let strategy = c.strategy.as_str();
    match c.strategy {
        StrategyKind::Single if n != 1 => {
            return Err("strategy `single` takes exactly 1 target".to_owned())
        }
        StrategyKind::Fallback | StrategyKind::Split | StrategyKind::Switch if n < 2 => {
            return Err(format!("strategy `{strategy}` takes at least 2 targets"))
        }
        _ => {}
    }
    for (i, t) in c.targets.iter().enumerate() {
        match (c.strategy, t.weight) {
            (StrategyKind::Split, None | Some(0)) => {
                return Err("every split target needs a weight > 0".to_owned())
            }
            (StrategyKind::Split, Some(_)) | (_, None) => {}
            (_, Some(_)) => return Err("`weight` is only valid under strategy `split`".to_owned()),
        }
        let last = i + 1 == n;
        match (c.strategy, t.when.is_some(), last) {
            (StrategyKind::Switch, false, false) => {
                return Err("every switch target but the last needs a `when`".to_owned())
            }
            (StrategyKind::Switch, true, true) => {
                return Err(
                    "the last switch target is the default and must not have a `when`".to_owned(),
                )
            }
            (StrategyKind::Switch, _, _) | (_, false, _) => {}
            (_, true, _) => return Err("`when` is only valid under strategy `switch`".to_owned()),
        }
    }
    if let Some(list) = &c.fallback_on {
        if !matches!(c.strategy, StrategyKind::Fallback | StrategyKind::Split) {
            return Err(
                "`fallback_on` is only valid under strategy `fallback` or `split`".to_owned(),
            );
        }
        if list.is_empty() {
            return Err("`fallback_on` must not be empty (omit it for the defaults)".to_owned());
        }
    }
    if c.preset.is_some() && c.capability != Capability::Chat {
        return Err("`preset` is only valid on a chat virtual model".to_owned());
    }
    if c.description
        .as_deref()
        .is_some_and(|d| d.trim().is_empty())
    {
        return Err("`description` must not be blank".to_owned());
    }
    Ok(())
}

fn is_blank(s: Option<&str>) -> bool {
    s.is_some_and(|t| t.trim().is_empty())
}

/// Fill the yes/no criteria of a remap question with the defaults.
fn resolve_criteria(c: Option<&CriteriaConfig>) -> Result<(String, String), String> {
    let yes = c.and_then(|c| c.yes.as_deref());
    let no = c.and_then(|c| c.no.as_deref());
    if is_blank(yes) || is_blank(no) {
        return Err("remap criteria must not be blank".to_owned());
    }
    Ok((
        yes.unwrap_or(DEFAULT_CRITERIA_TRUE).to_owned(),
        no.unwrap_or(DEFAULT_CRITERIA_FALSE).to_owned(),
    ))
}

fn compile_remap(r: &RemapConfig) -> Result<RerankTemplate, String> {
    if is_blank(r.context.as_deref()) || is_blank(r.instructions.as_deref()) {
        return Err("remap fields must not be blank".to_owned());
    }
    let strategy_name = match r.strategy {
        RemapStrategy::Noul => "noul",
        RemapStrategy::Score => "score",
        RemapStrategy::Composite => "composite",
        RemapStrategy::Choice => "choice",
    };
    let reject = |present: bool, field: &str| {
        if present {
            Err(format!(
                "`{field}` is not valid for strategy `{strategy_name}`"
            ))
        } else {
            Ok(())
        }
    };
    let instructions = |default: &str| r.instructions.clone().unwrap_or_else(|| default.to_owned());

    let strategy = match r.strategy {
        RemapStrategy::Noul => {
            reject(r.levels.is_some(), "levels")?;
            reject(r.questions.is_some(), "questions")?;
            let (criteria_true, criteria_false) = resolve_criteria(r.criteria.as_ref())?;
            RerankStrategy::Noul {
                instructions: instructions(DEFAULT_INSTRUCTIONS),
                criteria_true,
                criteria_false,
            }
        }
        RemapStrategy::Score => {
            reject(r.criteria.is_some(), "criteria")?;
            reject(r.questions.is_some(), "questions")?;
            let levels = r.levels.clone().ok_or("strategy `score` needs `levels`")?;
            if !(2..=MAX_SCORE_LEVELS).contains(&levels.len()) {
                return Err(format!(
                    "`levels` must hold 2 to {MAX_SCORE_LEVELS} entries"
                ));
            }
            if levels.iter().any(|l| l.trim().is_empty()) {
                return Err("`levels` must not contain blank entries".to_owned());
            }
            RerankStrategy::Score {
                instructions: instructions(DEFAULT_SCORE_INSTRUCTIONS),
                levels,
            }
        }
        RemapStrategy::Composite => {
            reject(r.instructions.is_some(), "instructions")?;
            reject(r.criteria.is_some(), "criteria")?;
            reject(r.levels.is_some(), "levels")?;
            let questions = r
                .questions
                .as_ref()
                .ok_or("strategy `composite` needs `questions`")?;
            if !(1..=MAX_COMPOSITE_QUESTIONS).contains(&questions.len()) {
                return Err(format!(
                    "`questions` must hold 1 to {MAX_COMPOSITE_QUESTIONS} entries"
                ));
            }
            let mut compiled = Vec::with_capacity(questions.len());
            for q in questions {
                if q.instructions.trim().is_empty() {
                    return Err("composite question instructions must not be blank".to_owned());
                }
                if !q.weight.is_finite() || q.weight <= 0.0 {
                    return Err("composite question weight must be a finite number > 0".to_owned());
                }
                let (criteria_true, criteria_false) = resolve_criteria(q.criteria.as_ref())?;
                compiled.push(CompositeQuestion {
                    instructions: q.instructions.clone(),
                    criteria_true,
                    criteria_false,
                    weight: q.weight,
                });
            }
            RerankStrategy::Composite {
                questions: compiled,
            }
        }
        RemapStrategy::Choice => {
            reject(r.criteria.is_some(), "criteria")?;
            reject(r.levels.is_some(), "levels")?;
            reject(r.questions.is_some(), "questions")?;
            RerankStrategy::Choice {
                instructions: instructions(DEFAULT_CHOICE_INSTRUCTIONS),
            }
        }
    };
    Ok(RerankTemplate {
        context: r.context.clone(),
        strategy,
    })
}

fn preset_warnings(order: &[Arc<VirtualModel>]) -> Vec<String> {
    let mut warnings = Vec::new();
    for parent in order {
        for target in &parent.targets {
            if let Node::Virtual(child) = &target.node {
                if child.preset.is_some() {
                    warnings.push(format!(
                        "the preset on virtual model '{}' is ignored when it is reached through \
                         '{}' (presets apply only to the requested id)",
                        child.id(),
                        parent.id()
                    ));
                }
            }
        }
    }
    warnings
}

fn plan_of(model: &VirtualModel) -> Value {
    let mut head = serde_json::to_value(&model.config).unwrap_or(Value::Null);
    let targets: Vec<Value> = model
        .config
        .targets
        .iter()
        .zip(&model.targets)
        .map(|(cfg, compiled)| {
            let mut entry = serde_json::to_value(cfg).unwrap_or(Value::Null);
            if let Value::Object(map) = &mut entry {
                match &compiled.node {
                    Node::Virtual(child) => {
                        map.insert("virtual".to_owned(), Value::Bool(true));
                        map.insert("plan".to_owned(), plan_of(child));
                    }
                    Node::Foundation(_) => {
                        map.insert("virtual".to_owned(), Value::Bool(false));
                    }
                }
            }
            entry
        })
        .collect();
    if let Value::Object(map) = &mut head {
        map.insert("targets".to_owned(), Value::Array(targets));
        if let Strategy::Fallback(t) | Strategy::Split { triggers: t, .. } = &model.strategy {
            map.insert(
                "fallback_on".to_owned(),
                serde_json::to_value(t.to_list()).unwrap_or(Value::Null),
            );
        }
    }
    head
}

#[cfg(test)]
mod tests {
    use super::*;

    fn foundation() -> FoundationIndex {
        let mut f = FoundationIndex::default();
        f.insert(
            "gpt-4o",
            vec![Capability::Chat],
            vec!["text".into(), "image".into()],
        );
        f.insert("claude", vec![Capability::Chat], vec!["text".into()]);
        f.insert(
            "rerank-english",
            vec![Capability::Rerank],
            vec!["text".into()],
        );
        f.insert("jev", vec![Capability::SystemOne], vec!["text".into()]);
        f
    }

    fn compile(toml_text: &str) -> Result<(RoutingTable, Vec<String>), RoutingConfigError> {
        #[derive(serde::Deserialize)]
        struct Doc {
            virtual_models: Vec<VirtualModelConfig>,
        }
        let doc: Doc = toml::from_str(toml_text).unwrap();
        RoutingTable::compile(&doc.virtual_models, &foundation())
    }

    fn err(toml_text: &str) -> String {
        compile(toml_text).map(|_| ()).unwrap_err().to_string()
    }

    #[test]
    fn a_valid_composed_table_compiles() {
        let (table, warnings) = compile(
            r#"
            [[virtual_models]]
            id = "acme/chat"
            capability = "chat"
            strategy = "switch"
            targets = [ { when = { group = "eu" }, model = "acme/eu" }, { model = "gpt-4o" } ]

            [[virtual_models]]
            id = "acme/eu"
            capability = "chat"
            strategy = "split"
            listed = false
            targets = [ { model = "gpt-4o", weight = 80 }, { model = "claude", weight = 20 } ]
        "#,
        )
        .unwrap();
        assert!(warnings.is_empty());
        assert_eq!(
            table.get("acme/chat").unwrap().capability(),
            Capability::Chat
        );
        assert_eq!(table.listed().count(), 1);
        // modalities are the intersection over reachable leaves
        assert_eq!(
            table.get("acme/eu").unwrap().modalities(),
            &["text".to_owned()]
        );
    }

    #[test]
    fn shape_errors() {
        let base = |strategy: &str, targets: &str| {
            format!(
            "[[virtual_models]]\nid = \"v\"\ncapability = \"chat\"\nstrategy = \"{strategy}\"\ntargets = {targets}\n")
        };
        assert!(
            err(&base("single", r#"[{model="gpt-4o"},{model="claude"}]"#)).contains("exactly 1")
        );
        assert!(err(&base("fallback", r#"[{model="gpt-4o"}]"#)).contains("at least 2"));
        assert!(err(&base(
            "split",
            r#"[{model="gpt-4o",weight=1},{model="claude"}]"#
        ))
        .contains("weight > 0"));
        assert!(err(&base(
            "fallback",
            r#"[{model="gpt-4o",weight=1},{model="claude"}]"#
        ))
        .contains("only valid under strategy `split`"));
        assert!(
            err(&base("switch", r#"[{model="gpt-4o"},{model="claude"}]"#))
                .contains("needs a `when`")
        );
        assert!(err(&base(
            "switch",
            r#"[{model="gpt-4o",when={stream=true}},{model="claude",when={stream=false}}]"#
        ))
        .contains("default"));
        assert!(err(&base(
            "fallback",
            r#"[{model="gpt-4o",when={stream=true}},{model="claude"}]"#
        ))
        .contains("only valid under strategy `switch`"));
    }

    /// `len` single-strategy chat models `v0 -> v1 -> ... -> gpt-4o`.
    fn chain_toml(len: usize, reverse: bool) -> String {
        let mut blocks: Vec<String> = (0..len)
            .map(|i| {
                let target = if i + 1 == len {
                    "gpt-4o".to_owned()
                } else {
                    format!("v{}", i + 1)
                };
                format!(
                    "[[virtual_models]]\nid = \"v{i}\"\ncapability = \"chat\"\nstrategy = \"single\"\ntargets = [{{ model = \"{target}\" }}]\n"
                )
            })
            .collect();
        if reverse {
            blocks.reverse();
        }
        blocks.concat()
    }

    #[test]
    fn reference_capability_cycle_and_depth_errors() {
        assert!(err(r#"
            [[virtual_models]]
            id = "v"
            capability = "chat"
            strategy = "single"
            targets = [{ model = "ghost" }]
        "#)
        .contains("unknown model 'ghost'"));
        assert!(err(r#"
            [[virtual_models]]
            id = "v"
            capability = "chat"
            strategy = "single"
            targets = [{ model = "rerank-english" }]
        "#)
        .contains("does not serve chat"));
        let cycle = err(r#"
            [[virtual_models]]
            id = "a"
            capability = "chat"
            strategy = "single"
            targets = [{ model = "b" }]
            [[virtual_models]]
            id = "b"
            capability = "chat"
            strategy = "single"
            targets = [{ model = "a" }]
        "#);
        assert!(cycle.contains("cycle: a -> b -> a"), "{cycle}");

        // The limit is on the longest path, whatever the declaration order.
        for reverse in [false, true] {
            let too_deep = err(&chain_toml(9, reverse));
            assert!(too_deep.contains("deeper than 8"), "{too_deep}");
            assert!(compile(&chain_toml(8, reverse)).is_ok());
        }
    }

    #[test]
    fn a_very_long_chain_is_rejected_without_overflowing_the_stack() {
        for reverse in [false, true] {
            let too_deep = err(&chain_toml(2_000, reverse));
            assert!(too_deep.contains("deeper than 8"), "{too_deep}");
        }
    }

    /// `depth` levels of `fallback` models, each listing the level below
    /// `width` times; the bottom level lists `gpt-4o` `width` times.
    fn fan_out_toml(depth: usize, width: usize) -> String {
        (0..depth)
            .map(|i| {
                let target = if i + 1 == depth {
                    "gpt-4o".to_owned()
                } else {
                    format!("l{}", i + 1)
                };
                let targets = vec![format!("{{ model = \"{target}\" }}"); width].join(", ");
                format!(
                    "[[virtual_models]]\nid = \"l{i}\"\ncapability = \"chat\"\nstrategy = \"fallback\"\ntargets = [{targets}]\n"
                )
            })
            .collect::<Vec<String>>()
            .concat()
    }

    #[test]
    fn the_flattened_attempt_count_is_capped() {
        // 4 x 4 x 4 = 64 attempts: exactly the limit.
        assert!(compile(&fan_out_toml(3, 4)).is_ok());
        // 4^4 = 256 attempts.
        let e = err(&fan_out_toml(4, 4));
        assert!(e.contains("virtual model 'l0'"), "{e}");
        assert!(
            e.contains("expands to 256 attempts per request; the limit is 64"),
            "{e}"
        );
        // A switch takes one branch, so it counts its largest branch only.
        let switch = format!(
            "{}[[virtual_models]]\nid = \"w\"\ncapability = \"chat\"\nstrategy = \"switch\"\n\
             targets = [{{ when = {{ group = \"a\" }}, model = \"l0\" }}, {{ when = {{ group = \"b\" }}, model = \"l0\" }}, {{ model = \"l0\" }}]\n",
            fan_out_toml(3, 4)
        );
        assert!(compile(&switch).is_ok());
        // A fallback over the same 64-attempt child twice is 128.
        let doubled = format!(
            "{}[[virtual_models]]\nid = \"f\"\ncapability = \"chat\"\nstrategy = \"fallback\"\n\
             targets = [{{ model = \"l0\" }}, {{ model = \"l0\" }}]\n",
            fan_out_toml(3, 4)
        );
        assert!(err(&doubled).contains("expands to 128 attempts"));
    }

    #[test]
    fn namespace_and_id_errors() {
        assert!(err(r#"
            [[virtual_models]]
            id = "gpt-4o"
            capability = "chat"
            strategy = "single"
            targets = [{ model = "claude" }]
        "#)
        .contains("collides with a foundation model"));
        assert!(err(r#"
            [[virtual_models]]
            id = "bad id!"
            capability = "chat"
            strategy = "single"
            targets = [{ model = "claude" }]
        "#)
        .contains("characters"));
    }

    #[test]
    fn remap_rules() {
        let (table, _) = compile(r#"
            [[virtual_models]]
            id = "acme/rerank"
            capability = "rerank"
            strategy = "fallback"
            targets = [ { model = "jev", remap = { strategy = "score", levels = ["no", "partly", "yes"] } },
                        { model = "rerank-english" } ]
        "#).unwrap();
        assert!(table.get("acme/rerank").is_some());

        let e = |remap: &str| {
            err(&format!(
                r#"
            [[virtual_models]]
            id = "r"
            capability = "rerank"
            strategy = "single"
            targets = [{{ model = "jev", remap = {remap} }}]
        "#
            ))
        };
        assert!(e(r#"{ strategy = "score" }"#).contains("levels"));
        assert!(e(r#"{ strategy = "score", levels = ["only"] }"#).contains("2 to 10"));
        assert!(e(r#"{ strategy = "noul", levels = ["a","b"] }"#)
            .contains("not valid for strategy `noul`"));
        assert!(e(r#"{ strategy = "composite" }"#).contains("questions"));
        assert!(e(
            r#"{ strategy = "composite", questions = [{ instructions = "q", weight = 0.0 }] }"#
        )
        .contains("weight"));
        assert!(e(r#"{ instructions = " " }"#).contains("blank"));
        assert!(err(r#"
            [[virtual_models]]
            id = "r"
            capability = "rerank"
            strategy = "single"
            targets = [{ model = "rerank-english", remap = {} }]
        "#)
        .contains("must serve systemone"));
        assert!(err(r#"
            [[virtual_models]]
            id = "c"
            capability = "chat"
            strategy = "single"
            targets = [{ model = "gpt-4o", remap = {} }]
        "#)
        .contains("only valid on a rerank"));
    }

    #[test]
    fn presets_reached_through_composition_warn() {
        let (_, warnings) = compile(
            r#"
            [[virtual_models]]
            id = "outer"
            capability = "chat"
            strategy = "single"
            targets = [{ model = "inner" }]
            [[virtual_models]]
            id = "inner"
            capability = "chat"
            strategy = "single"
            preset = { system_prompt = "P" }
            targets = [{ model = "gpt-4o" }]
        "#,
        )
        .unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("inner") && warnings[0].contains("outer"));
    }

    #[test]
    fn plan_expands_references_and_shows_default_triggers() {
        let (table, _) = compile(
            r#"
            [[virtual_models]]
            id = "outer"
            capability = "chat"
            strategy = "fallback"
            targets = [{ model = "inner" }, { model = "claude" }]
            [[virtual_models]]
            id = "inner"
            capability = "chat"
            strategy = "single"
            targets = [{ model = "gpt-4o" }]
        "#,
        )
        .unwrap();
        let plan = table.plan("outer").unwrap();
        assert_eq!(
            plan["fallback_on"],
            serde_json::json!(["provider_error", "rate_limited", "timeout", "circuit_open"])
        );
        assert_eq!(plan["targets"][0]["virtual"], serde_json::json!(true));
        assert_eq!(
            plan["targets"][0]["plan"]["targets"][0]["model"],
            serde_json::json!("gpt-4o")
        );
        assert_eq!(plan["targets"][1]["virtual"], serde_json::json!(false));
    }
}
