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

/// Pass 1: which foundation models are renamed, removed or created, and the
/// remap target of each legacy Jev reranker.
fn plan_foundations(
    cfg: &Config,
    ids: &mut HashSet<String>,
    plan: &mut Plan,
) -> Result<HashMap<String, TargetConfig>, MigrateError> {
    let mut remap_targets: HashMap<String, TargetConfig> = HashMap::new();
    for (pi, p) in cfg.providers.iter().enumerate() {
        for (mi, m) in p.models.iter().enumerate() {
            let legacy = is_legacy_rerank(p, m);
            let keeps_foundation = (!m.fallbacks.is_empty() && !legacy)
                || (legacy && m.capabilities.contains(&Capability::SystemOne));
            if keeps_foundation {
                let new_id = claim(format!("{}/{}", p.name, m.id), ids)?;
                plan.renames.insert(m.id.clone(), new_id);
            }
            if !legacy {
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
                    sibling.id.clone()
                } else {
                    let id = claim(format!("{}/{upstream}", p.name), ids)?;
                    plan.created.push(Created {
                        provider: pi,
                        model: ModelConfig {
                            upstream_id: Some(upstream.to_owned()),
                            capabilities: vec![Capability::SystemOne],
                            cost_per_1m_input: m.cost_per_1m_input,
                            cost_per_1m_output: m.cost_per_1m_output,
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

    // Pass 2: one virtual model per legacy model. A target naming a removed
    // Jev reranker carries its remap inline; one naming a renamed model
    // points at the new foundation id. Only foundation ids are referenced.
    let fallback_target = |id: &str| -> TargetConfig {
        remap_targets.get(id).cloned().unwrap_or_else(|| {
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
                .filter(|c| **c != capability && !(legacy && **c == Capability::SystemOne))
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
            let mut targets = vec![fallback_target(&m.id)];
            targets.extend(m.fallbacks.iter().map(|f| fallback_target(f)));
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
    plan.notes = notes;
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

/// The replacement snippet for one legacy model, for the boot error
/// (Task 15). `None` when `model_id` needs no migration.
#[must_use]
pub fn hint_for(cfg: &Config, model_id: &str) -> Option<String> {
    #[derive(serde::Serialize)]
    struct Snippet<'a> {
        virtual_models: [&'a VirtualModelConfig; 1],
    }
    let plan = plan(cfg).ok()?;
    let vm = plan.virtuals.iter().find(|v| v.id == model_id)?;
    let body = toml::to_string(&Snippet {
        virtual_models: [vm],
    })
    .ok()?;
    Some(match plan.renames.get(model_id) {
        Some(new_id) => format!("rename the foundation model to '{new_id}' and add:\n{body}"),
        None => format!("remove that foundation model entry and add:\n{body}"),
    })
}
