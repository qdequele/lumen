//! `lumen config migrate` (ADR 014): rewrite a config document that still
//! uses per-model `fallbacks` or the Jev `[providers.models.rerank]` block
//! into foundation models plus `[[virtual_models]]`. Format preserving
//! outside the edited tables; the result is validated by the caller.

use std::collections::{HashMap, HashSet};

use lumen_core::Capability;
use lumen_providers::ProviderKind;
use lumen_router::virtual_models::config::{
    CriteriaConfig, RemapConfig, StrategyKind, TargetConfig, VirtualModelConfig,
};
use toml_edit::{DocumentMut, Item};

use crate::config::{Config, ModelConfig, ProviderConfig};
use crate::config_edit::{normalize_to_array_of_tables, upsert_virtual_model, EditError};

/// The result of a migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    /// The migrated document.
    pub text: String,
    /// Whether anything changed.
    pub changed: bool,
    /// Things the operator should review.
    pub notes: Vec<String>,
}

/// Why a document could not be migrated.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    /// The document does not parse.
    #[error("config document does not parse: {0}")]
    Parse(String),
    /// A computed id already exists.
    #[error("cannot migrate: {0}")]
    Conflict(String),
    /// A document edit failed.
    #[error(transparent)]
    Edit(#[from] EditError),
}

/// A foundation model to add.
struct Created {
    provider: usize,
    model: ModelConfig,
}

/// Every change, computed from the parsed config before touching the text.
#[derive(Default)]
struct Plan {
    /// old id -> new foundation id.
    renames: HashMap<String, String>,
    /// (provider index, model index) of tables to delete.
    removals: Vec<(usize, usize)>,
    /// (provider index, model index) -> capabilities to keep (a Jev reranker
    /// that also serves systemone loses `rerank`).
    recapabilities: HashMap<(usize, usize), Vec<Capability>>,
    created: Vec<Created>,
    virtuals: Vec<VirtualModelConfig>,
    notes: Vec<String>,
}

fn is_legacy_rerank(provider: &ProviderConfig, model: &ModelConfig) -> bool {
    provider.kind == ProviderKind::Typesafe && model.capabilities.contains(&Capability::Rerank)
}

fn remap_of(model: &ModelConfig) -> RemapConfig {
    let converter = model.rerank.clone().unwrap_or_default();
    RemapConfig {
        instructions: converter.instructions,
        criteria: converter.criteria.map(|c| CriteriaConfig {
            yes: c.yes,
            no: c.no,
        }),
        ..RemapConfig::default()
    }
}

fn plain_target(model: String) -> TargetConfig {
    TargetConfig {
        model,
        weight: None,
        when: None,
        overrides: None,
        remap: None,
    }
}

/// Reserve `id` in the shared namespace, or fail naming it.
fn claim(id: String, ids: &mut HashSet<String>) -> Result<String, MigrateError> {
    if ids.insert(id.clone()) {
        Ok(id)
    } else {
        Err(MigrateError::Conflict(format!(
            "the id '{id}' the migration needs already exists"
        )))
    }
}

/// Whether `removed` carries a price that `target` does not: dropping the
/// removed reranker's table would silently change what is billed.
fn prices_lost(removed: &ModelConfig, target: &ModelConfig) -> bool {
    let differs = |a: Option<f64>, b: Option<f64>| a.is_some() && a != b;
    differs(removed.cost_per_1m_input, target.cost_per_1m_input)
        || differs(removed.cost_per_1m_output, target.cost_per_1m_output)
        || differs(removed.cost_per_1k_searches, target.cost_per_1k_searches)
}

fn price_note(removed: &ModelConfig, target_id: &str) -> String {
    format!(
        "the prices of the removed reranker '{}' differ from those of '{target_id}', which now serves it: \
         review cost_per_1m_input, cost_per_1m_output and cost_per_1k_searches on '{target_id}'",
        removed.id
    )
}

