//! The account-scoped half of `/admin/config/virtual_models` (ADR 022): the
//! same routes and shapes as the platform's, answered from the calling
//! account's own document when the call carries `X-Lumen-Account-Ref`.
//!
//! A save is strict where the platform's is: the saved model must be clean
//! (no unknown target, no broken rule, no platform id) and the save may not
//! leave any other model of the account worse off. Problems the platform
//! caused (a target it deleted) never block a save of another model; they are
//! reported as `warnings` on the list and the plan.
//!
//! Every write is a compare-and-swap on the account's own hash, then installs
//! the new table in this process before answering, so the next request sees
//! it. Other processes load it through the refresher (ADR 022 section 5).

use axum::http::StatusCode;
use axum::Json;
use lumen_auth::store::AccountModelsCas;
use lumen_core::GatewayError;
use lumen_router::virtual_models::{Problem, ProblemKind, RoutingTable, VirtualModelConfig};

use crate::account_models::{
    document_hash, document_text, empty_hash, parse_row, MAX_ACCOUNT_DOCUMENT_BYTES,
    MAX_ACCOUNT_VIRTUAL_MODELS,
};
use crate::admin::{
    internal, runtime, VirtualModelDocument, VirtualModelPlan, VirtualModelSummary,
    VirtualModelsList,
};
use crate::error::ApiError;
use crate::state::AppState;

/// The account's stored configs and hash (`[]` and the empty hash when it
/// never wrote any). Reads the store: admin path only.
async fn load(
    state: &AppState,
    account: &str,
) -> Result<(Vec<VirtualModelConfig>, String), ApiError> {
    let row = runtime(state)?
        .store
        .load_account_virtual_models(account)
        .await
        .map_err(|e| internal(&e))?;
    let Some(row) = row else {
        return Ok((Vec::new(), empty_hash()));
    };
    let hash = row.hash.clone();
    let (_, configs, _, _) = parse_row(row).ok_or_else(|| {
        ApiError::from(GatewayError::Internal(
            "the stored account virtual models do not parse".to_owned(),
        ))
    })?;
    Ok((configs, hash))
}

/// The `LM-1004` a stale account `If-Match` gets.
fn stale() -> ApiError {
    GatewayError::ConfigStale(
        "this account's virtual models changed since they were read; \
         GET /admin/config/virtual_models and re-apply"
            .to_owned(),
    )
    .into()
}

/// The `LM-1001` for a problem of model `id`.
fn invalid(id: &str, message: &str) -> ApiError {
    GatewayError::InvalidRequest(format!("virtual model '{id}': {message}")).into()
}

/// The 404 `LM-1003` an id the account does not hold gets.
fn unknown(id: &str) -> ApiError {
    GatewayError::NotFound(format!("unknown virtual model '{id}'")).into()
}

/// The messages of every problem of `id` in `table`.
fn warnings(table: &RoutingTable, id: &str) -> Vec<String> {
    table
        .problems(id)
        .iter()
        .map(|p| p.message.clone())
        .collect()
}

/// Whether a problem refuses a save.
fn blocking(problem: &Problem) -> bool {
    problem.kind != ProblemKind::Note
}

/// `GET /admin/config/virtual_models` for `account`.
///
/// # Errors
/// `LM-5001` on a store failure.
pub async fn list(state: &AppState, account: &str) -> Result<Json<VirtualModelsList>, ApiError> {
    let (configs, hash) = load(state, account).await?;
    let table = state.resilience.compile_account(&configs);
    Ok(Json(VirtualModelsList {
        virtual_models: configs
            .iter()
            .map(|v| VirtualModelSummary {
                id: v.id.clone(),
                capability: v.capability.as_str(),
                strategy: v.strategy.as_str(),
                warnings: warnings(&table, &v.id),
            })
            .collect(),
        hash,
    }))
}

/// `GET /admin/config/virtual_models/{id}` for `account`.
///
/// # Errors
/// `LM-1003` when the account holds no such id.
pub async fn get(
    state: &AppState,
    account: &str,
    id: &str,
) -> Result<Json<VirtualModelDocument>, ApiError> {
    let (configs, hash) = load(state, account).await?;
    let virtual_model = configs
        .into_iter()
        .find(|v| v.id == id)
        .ok_or_else(|| unknown(id))?;
    Ok(Json(VirtualModelDocument {
        virtual_model,
        hash,
    }))
}