/// Pass 1: which foundation models are renamed, removed or created, and the
/// remap target of each legacy Jev reranker. Every rename is computed before
/// any target is resolved, so a target never points at an id that a later
/// rename turns into a virtual model.
fn plan_foundations(
    cfg: &Config,
    ids: &mut HashSet<String>,
    plan: &mut Plan,
) -> Result<HashMap<String, TargetConfig>, MigrateError> {
    for p in &cfg.providers {
        for m in &p.models {
            let legacy = is_legacy_rerank(p, m);
            let keeps_foundation = (!m.fallbacks.is_empty() && !legacy)
                || (legacy && m.capabilities.contains(&Capability::SystemOne));
            if keeps_foundation {
                let new_id = claim(format!("{}/{}", p.name, m.id), ids)?;
                plan.renames.insert(m.id.clone(), new_id);
            }
        }
    }

    let mut remap_targets: HashMap<String, TargetConfig> = HashMap::new();
    // (provider index, upstream id) -> index into `plan.created`, so several
    // rerankers over one Jev model share one created foundation model.
    let mut created_for: HashMap<(usize, String), usize> = HashMap::new();
    for (pi, p) in cfg.providers.iter().enumerate() {
        for (mi, m) in p.models.iter().enumerate() {
            if !is_legacy_rerank(p, m) {
                continue;
            }
            let target_id = if let Some(renamed) = plan.renames.get(&m.id) {
                plan.recapabilities.insert(
                    (pi, mi),
                    m.capabilities
                        .iter()
                        .copied()
                        .filter(|c| *c != Capability::Rerank)
                        .collect(),
                );
                renamed.clone()
            } else {
                plan.removals.push((pi, mi));
                let upstream = m.resolved_upstream_id();
                let sibling = p.models.iter().find(|o| {
                    o.id != m.id
                        && o.capabilities.contains(&Capability::SystemOne)
                        && o.resolved_upstream_id() == upstream
                });
                if let Some(sibling) = sibling {
                    let id = plan
                        .renames
                        .get(&sibling.id)
                        .cloned()
                        .unwrap_or_else(|| sibling.id.clone());
                    if prices_lost(m, sibling) {
                        plan.notes.push(price_note(m, &id));
                    }
                    id
                } else if let Some(&index) = created_for.get(&(pi, upstream.to_owned())) {
                    let created = &plan.created[index].model;
                    let id = created.id.clone();
                    let note = prices_lost(m, created).then(|| price_note(m, &id));
                    plan.notes.extend(note);
                    id
                } else {
                    let id = claim(format!("{}/{upstream}", p.name), ids)?;
                    created_for.insert((pi, upstream.to_owned()), plan.created.len());
                    plan.created.push(Created {
                        provider: pi,
                        model: ModelConfig {
                            upstream_id: Some(upstream.to_owned()),
                            capabilities: vec![Capability::SystemOne],
                            cost_per_1m_input: m.cost_per_1m_input,
                            cost_per_1m_output: m.cost_per_1m_output,
                            cost_per_1k_searches: m.cost_per_1k_searches,
                            ..ModelConfig::minimal(&id)
                        },
                    });
                    id
                }
            };
            remap_targets.insert(
                m.id.clone(),
                TargetConfig {
                    remap: Some(remap_of(m)),
                    ..plain_target(target_id)
                },
            );
        }
    }
    Ok(remap_targets)
}

fn plan(cfg: &Config) -> Result<Plan, MigrateError> {
    let mut plan = Plan::default();
    let mut ids: HashSet<String> = cfg
        .providers
        .iter()
        .flat_map(|p| p.models.iter().map(|m| m.id.clone()))
        .chain(cfg.virtual_models.iter().map(|v| v.id.clone()))
        .collect();
    let remap_targets = plan_foundations(cfg, &mut ids, &mut plan)?;

    // Pass 2: one virtual model per legacy model. A rerank virtual model's
    // target naming a removed Jev reranker carries its remap inline (a remap
    // is only valid on rerank); any other reference points at the (possibly
    // renamed) foundation id. Only foundation ids are referenced.
    let fallback_target = |id: &str, capability: Capability| -> TargetConfig {
        let remapped = (capability == Capability::Rerank)
            .then(|| remap_targets.get(id).cloned())
            .flatten();
        remapped.unwrap_or_else(|| {
            plain_target(
                plan.renames
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| id.to_owned()),
            )
        })
    };
    let mut virtuals = Vec::new();
    let mut notes = Vec::new();
    for p in &cfg.providers {
        for m in &p.models {
            let legacy = is_legacy_rerank(p, m);
            if m.fallbacks.is_empty() && !legacy {
                continue;
            }
            let capability = if legacy {
                Capability::Rerank
            } else {
                // Old configs required at least one capability; Chat is never reached.
                m.capabilities.first().copied().unwrap_or(Capability::Chat)
            };
            let others: Vec<&str> = m
                .capabilities
                .iter()
                .filter(|c| **c != capability)
                .map(|c| c.as_str())
                .collect();
            if !others.is_empty() {
                let consequence = match plan.renames.get(&m.id) {
                    Some(new_id) => {
                        format!("clients that call it for those must now use '{new_id}'")
                    }
                    None => "those capabilities are dropped with the removed model".to_owned(),
                };
                notes.push(format!(
                    "model '{}' also serves {}: the virtual model '{}' only serves {}; {consequence}",
                    m.id,
                    others.join(", "),
                    m.id,
                    capability.as_str(),
                ));
            }
            let mut targets = vec![fallback_target(&m.id, capability)];
            targets.extend(m.fallbacks.iter().map(|f| fallback_target(f, capability)));
            virtuals.push(VirtualModelConfig {
                id: m.id.clone(),
                capability,
                strategy: if targets.len() == 1 {
                    StrategyKind::Single
                } else {
                    StrategyKind::Fallback
                },
                targets,
                fallback_on: None,
                preset: None,
                description: None,
                listed: true,
            });
        }
    }
    plan.virtuals = virtuals;
    plan.notes.extend(notes);
    Ok(plan)
}

/// Rewrite `doc` (see the module docs).
///
/// # Errors
/// [`MigrateError`] when the document does not parse, a needed id exists,
/// or an edit fails.
pub fn migrate_document(doc: &str) -> Result<Migration, MigrateError> {
    let cfg: Config = toml::from_str(doc).map_err(|e| MigrateError::Parse(e.to_string()))?;
    let plan = plan(&cfg)?;
    if plan.virtuals.is_empty() {
        return Ok(Migration {
            text: doc.to_owned(),
            changed: false,
            notes: Vec::new(),
        });
    }
    let mut document = doc
        .parse::<DocumentMut>()
        .map_err(|e| MigrateError::Parse(e.to_string()))?;
    let providers = document
        .get_mut("providers")
        .ok_or_else(|| MigrateError::Parse("no providers".to_owned()))?;
    normalize_to_array_of_tables(providers);
    let providers = providers
        .as_array_of_tables_mut()
        .ok_or(MigrateError::Edit(EditError::ProvidersNotArray))?;

    for (pi, provider) in providers.iter_mut().enumerate() {
        let Some(models) = provider.get_mut("models") else {
            continue;
        };
        normalize_to_array_of_tables(models);
        let Some(models) = models.as_array_of_tables_mut() else {
            continue;
        };
        for (mi, table) in models.iter_mut().enumerate() {
            let Some(id) = table.get("id").and_then(Item::as_str).map(str::to_owned) else {
                continue;
            };
            if let Some(new_id) = plan.renames.get(&id) {
                // Without an explicit upstream id the old id was sent
                // upstream; pin it so the rename changes nothing upstream.
                if !table.contains_key("upstream_id") {
                    table["upstream_id"] = toml_edit::value(id.as_str());
                }
                table["id"] = toml_edit::value(new_id.as_str());
                table.remove("fallbacks");
            }
            if let Some(caps) = plan.recapabilities.get(&(pi, mi)) {
                let mut array = toml_edit::Array::new();
                for c in caps {
                    array.push(c.as_str());
                }
                table["capabilities"] = toml_edit::value(array);
                table.remove("rerank");
            }
        }
        let mut removed: Vec<usize> = plan
            .removals
            .iter()
            .filter(|(p, _)| *p == pi)
            .map(|(_, m)| *m)
            .collect();
        removed.sort_unstable_by(|a, b| b.cmp(a));
        for mi in removed {
            models.remove(mi);
        }
        for created in plan.created.iter().filter(|c| c.provider == pi) {
            let table = toml_edit::ser::to_document(&created.model)
                .map_err(EditError::from)?
                .as_table()
                .clone();
            models.push(table);
        }
    }

    let mut text = document.to_string();
    for vm in &plan.virtuals {
        text = upsert_virtual_model(&text, vm)?;
    }
    Ok(Migration {
        text,
        changed: true,
        notes: plan.notes,
    })
}