/// `GET /admin/config/virtual_models/{id}/plan` for `account`: the tree
/// compiled from the stored document over the live platform, platform
/// virtual targets shown as references. A disabled model's plan is its own
/// config, with the reason in `warnings`.
///
/// # Errors
/// `LM-1003` when the account holds no such id.
pub async fn plan(
    state: &AppState,
    account: &str,
    id: &str,
) -> Result<Json<VirtualModelPlan>, ApiError> {
    let (configs, hash) = load(state, account).await?;
    let config = configs
        .iter()
        .find(|v| v.id == id)
        .ok_or_else(|| unknown(id))?;
    let table = state.resilience.compile_account(&configs);
    let plan = match table.plan(id) {
        Some(plan) => plan,
        None => serde_json::to_value(config)
            .map_err(|e| ApiError::from(GatewayError::Internal(e.to_string())))?,
    };
    Ok(Json(VirtualModelPlan {
        plan,
        hash,
        warnings: warnings(&table, id),
    }))
}

/// `PUT /admin/config/virtual_models/{id}` for `account`: insert or replace
/// `vm` (already checked against the path id and for JSON nulls).
///
/// # Errors
/// `LM-1001` for an invalid model, a platform id, a limit or a save that
/// breaks another model of the account; `LM-1004` for a stale `If-Match`.
pub async fn put(
    state: &AppState,
    account: &str,
    if_match: &str,
    vm: VirtualModelConfig,
) -> Result<StatusCode, ApiError> {
    if state.resilience.is_platform_id(&vm.id) {
        return Err(invalid(
            &vm.id,
            "collides with a platform model of the same id; pick another id",
        ));
    }
    let (current, hash) = load(state, account).await?;
    if hash != if_match {
        return Err(stale());
    }
    let id = vm.id.clone();
    let mut next = current.clone();
    match next.iter_mut().find(|v| v.id == id) {
        Some(slot) => *slot = vm,
        None => next.push(vm),
    }
    if next.len() > MAX_ACCOUNT_VIRTUAL_MODELS {
        return Err(invalid(
            &id,
            &format!(
                "an account holds at most {MAX_ACCOUNT_VIRTUAL_MODELS} virtual models; delete one first"
            ),
        ));
    }
    save(state, account, if_match, &current, next, Some(&id)).await
}

/// `DELETE /admin/config/virtual_models/{id}` for `account`.
///
/// # Errors
/// `LM-1003` when absent; `LM-1001` when another model of the account still
/// targets it; `LM-1004` for a stale `If-Match`.
pub async fn delete(
    state: &AppState,
    account: &str,
    if_match: &str,
    id: &str,
) -> Result<StatusCode, ApiError> {
    let (current, hash) = load(state, account).await?;
    if !current.iter().any(|v| v.id == id) {
        return Err(unknown(id));
    }
    if hash != if_match {
        return Err(stale());
    }
    let next: Vec<VirtualModelConfig> = current.iter().filter(|v| v.id != id).cloned().collect();
    if let Some(dependent) = next
        .iter()
        .find(|v| v.targets.iter().any(|t| t.model == id))
    {
        return Err(invalid(
            &dependent.id,
            &format!("still targets '{id}'; change it before deleting '{id}'"),
        ));
    }
    save(state, account, if_match, &current, next, None).await
}

/// Check `next` against `current`, store it with a compare-and-swap on
/// `if_match`, then install it in this process. `written` is the id a `PUT`
/// saved: it must be free of blocking problems; every other model may only
/// keep the problems it already had.
async fn save(
    state: &AppState,
    account: &str,
    if_match: &str,
    current: &[VirtualModelConfig],
    next: Vec<VirtualModelConfig>,
    written: Option<&str>,
) -> Result<StatusCode, ApiError> {
    let text = document_text(&next).map_err(|e| {
        ApiError::from(GatewayError::InvalidRequest(format!(
            "the virtual model cannot be stored: {e}"
        )))
    })?;
    if text.len() > MAX_ACCOUNT_DOCUMENT_BYTES {
        return Err(GatewayError::InvalidRequest(format!(
            "this account's virtual models would take {} bytes; the limit is \
             {MAX_ACCOUNT_DOCUMENT_BYTES}",
            text.len()
        ))
        .into());
    }
    let before = state.resilience.compile_account(current);
    let after = state.resilience.compile_account(&next);
    if let Some(id) = written {
        if let Some(problem) = after.problems(id).iter().find(|p| blocking(p)) {
            return Err(invalid(id, &problem.message));
        }
    }
    for config in &next {
        let had = before.problems(&config.id);
        if let Some(problem) = after
            .problems(&config.id)
            .iter()
            .find(|p| blocking(p) && !had.contains(p))
        {
            return Err(invalid(&config.id, &problem.message));
        }
    }

    let hash = document_hash(&text);
    let outcome = runtime(state)?
        .store
        .save_account_virtual_models(account, &text, &hash, if_match, &empty_hash())
        .await
        .map_err(|e| internal(&e))?;
    let generation = match outcome {
        AccountModelsCas::Applied { generation } => generation,
        AccountModelsCas::Stale { .. } => return Err(stale()),
    };
    state
        .resilience
        .install_accounts(vec![(account.to_owned(), next, hash, generation)], false);
    Ok(StatusCode::NO_CONTENT)
}