/// Stands in for every remap string in a [`hint_for`] snippet.
const REMAP_PLACEHOLDER: &str = "<copy from your [providers.models.rerank] block>";

/// The replacement instructions for one legacy model, for the boot error
/// (Task 15). Following them yields a config that validates. `None` when
/// `model_id` needs no migration, or has no virtual-model equivalent.
///
/// The hint ends up in the boot error, the reload log line and the admin
/// `LM-1001` body, so every remap string (instructions, criteria, context,
/// levels) is replaced by a placeholder: remap text is operator prompt text
/// and is never logged. `lumen config migrate` carries the real text.
#[must_use]
pub fn hint_for(cfg: &Config, model_id: &str) -> Option<String> {
    #[derive(serde::Serialize)]
    struct Snippet<'a> {
        virtual_models: [&'a VirtualModelConfig; 1],
    }
    let plan = plan(cfg).ok()?;
    let mut vm = plan.virtuals.iter().find(|v| v.id == model_id)?.clone();
    for remap in vm.targets.iter_mut().filter_map(|t| t.remap.as_mut()) {
        redact_remap(remap);
    }
    let vm = &vm;
    let body = toml::to_string(&Snippet {
        virtual_models: [vm],
    })
    .ok()?;
    let owner = cfg
        .providers
        .iter()
        .flat_map(|p| p.models.iter().map(move |m| (p, m)))
        .find(|(_, m)| m.id == model_id);
    let legacy_rerank = owner.is_some_and(|(p, m)| is_legacy_rerank(p, m));
    // A rename keeps the upstream id the model sent before (its old id when
    // it had no explicit `upstream_id`).
    let pin = if owner.is_some_and(|(_, m)| m.upstream_id.is_none()) {
        format!(", set `upstream_id = \"{model_id}\"`")
    } else {
        String::new()
    };
    let mut steps = Vec::new();
    match plan.renames.get(model_id) {
        Some(new_id) if legacy_rerank => steps.push(format!(
            "rename the foundation model to '{new_id}'{pin}, remove `rerank` from its \
             `capabilities`, delete its `[providers.models.rerank]` block and remove its \
             `fallbacks`"
        )),
        Some(new_id) => steps.push(format!(
            "rename the foundation model to '{new_id}'{pin} and remove its `fallbacks`"
        )),
        None => steps.push("remove that foundation model entry".to_owned()),
    }
    for created in &plan.created {
        if vm.targets.iter().any(|t| t.model == created.model.id) {
            let provider = &cfg.providers[created.provider].name;
            let model = toml::to_string(&created.model).ok()?;
            steps.push(format!(
                "unless it already exists, add this foundation model to provider '{provider}':\n\
                 [[providers.models]]\n{model}"
            ));
        }
    }
    steps.push(format!("add:\n{body}"));
    Some(steps.join("\nthen "))
}

/// Replace every string of `remap` with [`REMAP_PLACEHOLDER`], keeping its
/// shape (which fields are set, how many levels and questions).
fn redact_remap(remap: &mut RemapConfig) {
    fn redact(s: &mut String) {
        REMAP_PLACEHOLDER.clone_into(s);
    }
    fn redact_criteria(c: &mut CriteriaConfig) {
        c.yes.iter_mut().chain(c.no.iter_mut()).for_each(redact);
    }
    remap
        .context
        .iter_mut()
        .chain(remap.instructions.iter_mut())
        .chain(remap.levels.iter_mut().flatten())
        .for_each(redact);
    remap.criteria.iter_mut().for_each(redact_criteria);
    for q in remap.questions.iter_mut().flatten() {
        redact(&mut q.instructions);
        q.criteria.iter_mut().for_each(redact_criteria);
    }
}
