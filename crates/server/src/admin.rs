//! Minimal admin API (M5 §5.5): key management under `/admin`, protected by
//! the master key (see `auth::require_master_key`).
//!
//! * `POST /admin/keys` - create a key. The response is the ONLY place the
//!   plaintext key ever appears; it is never stored and never logged.
//! * `GET /admin/keys` - list keys (records only, no hashes, no plaintext).
//!   `?include_deleted=true` also shows soft-deleted tombstones.
//! * `PATCH /admin/keys/{id}` - adjust budgets/limits, enable/disable.
//! * `DELETE /admin/keys/{id}` - soft-delete: the row becomes a tombstone
//!   (usage-log attribution and audit history survive) and the key stops
//!   authenticating immediately.
//! * `POST /admin/keys/{id}/rotate` - mint a new secret for an existing key;
//!   same one-time-plaintext contract as creation, identity and budget state
//!   preserved.
//! * `POST /admin/groups` / `GET /admin/groups` - create and list budget
//!   groups (ADR 009): shared pools that member keys draw from in addition
//!   to their own budgets.
//! * `PATCH /admin/groups/{id}` - adjust a group's shared budget; binds
//!   every member on their next request.
//! * `DELETE /admin/groups/{id}` - soft-delete a group; refused while it
//!   still has active member keys.
//! * `POST /admin/keys/{id}/grant` / `POST /admin/groups/{id}/grant` -
//!   atomically raise a budget cap by `amount` USD (ADR 009 amendment): an
//!   in-database and in-memory `fetch_add`, so concurrent top-ups from a
//!   billing control plane never lose an update the way read-modify-write
//!   PATCHes can.
//! * `PUT /admin/provider-keys/{name}` - store a provider API key encrypted
//!   at rest (AES-256-GCM under the master key) and apply it without a restart
//!   by requesting a hot reload; used for providers whose `api_key_env` is
//!   unset or empty (env keeps precedence when set).
//! * `GET /admin/usage` - aggregated usage and spend reporting over the
//!   `usage_log` table (issue #64).
//! * `GET /admin/usage/export` - cursor-paginated raw `usage_log` rows, for a
//!   control plane building its own multi-dimensional view (ADR 010).
//! * `GET /admin/config` - the config file verbatim, plus a BLAKE3 content
//!   hash to be echoed as `If-Match` on the `PUT` that applies a new one
//!   (ADR 010). Never a serialisation of the merged in-memory `Config`: that
//!   would render environment overrides as if they were file content.
//! * `PUT /admin/config` - apply a new config document (ADR 010). The
//!   submitted bytes are staged in a temp file next to the real one,
//!   validated there (parse + registry build), then the current file is
//!   backed up to `.bak` and the staged file is renamed into place. Requires
//!   an `If-Match` header carrying the current hash from `GET /admin/config`;
//!   a stale hash is a 412 (`LM-1004`), a missing header or an invalid
//!   document is a 400 (`LM-1001`). This is the highest-privilege route in
//!   the gateway: it can repoint a provider's `base_url` and thereby redirect
//!   customer traffic.
//!
//! Every change is applied to the database AND the in-memory state, so it
//! takes effect immediately without a restart.

use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use lumen_auth::events::EventKind;
use lumen_auth::key::hash_key;
use lumen_auth::state::micro_to_usd;
use lumen_auth::store::{
    DeleteGroupOutcome, GroupPatch, GroupRecord, KeyPatch, NewGroup, NewKey, UsageAggregate,
    UsageFilter, UsageGroupBy, VirtualKeyRecord,
};
use lumen_core::GatewayError;
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::sync::Arc;

/// `POST /admin/keys` response: the record plus the one-time plaintext key.
#[derive(Serialize)]
pub struct CreatedKey {
    /// The clear virtual key. Shown exactly once - store it now.
    pub key: String,
    /// The created record.
    #[serde(flatten)]
    pub record: VirtualKeyRecord,
}

// STRICT rule 5: the plaintext must be unrepresentable through `Debug`, so a
// stray `{:?}` in a log line or error chain can never leak it. Serialization
// (the one intended exposure) goes through `Serialize` only. Mirrors
// `PlaintextKey`'s own redacted `Debug`.
impl std::fmt::Debug for CreatedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatedKey")
            .field("key", &"REDACTED")
            .field("record", &self.record)
            .finish()
    }
}

/// Map an auth-layer failure to an opaque 500 - never a misleading 401.
fn internal(error: &lumen_auth::AuthError) -> ApiError {
    GatewayError::Internal(error.to_string()).into()
}

/// Map a store failure from a key/group write: a caller-named unknown group
/// or capless grant target is their error (400 `LM-1001` - naming the id
/// back leaks nothing, they sent it), everything else stays an opaque 500.
fn store_error(error: lumen_auth::AuthError) -> ApiError {
    match error {
        lumen_auth::AuthError::UnknownGroup(id) => {
            GatewayError::InvalidRequest(format!("unknown budget group '{id}'")).into()
        }
        lumen_auth::AuthError::NoBudgetCap(id) => GatewayError::InvalidRequest(format!(
            "'{id}' has no budget cap to grant to (budget_max is unlimited); \
             set one with a PATCH first"
        ))
        .into(),
        other => internal(&other),
    }
}

fn runtime(state: &AppState) -> Result<&crate::auth::AuthRuntime, ApiError> {
    state
        .auth
        .as_deref()
        .ok_or_else(|| GatewayError::Unauthorized.into())
}

/// Create a virtual key.
pub async fn create_key(
    State(state): State<AppState>,
    payload: Result<Json<NewKey>, JsonRejection>,
) -> Result<(StatusCode, Json<CreatedKey>), ApiError> {
    let Json(params) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    if params.name.trim().is_empty() {
        return Err(GatewayError::InvalidRequest("`name` must not be empty".to_owned()).into());
    }
    let auth = runtime(&state)?;
    let (plaintext, record) = auth.store.create_key(params).await.map_err(store_error)?;
    // Make the key usable immediately, without waiting for a reboot.
    auth.keys.upsert(hash_key(plaintext.reveal()), &record);
    Ok((
        StatusCode::CREATED,
        Json(CreatedKey {
            key: plaintext.reveal().to_owned(),
            record,
        }),
    ))
}

/// `GET /admin/keys` query parameters.
#[derive(Debug, Default, Deserialize)]
pub struct ListKeysParams {
    /// Also list soft-deleted tombstones (default: active keys only).
    #[serde(default)]
    pub include_deleted: bool,
}

/// List every active key (no secrets: ids, names, budgets, limits, flags).
/// `?include_deleted=true` adds soft-deleted tombstones for auditing. A
/// malformed query string is a `LM-1001` JSON envelope, like every other
/// extractor failure in this module - never axum's bare-text rejection.
pub async fn list_keys(
    State(state): State<AppState>,
    params: Result<Query<ListKeysParams>, QueryRejection>,
) -> Result<Json<Vec<VirtualKeyRecord>>, ApiError> {
    let Query(params) = params.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    let auth = runtime(&state)?;
    let keys = auth
        .store
        .list_keys(params.include_deleted)
        .await
        .map_err(|e| internal(&e))?;
    Ok(Json(keys))
}

/// Patch a key: adjust budgets/limits, enable/disable. An unknown id is a
/// 400 `LM-1001` naming the id - the public taxonomy reserves 404 for
/// unknown *models* (`LM-2001`) and has no admin-resource code.
pub async fn patch_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    payload: Result<Json<KeyPatch>, JsonRejection>,
) -> Result<Json<VirtualKeyRecord>, ApiError> {
    let Json(patch) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    let auth = runtime(&state)?;
    // Snapshot the live disabled flag BEFORE the patch so `key.disabled` can
    // be edge-triggered: a PATCH that leaves an already-disabled key disabled
    // is not a state change and must not re-notify the billing backend
    // (ADR 011 §1).
    let was_disabled = auth.keys.key_disabled(&id);
    let updated = auth
        .store
        .update_key(&id, patch)
        .await
        .map_err(store_error)?
        .ok_or_else(|| GatewayError::InvalidRequest(format!("unknown key id '{id}'")))?;
    // Reflect the change in the live table (spend is preserved).
    auth.keys.apply(&updated);
    if updated.disabled && was_disabled == Some(false) {
        auth.keys.signal_key_lifecycle(&id, EventKind::KeyDisabled);
    }
    Ok(Json(updated))
}

/// Delete a key - a **soft delete** by design: `usage_log` rows reference
/// the key id, so removing the row would orphan usage history, and the
/// tombstone keeps the audit trail (see `docs/operations/keys-budgets.md`).
/// The key stops authenticating on the very next request (the live table is
/// updated like `patch_key`), disappears from the default list, and any
/// further PATCH/DELETE/rotate on the id behaves like an unknown id
/// (400 `LM-1001`) - it can never be resurrected by accident.
pub async fn delete_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let auth = runtime(&state)?;
    let deleted = auth.store.delete_key(&id).await.map_err(|e| internal(&e))?;
    // Evict from the live table UNCONDITIONALLY - whether this call's DB
    // write actually matched a row (`Some`) or the row was already
    // tombstoned by an earlier attempt (`None`, e.g. a client retry after a
    // disconnect that landed the DB write but never reached this line the
    // first time). Without this, a cancelled request could tombstone the DB
    // row yet leave the key authenticating from memory forever - only a
    // restart would notice. Making the eviction unconditional means every
    // retry repairs a previously missed one, so the zombie window closes on
    // the very next delete attempt rather than lasting until a restart.
    if let Some(entry) = auth.keys.remove(&id) {
        // Announce the removal only when THIS call's DB write is the one that
        // tombstoned the row: a retry that only repaired a missed eviction
        // (see above) must not fire a second `key.deleted` (ADR 011 §1).
        if deleted.is_some() {
            entry.signal_lifecycle(EventKind::KeyDeleted);
        }
        // Flush the final accrued spend now: once the entry is dropped here
        // the periodic flusher (`drain_dirty`) will never see this id again,
        // so the tombstone's `budget_spent` would otherwise freeze at
        // whatever the last periodic flush happened to catch.
        let spent = micro_to_usd(entry.spent_micro());
        if let Err(error) = auth.store.persist_budgets(&[(id.clone(), spent)]).await {
            // Best-effort: the accounting is already backed by the periodic
            // flush for every other key, so a failure here must not turn a
            // successful delete into a 500 - log and continue.
            tracing::warn!(
                key_id = %id,
                %error,
                "failed to persist final spend while deleting a key"
            );
        }
    }
    deleted.ok_or_else(|| GatewayError::InvalidRequest(format!("unknown key id '{id}'")))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Rotate a key's secret: mint a new plaintext through the exact generation
/// path used at creation, store its hash, and return the new plaintext in
/// the same one-time response shape as `POST /admin/keys`. The record's id,
/// name, budgets, accrued spend and quotas are all preserved (the live entry
/// is kept, only its hash alias changes), so `usage_log` attribution is
/// unbroken. The old plaintext stops authenticating immediately; an unknown
/// or deleted id is a 400 `LM-1001`, like `patch_key`.
pub async fn rotate_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<CreatedKey>, ApiError> {
    let auth = runtime(&state)?;
    let (plaintext, record) = auth
        .store
        .rotate_key(&id)
        .await
        .map_err(|e| internal(&e))?
        .ok_or_else(|| GatewayError::InvalidRequest(format!("unknown key id '{id}'")))?;
    // Swap the live alias: the old plaintext dies and the new one works
    // right away, with spend and quota windows carried over.
    auth.keys.rotate(hash_key(plaintext.reveal()), &record);
    // The rotation itself IS the edge, so this fires every time - a backend
    // registry that caches key material needs every one of them. The payload
    // never carries the new (or old) plaintext.
    auth.keys
        .signal_key_lifecycle(&record.id, EventKind::KeyRotated);
    Ok(Json(CreatedKey {
        key: plaintext.reveal().to_owned(),
        record,
    }))
}

// ---- Budget groups (ADR 009) ------------------------------------------------

/// Create a budget group. No secret exists for a group, so the response is
/// just the record - nothing one-time about it.
pub async fn create_group(
    State(state): State<AppState>,
    payload: Result<Json<NewGroup>, JsonRejection>,
) -> Result<(StatusCode, Json<GroupRecord>), ApiError> {
    let Json(params) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    if params.name.trim().is_empty() {
        return Err(GatewayError::InvalidRequest("`name` must not be empty".to_owned()).into());
    }
    let auth = runtime(&state)?;
    let record = auth
        .store
        .create_group(params)
        .await
        .map_err(|e| internal(&e))?;
    // Make the group joinable and enforced immediately, no restart.
    auth.keys.upsert_group(&record);
    Ok((StatusCode::CREATED, Json(record)))
}

/// `GET /admin/groups` query parameters.
#[derive(Debug, Default, Deserialize)]
pub struct ListGroupsParams {
    /// Also list soft-deleted tombstones (default: active groups only).
    #[serde(default)]
    pub include_deleted: bool,
}

/// List every active budget group; `?include_deleted=true` adds tombstones.
pub async fn list_groups(
    State(state): State<AppState>,
    params: Result<Query<ListGroupsParams>, QueryRejection>,
) -> Result<Json<Vec<GroupRecord>>, ApiError> {
    let Query(params) = params.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    let auth = runtime(&state)?;
    let groups = auth
        .store
        .list_groups(params.include_deleted)
        .await
        .map_err(|e| internal(&e))?;
    Ok(Json(groups))
}

/// Patch a group: adjust the shared budget or the label. Pool spend is
/// preserved, and the new cap binds every member key on its very next
/// request. An unknown or deleted id is a 400 `LM-1001`, like `patch_key`.
pub async fn patch_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
    payload: Result<Json<GroupPatch>, JsonRejection>,
) -> Result<Json<GroupRecord>, ApiError> {
    let Json(patch) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    let auth = runtime(&state)?;
    let updated = auth
        .store
        .update_group(&id, patch)
        .await
        .map_err(|e| internal(&e))?
        .ok_or_else(|| GatewayError::InvalidRequest(format!("unknown group id '{id}'")))?;
    auth.keys.apply_group(&updated);
    Ok(Json(updated))
}

/// Delete a group - a **soft delete** like keys (the tombstone keeps
/// `usage_log.group_id` attribution), refused while the group still has
/// active member keys: silently dropping members out of pool enforcement
/// would be worse than this 400. Move or delete the member keys first.
pub async fn delete_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let auth = runtime(&state)?;
    let outcome = auth
        .store
        .delete_group(&id)
        .await
        .map_err(|e| internal(&e))?;
    match outcome {
        DeleteGroupOutcome::Deleted(_) => {
            // Evict from the live table and flush the final pool spend now:
            // once the entry is gone the periodic flusher never sees this id
            // again (mirrors `delete_key`).
            if let Some(entry) = auth.keys.remove_group(&id) {
                let spent = micro_to_usd(entry.spent_micro());
                if let Err(error) = auth
                    .store
                    .persist_group_budgets(&[(id.clone(), spent)])
                    .await
                {
                    tracing::warn!(
                        group_id = %id,
                        %error,
                        "failed to persist final pool spend while deleting a group"
                    );
                }
            }
            Ok(StatusCode::NO_CONTENT)
        }
        DeleteGroupOutcome::HasMembers(count) => Err(GatewayError::InvalidRequest(format!(
            "group '{id}' still has {count} active member key(s); move or delete them first"
        ))
        .into()),
        DeleteGroupOutcome::NotFound => {
            Err(GatewayError::InvalidRequest(format!("unknown group id '{id}'")).into())
        }
    }
}

/// `POST /admin/keys/{id}/grant` and `/admin/groups/{id}/grant` body.
#[derive(Debug, Deserialize)]
pub struct GrantBody {
    /// USD to add to the budget cap. Must be a positive finite number.
    pub amount: f64,
}

/// Hard upper bound on a single grant, in USD. Keeps the DB cap far away
/// from both f64 infinity (repeated huge grants would sum to `+Inf`, which
/// serializes as `null` and reloads as *unlimited*) and the in-memory
/// micro-USD clamp at ~9.2e12 - either would silently mint the unlimited
/// sentinel the grant path promises never to produce.
const MAX_GRANT_USD: f64 = 1e12;

/// Validate a grant amount: positive, finite and at most [`MAX_GRANT_USD`],
/// or 400 `LM-1001`. serde_json rejects overflowing literals like `1e999`
/// at parse time (and JSON has no NaN literal), so the finite check is
/// belt-and-braces in case a future serde version saturates to `inf`
/// instead of erroring.
fn validated_grant_amount(
    payload: Result<Json<GrantBody>, JsonRejection>,
) -> Result<f64, ApiError> {
    let Json(body) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    if body.amount.is_finite() && body.amount > 0.0 && body.amount <= MAX_GRANT_USD {
        Ok(body.amount)
    } else {
        Err(GatewayError::InvalidRequest(format!(
            "grant `amount` must be a positive finite number of at most {MAX_GRANT_USD:e} USD, \
             got '{}'",
            body.amount
        ))
        .into())
    }
}

/// Grant budget to a key: atomically raise `budget_max` by `amount` USD
/// (ADR 009 amendment). DB first (the durable atomic add), then the live
/// entry (its own `fetch_add`) - two concurrent grants both land on both
/// sides, which is the whole point over a read-modify-write PATCH. Takes
/// effect on the very next request, no restart. An unknown or deleted id is
/// a 400 `LM-1001`; so is a capless key (there is no cap to raise).
pub async fn grant_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    payload: Result<Json<GrantBody>, JsonRejection>,
) -> Result<Json<VirtualKeyRecord>, ApiError> {
    let amount = validated_grant_amount(payload)?;
    let auth = runtime(&state)?;
    let record = auth
        .store
        .grant_key_budget(&id, amount)
        .await
        .map_err(store_error)?
        .ok_or_else(|| GatewayError::InvalidRequest(format!("unknown key id '{id}'")))?;
    // The live entry increments independently (never re-read from the DB
    // record, which could interleave with a concurrent grant's re-read).
    // A dead id here means a racing delete: the tombstoned row never
    // reloads, and a deleted key needs no live cap - nothing to repair.
    //
    // Honest divergence windows (DB is authoritative; memory drift is
    // bounded by one grant amount and healed by the next reload/boot):
    // a hot reload or a PATCH stores caps ABSOLUTELY and can interleave
    // with the two-step grant in either direction, and a client disconnect
    // between the DB write and this line credits the DB but not memory.
    auth.keys
        .grant_key(&id, lumen_auth::state::usd_to_micro(amount));
    Ok(Json(record))
}

/// The group half of [`grant_key`]: atomically raise a pool's cap. Every
/// member key sees the new headroom on its next admission.
pub async fn grant_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
    payload: Result<Json<GrantBody>, JsonRejection>,
) -> Result<Json<GroupRecord>, ApiError> {
    let amount = validated_grant_amount(payload)?;
    let auth = runtime(&state)?;
    let record = auth
        .store
        .grant_group_budget(&id, amount)
        .await
        .map_err(store_error)?
        .ok_or_else(|| GatewayError::InvalidRequest(format!("unknown group id '{id}'")))?;
    auth.keys
        .grant_group(&id, lumen_auth::state::usd_to_micro(amount));
    Ok(Json(record))
}

/// `PUT /admin/provider-keys/{name}` body.
#[derive(Debug, Deserialize)]
pub struct ProviderKeyBody {
    /// The provider API key to seal. Never logged; encrypted at rest.
    pub key: String,
}

// The body deliberately has no Debug-derived secret exposure: ProviderKeyBody
// derives Debug for extractor plumbing but is never logged by the handler.

/// Store a provider key encrypted at rest and apply it without a restart: the
/// handler pings the hot-reload trigger, and the reloader re-reads the key from
/// the encrypted store and rebuilds the provider registry (M7). Providers whose
/// `api_key_env` resolves keep using the env value (env stays the primary
/// source); rotation via this route only affects env-keyless providers.
pub async fn put_provider_key(
    State(state): State<AppState>,
    Path(name): Path<String>,
    payload: Result<Json<ProviderKeyBody>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(body) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    if body.key.trim().is_empty() {
        return Err(GatewayError::InvalidRequest("`key` must not be empty".to_owned()).into());
    }
    let auth = runtime(&state)?;
    let Some(master) = auth.master.as_ref() else {
        return Err(GatewayError::Internal("master key unavailable".to_owned()).into());
    };
    auth.store
        .store_provider_key(&name, &body.key, master)
        .await
        .map_err(|e| internal(&e))?;
    // Apply the rotation without a restart: the reloader re-reads provider keys
    // from the DB (off the request path) and swaps the registry atomically. The
    // DB write above completed first, so the reload sees the new key.
    if let Some(trigger) = &state.reload_trigger {
        trigger.notify_one();
        tracing::info!(provider = %name, "provider key stored; hot reload requested to apply it");
    } else {
        tracing::info!(
            provider = %name,
            "provider key stored; no reloader armed, so it applies at next restart"
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- Usage reporting (issue #64) --------------------------------------------

/// Default window when `since` is absent: the last 24 hours.
const DEFAULT_WINDOW_SECS: i64 = 24 * 60 * 60;
/// Default number of groups returned.
const DEFAULT_GROUP_LIMIT: u32 = 100;
/// Hard cap on the number of groups a single call may return.
const MAX_GROUP_LIMIT: u32 = 1_000;

/// `GET /admin/usage` query parameters. Unknown parameters are rejected
/// (400 `LM-1001`), so a typo never silently widens a report.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageParams {
    /// Only rows for this virtual key id.
    pub key_id: Option<String>,
    /// Only rows attributed to this budget group id (ADR 009).
    pub group_id: Option<String>,
    /// Only rows for this client-facing model id.
    pub model: Option<String>,
    /// Only rows served by this provider instance.
    pub provider: Option<String>,
    /// Only rows of this capability: `chat` | `embed` | `rerank`.
    pub capability: Option<String>,
    /// Window start (inclusive): unix seconds or RFC3339. Default: 24 hours
    /// before `until`.
    pub since: Option<String>,
    /// Window end (inclusive): unix seconds or RFC3339. Default: now.
    pub until: Option<String>,
    /// Grouping dimension: `model` (default) | `model_used` | `provider` |
    /// `capability` | `key_id` | `group_id` | `status` | `total`.
    pub group_by: Option<String>,
    /// Maximum number of groups returned (1..=1000, default 100).
    pub limit: Option<u32>,
}

/// `GET /admin/usage` response: the effective window and grouping (defaults
/// resolved), plus one aggregate per group.
#[derive(Debug, Serialize)]
pub struct UsageReport {
    /// Effective window start, unix seconds (inclusive).
    pub since: i64,
    /// Effective window end, unix seconds (inclusive).
    pub until: i64,
    /// Effective grouping dimension.
    pub group_by: &'static str,
    /// `true` when more groups matched than `limit` allowed; the returned
    /// groups are the most expensive ones.
    pub truncated: bool,
    /// One aggregate per group, ordered by cost (descending, then name).
    pub groups: Vec<UsageAggregate>,
}

/// Report aggregated usage and spend from the `usage_log` table.
///
/// Master-key gated like every `/admin/*` route. The read runs directly
/// against SQLite - this is an admin route, off the hot path; API requests
/// only ever touch the bounded logging channel. Note the flush lag that
/// implies: usage rows are batched through that channel and flushed every
/// `usage_flush_ms` (or `usage_batch_max` rows), so requests from the last
/// couple of seconds may not be visible yet.
///
/// Invalid filters, timestamps, `group_by` values or limits are 400
/// `LM-1001`; a window that matches nothing is a 200 with empty `groups`.
pub async fn usage_report(
    State(state): State<AppState>,
    params: Result<Query<UsageParams>, QueryRejection>,
) -> Result<Json<UsageReport>, ApiError> {
    let Query(params) = params.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    let auth = runtime(&state)?;

    let group_by = match params.group_by.as_deref() {
        None => UsageGroupBy::Model,
        Some(value) => UsageGroupBy::parse(value).ok_or_else(|| {
            GatewayError::InvalidRequest(format!(
                "invalid `group_by` '{value}': expected one of model, model_used, \
                 provider, capability, key_id, group_id, status, total"
            ))
        })?,
    };
    if let Some(capability) = params.capability.as_deref() {
        if !matches!(capability, "chat" | "embed" | "rerank") {
            return Err(GatewayError::InvalidRequest(format!(
                "invalid `capability` '{capability}': expected chat, embed or rerank"
            ))
            .into());
        }
    }
    let until = match params.until.as_deref() {
        None => crate::auth::now_unix(),
        Some(value) => parse_time_param(value).ok_or_else(|| invalid_time("until", value))?,
    };
    let since = match params.since.as_deref() {
        None => until.saturating_sub(DEFAULT_WINDOW_SECS),
        Some(value) => parse_time_param(value).ok_or_else(|| invalid_time("since", value))?,
    };
    if since > until {
        return Err(GatewayError::InvalidRequest(format!(
            "`since` ({since}) must not be after `until` ({until})"
        ))
        .into());
    }
    let limit = params.limit.unwrap_or(DEFAULT_GROUP_LIMIT);
    if !(1..=MAX_GROUP_LIMIT).contains(&limit) {
        return Err(GatewayError::InvalidRequest(format!(
            "`limit` must be between 1 and {MAX_GROUP_LIMIT}, got {limit}"
        ))
        .into());
    }

    let filter = UsageFilter {
        key_id: params.key_id,
        group_id: params.group_id,
        model: params.model,
        provider: params.provider,
        capability: params.capability,
        since,
        until,
        // One extra row detects truncation without a second COUNT query.
        limit: i64::from(limit) + 1,
    };
    let mut groups = auth
        .store
        .usage_summary(&filter, group_by)
        .await
        .map_err(|e| internal(&e))?;
    // `limit` is at most 1000, so the conversion never actually saturates.
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let truncated = groups.len() > limit;
    groups.truncate(limit);

    Ok(Json(UsageReport {
        since,
        until,
        group_by: group_by.as_str(),
        truncated,
        groups,
    }))
}

/// Default page size for `GET /admin/usage/export`.
const DEFAULT_EXPORT_LIMIT: u32 = 1_000;
/// Hard cap on a single export page. A caller asking for more is rejected
/// rather than silently clamped: a silent clamp reads as "that was the whole
/// window" and would make a console under-report without any signal.
const MAX_EXPORT_LIMIT: u32 = 10_000;

/// `GET /admin/usage/export` query parameters. Unknown parameters are
/// rejected (400 `LM-1001`), so a typo never silently widens an export.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageExportParams {
    /// Window start (inclusive): unix seconds or RFC3339. Default: 24 hours
    /// before `until`.
    pub since: Option<String>,
    /// Window end (inclusive): unix seconds or RFC3339. Default: now.
    pub until: Option<String>,
    /// Return rows with an id strictly greater than this. Absent starts at
    /// the beginning of the window.
    pub cursor: Option<i64>,
    /// Page size. Default 1000, hard cap 10000.
    pub limit: Option<u32>,
}

/// `GET /admin/usage/export` response.
#[derive(Debug, Serialize)]
pub struct UsageExportPage {
    /// Effective window start, unix seconds (inclusive): either the caller's
    /// own `since`, or the resolved default. Echoed (like `usage_report`'s
    /// `since`/`until`) so a caller paginating without explicit bounds can
    /// pin the window to what the FIRST page actually resolved, by passing
    /// these two values back on every later page - otherwise `since`/`until`
    /// default to "24 hours before now" recomputed on every single call, so
    /// a multi-page export with no explicit window is filtering each page
    /// against a window that moved forward while it paginated.
    pub since: i64,
    /// Effective window end, unix seconds (inclusive). See `since`.
    pub until: i64,
    /// The rows of this page, ordered by `id`.
    pub rows: Vec<lumen_auth::store::UsageRow>,
    /// Cursor to pass as `cursor` for the next page, or `null` when the
    /// window is exhausted.
    pub next_cursor: Option<i64>,
}

/// Export raw usage rows for a window, paginated by primary key.
///
/// Complements `GET /admin/usage`, which aggregates over one dimension at a
/// time: a control plane building a multi-dimensional view needs the rows
/// themselves (ADR 010). The rows carry no prompt or response content.
pub async fn usage_export(
    State(state): State<AppState>,
    params: Result<Query<UsageExportParams>, QueryRejection>,
) -> Result<Json<UsageExportPage>, ApiError> {
    let Query(params) = params.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;

    let limit = params.limit.unwrap_or(DEFAULT_EXPORT_LIMIT);
    if limit == 0 || limit > MAX_EXPORT_LIMIT {
        return Err(GatewayError::InvalidRequest(format!(
            "`limit` must be between 1 and {MAX_EXPORT_LIMIT}"
        ))
        .into());
    }

    // Same helpers `usage_report` uses, so both routes accept identical
    // time formats: a digit string is unix seconds, anything else RFC3339.
    let until = match params.until.as_deref() {
        None => crate::auth::now_unix(),
        Some(value) => parse_time_param(value).ok_or_else(|| invalid_time("until", value))?,
    };
    let since = match params.since.as_deref() {
        None => until.saturating_sub(DEFAULT_WINDOW_SECS),
        Some(value) => parse_time_param(value).ok_or_else(|| invalid_time("since", value))?,
    };
    if since > until {
        return Err(
            GatewayError::InvalidRequest("`since` must not be after `until`".to_owned()).into(),
        );
    }

    let auth = runtime(&state)?;
    let rows = auth
        .store
        .usage_export(since, until, params.cursor, i64::from(limit))
        .await
        .map_err(|e| internal(&e))?;

    // A short page means the window is exhausted. A full page might be the
    // last one, in which case the caller gets one empty page: cheap, and far
    // safer than guessing and truncating an export.
    let next_cursor = if rows.len() == limit as usize {
        rows.last().map(|row| row.id)
    } else {
        None
    };

    Ok(Json(UsageExportPage {
        since,
        until,
        rows,
        next_cursor,
    }))
}

fn invalid_time(name: &str, value: &str) -> GatewayError {
    GatewayError::InvalidRequest(format!(
        "invalid `{name}` '{value}': expected unix seconds or an RFC3339 timestamp"
    ))
}

/// Parse a time parameter: a plain digit string is unix seconds; anything
/// else must be RFC3339 (`2026-07-16T08:30:00Z`, offsets allowed). `None`
/// on any malformation.
fn parse_time_param(value: &str) -> Option<i64> {
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return value.parse::<i64>().ok();
    }
    parse_rfc3339(value)
}

/// Minimal RFC3339 parser: `YYYY-MM-DDTHH:MM:SS[.frac](Z|+HH:MM|-HH:MM)`,
/// case-insensitive `T`/`Z` (a space instead of `T` is also accepted).
/// Fractional seconds are truncated; a leap second (`:60`) clamps to `:59`.
/// Returns unix seconds, or `None` on any malformation.
fn parse_rfc3339(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = value.get(range)?;
        if !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;
    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Skip fractional seconds.
    let mut idx = 19;
    if bytes.get(idx) == Some(&b'.') {
        idx += 1;
        let start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == start {
            return None;
        }
    }
    let offset_secs = match bytes.get(idx)? {
        b'Z' | b'z' if idx + 1 == bytes.len() => 0,
        sign @ (b'+' | b'-') if idx + 6 == bytes.len() && bytes[idx + 3] == b':' => {
            let offset_hour = num(idx + 1..idx + 3)?;
            let offset_minute = num(idx + 4..idx + 6)?;
            if offset_hour > 23 || offset_minute > 59 {
                return None;
            }
            let total = offset_hour * 3_600 + offset_minute * 60;
            if *sign == b'+' {
                total
            } else {
                -total
            }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second.min(59) - offset_secs)
}

/// Days in `month` of `year`, Gregorian.
const fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
    }
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

// ---- Config read and apply (ADR 010) ----------------------------------------

/// Content hash of a config document: BLAKE3, lowercase hex.
///
/// Used as the concurrency token for `PUT /admin/config`. It is a change
/// detector, not a security boundary.
#[must_use]
fn config_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// `GET /admin/config` response.
#[derive(Debug, Serialize)]
pub struct ConfigDocument {
    /// The config file's contents, byte for byte.
    pub config: String,
    /// BLAKE3 hash of those bytes, to be echoed as `If-Match` on a PUT.
    pub hash: String,
}

/// Return the config file verbatim.
///
/// Deliberately NOT a serialisation of the in-memory `Config`: `Config::load`
/// merges the TOML file with `LUMEN_`-prefixed environment variables, so
/// rendering the merged struct would show environment overrides as if they
/// were file content, and a subsequent PUT would write them permanently into
/// the file. The operator must see and edit exactly what is on disk.
pub async fn get_config(State(state): State<AppState>) -> Result<Json<ConfigDocument>, ApiError> {
    let path = config_path(&state)?;
    let bytes = tokio::fs::read(path.as_ref())
        .await
        .map_err(|e| GatewayError::Internal(format!("reading config: {e}")))?;
    let config = String::from_utf8(bytes)
        .map_err(|_| GatewayError::Internal("config file is not valid UTF-8".to_owned()))?;
    let hash = config_hash(config.as_bytes());
    Ok(Json(ConfigDocument { config, hash }))
}

/// Apply a new config document.
///
/// The file stays the source of truth. The sequence is deliberate:
///
/// 1. Compare `If-Match` against the current file's hash, so two operators
///    editing at once cannot silently lose one edit.
/// 2. Stage the submitted bytes in a temporary file in the SAME directory
///    (a rename is only atomic within a filesystem).
/// 3. Validate the staged file. Writing first and validating after would
///    leave an invalid document on disk that breaks the next SIGHUP or
///    restart with no visible cause.
/// 4. Back up the current file, then rename the staged file into place.
/// 5. Ping the hot-reload trigger.
///
/// This route can repoint a provider's `base_url` and thereby redirect
/// customer traffic. It is the highest-privilege operation in the gateway,
/// which is why the apply is logged.
///
/// # Errors
///
/// A 400 (`LM-1001`) when `If-Match` is missing or the document is invalid
/// TOML or fails registry construction; a 412 (`LM-1004`) when `If-Match`
/// does not match the current file's hash.
pub async fn put_config(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Result<StatusCode, ApiError> {
    let path = config_path(&state)?;

    let Some(if_match) = headers.get("if-match").and_then(|v| v.to_str().ok()) else {
        return Err(GatewayError::InvalidRequest(
            "`If-Match` is required: GET /admin/config first and echo its hash".to_owned(),
        )
        .into());
    };
    let if_match = if_match.trim_matches('"').to_owned();

    let path_for_blocking = path.clone();
    let lock = Arc::clone(&state.config_apply_lock);
    let outcome = tokio::task::spawn_blocking(move || {
        apply_config_document(&path_for_blocking, &body, &if_match, &lock)
    })
    .await
    .map_err(|e| GatewayError::Internal(format!("config apply task failed: {e}")))?;
    outcome?;

    if let Some(trigger) = &state.reload_trigger {
        trigger.notify_one();
        tracing::info!("config applied through the admin API; hot reload requested");
    } else {
        tracing::info!(
            "config applied through the admin API; no reloader armed, applies at restart"
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The blocking half of [`put_config`]: hash check, staged write, validation,
/// backup and atomic rename. Runs on a blocking thread; never on the runtime.
///
/// The whole sequence runs under `lock` (see [`AppState::config_apply_lock`]):
/// two concurrent `PUT`s must never interleave, or the second could clobber
/// the first's staged bytes before validation runs, defeating the very
/// lost-update guarantee `If-Match` exists to provide. A poisoned lock is
/// surfaced as an internal error rather than silently recovered into: it can
/// only mean an earlier apply panicked mid-sequence (which the workspace's
/// no-`unwrap`/`expect`/`panic!` rule should make unreachable in practice),
/// and building on top of whatever state that left behind is not a risk
/// worth taking.
fn apply_config_document(
    path: &std::path::Path,
    body: &str,
    if_match: &str,
    lock: &std::sync::Mutex<()>,
) -> Result<(), ApiError> {
    let _guard = lock.lock().map_err(|_| {
        ApiError::from(GatewayError::Internal(
            "config apply lock poisoned by an earlier failed apply".to_owned(),
        ))
    })?;

    let current = std::fs::read(path)
        .map_err(|e| ApiError::from(GatewayError::Internal(format!("reading config: {e}"))))?;
    let old_hash = config_hash(&current);
    if old_hash != if_match {
        // See `describe_rejection`'s doc comment: `ApiError`'s `IntoResponse`
        // only logs `code`/`status` for a non-5xx response, so without this a
        // rejected apply (stale `If-Match` included) would leave no trace in
        // the gateway's own logs at all.
        tracing::warn!(
            config_path = %path.display(),
            current_hash = %old_hash,
            provided_if_match = %if_match,
            "config apply rejected: If-Match does not match the current file hash"
        );
        return Err(GatewayError::ConfigStale(
            "config changed since it was read; GET /admin/config and re-apply".to_owned(),
        )
        .into());
    }

    // Unique per call (process id + a monotonic counter), even though `lock`
    // already serialises every apply within this process: a process that was
    // killed or crashed mid-apply can leave a stale `.tmp` behind, and a
    // fixed name would let a later request mistake it for its own staging
    // file. Same directory as `path` throughout - a rename is only atomic
    // within one filesystem.
    let staged = path.with_extension(format!("toml.{}.tmp", unique_suffix()));
    // Armed BEFORE the file is created, not after the write block: a failing
    // `write_all` or `sync_all` (a full disk is the realistic trigger) returns
    // through `?` with the file already created, and a guard armed after the
    // block would never see it. From here on ANY early return must not leave
    // the staged file behind, and the guard makes that structural instead of
    // relying on every future `return Err(..)` to remember its own cleanup:
    // `commit()` is the only way to suppress the removal, and it is called
    // exactly once, after the rename that consumes the file. Arming it before
    // `File::create` costs nothing when creation itself fails, since removing
    // a path that was never created is ignored.
    let staging = StagingGuard::new(&staged);
    {
        // `File::create` + `write_all` + `sync_all`, not `std::fs::write`:
        // `sync_all` is the step that actually matters here. `rename` only
        // makes the NAME change atomic; it says nothing about whether the
        // bytes behind the old name ever reached disk. On ext4 with delayed
        // allocation in particular, the rename's metadata can be durable
        // while the data blocks are not, so a crash shortly after an apply
        // can leave `path` pointing at a zero-length file - `.bak` still
        // holds the previous good document, but nothing tells the operator
        // to reach for it, and `Config::load` refuses to boot on the
        // corrupt result. Flushing the data before the rename closes that
        // window. Scoped so the file handle (and its fsync) completes
        // before `validate_candidate` touches the path.
        let mut file = std::fs::File::create(&staged)
            .map_err(|e| ApiError::from(GatewayError::Internal(format!("staging config: {e}"))))?;
        file.write_all(body.as_bytes())
            .map_err(|e| ApiError::from(GatewayError::Internal(format!("staging config: {e}"))))?;
        file.sync_all()
            .map_err(|e| ApiError::from(GatewayError::Internal(format!("staging config: {e}"))))?;
    }

    // `File::create` above always creates with mode `0o666 & !umask` -
    // typically `0644` - regardless of what `path` was actually set to, and
    // the live file adopts the STAGED file's mode on rename, not the
    // original's (`std::fs::copy`, used for the `.bak` sibling below, DOES
    // preserve mode - only this staged-then-renamed file does not). Without
    // this, a config file an operator hardened to e.g. `0600` would silently
    // widen to whatever the process umask allows on the very first apply
    // through this route, exposing base URLs, model topology, `db_path` and
    // `api_key_env` names to any other local account on a shared host.
    // Best-effort, like `sync_parent_dir` below: on a filesystem with no
    // Unix permission model to speak of (CIFS/FAT-style mounts, some FUSE
    // layers), `chmod` fails not because a real permission would be widened,
    // but because there was never a permission bit to preserve in the first
    // place. Refusing the apply over that would turn a config change that
    // would previously have succeeded into a hard failure for a reason the
    // operator cannot fix from this side of the API.
    if let Err(error) = preserve_permissions(path, &staged) {
        tracing::warn!(
            %error,
            path = %path.display(),
            "failed to preserve the config file's permissions across the apply; \
             continuing, since a filesystem without a permission model has \
             nothing to protect"
        );
    }

    if let Err(error) = crate::reload::validate_candidate(&staged) {
        return Err(describe_rejection(path, &error));
    }

    let backup = path.with_extension("toml.bak");
    std::fs::copy(path, &backup)
        .map_err(|e| ApiError::from(GatewayError::Internal(format!("backing up config: {e}"))))?;
    std::fs::rename(&staged, path)
        .map_err(|e| ApiError::from(GatewayError::Internal(format!("applying config: {e}"))))?;
    staging.commit();

    // Fsync the containing directory too: a data-only fsync guarantees the
    // staged bytes are durable, but says nothing about whether the RENAME
    // itself (the directory-entry update that gives `path` its new
    // contents) survived a crash. Best-effort and non-fatal on purpose:
    // directory fsync is a documented no-op or an outright error on some
    // platforms (Windows in particular), and the apply has already
    // succeeded on disk by this point - failing the request over a
    // durability nicety it cannot control would be worse than logging and
    // moving on.
    sync_parent_dir(path);

    // ADR 010 names this the highest-privilege route in the gateway (it can
    // repoint a provider's `base_url` and thereby redirect customer
    // traffic), but the gateway itself has no acting identity to log: ADR
    // 010 decision 4 delegates human identity to the reverse proxy in front
    // of the console, which keeps its own audit log of who applied what.
    // What the gateway CAN and does record is the content-level fact of the
    // change: an incident responder reading gateway logs alone can see that
    // a config was applied through this API, and, by comparing hashes
    // against `GET /admin/config` history or backups, tell exactly what
    // changed. Hashes only, never content - this must never become a vector
    // for logging secrets or provider topology.
    tracing::info!(
        config_path = %path.display(),
        old_hash = %old_hash,
        new_hash = %config_hash(body.as_bytes()),
        "config applied through the admin API"
    );
    Ok(())
}

/// Preserve `source`'s Unix file mode on `target`, best-effort.
///
/// `std::fs::File::create` always creates a new file with mode
/// `0o666 & !umask`, never the mode of any existing file at a neighbouring
/// path, so staging a config document in a fresh file and renaming it into
/// place would otherwise silently change the live file's permissions on
/// every apply. A no-op on non-Unix targets: there is no equivalent
/// permission-bit model to copy there, and this route already only fsyncs a
/// parent directory (see [`sync_parent_dir`]) on a best-effort basis
/// elsewhere on non-Unix platforms.
///
/// The caller treats a returned error as best-effort too (log and continue,
/// exactly like [`sync_parent_dir`]): on a filesystem that implements no
/// Unix permission model at all (CIFS/FAT-style mounts, some FUSE layers),
/// `chmod` fails, but there was never a permission to widen, so there is
/// nothing to protect by refusing the apply. Do not turn this back into a
/// hard failure; that would reject an apply that would previously have
/// succeeded, for a condition the operator has no way to fix from the API.
#[cfg(unix)]
fn preserve_permissions(source: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(source)?.permissions().mode();
    std::fs::set_permissions(target, std::fs::Permissions::from_mode(mode))
}

/// Non-Unix targets have no permission bits to copy.
#[cfg(not(unix))]
fn preserve_permissions(
    _source: &std::path::Path,
    _target: &std::path::Path,
) -> std::io::Result<()> {
    Ok(())
}

/// Best-effort fsync of `path`'s containing directory, so a rename into
/// `path` is durable across a crash, not just present in the page cache.
/// Never returns an error: see the call site in [`apply_config_document`]
/// for why a failure here must not fail an apply that already landed.
fn sync_parent_dir(path: &std::path::Path) {
    let parent = match path.parent() {
        // A bare filename (e.g. "lumen.toml", no directory component) has
        // an empty parent; its containing directory is the CWD.
        Some(p) if p.as_os_str().is_empty() => std::path::Path::new("."),
        Some(p) => p,
        None => return,
    };
    if let Err(error) = std::fs::File::open(parent).and_then(|dir| dir.sync_all()) {
        tracing::warn!(
            %error,
            path = %parent.display(),
            "failed to fsync the config directory after applying a new config; \
             the rename may not survive a crash even though the apply itself succeeded"
        );
    }
}

/// A process id + monotonic counter suffix, unique within this process's
/// lifetime. Not a security token - only meant to keep a crash-orphaned
/// staging file from colliding with a live one; the ordinary case is
/// serialised entirely by `config_apply_lock`.
fn unique_suffix() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{n}", std::process::id())
}

/// Removes the staged config file on drop, unless [`commit`](Self::commit)
/// was called first. Exists so every failure path out of
/// [`apply_config_document`] - validation, backup, or the final rename -
/// cleans up the staging file without each call site having to remember to.
struct StagingGuard<'a> {
    path: &'a std::path::Path,
    committed: bool,
}

impl<'a> StagingGuard<'a> {
    /// Guard `path`, the staging file, which need NOT exist yet: the guard is
    /// armed before creation so a failed write or fsync cannot leak a
    /// partially written file, and `Drop` ignores a path that is not there.
    fn new(path: &'a std::path::Path) -> Self {
        Self {
            path,
            committed: false,
        }
    }

    /// Declare the staged file consumed (renamed into place): `Drop` must
    /// not attempt to remove a path that no longer names the staging file.
    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for StagingGuard<'_> {
    fn drop(&mut self) {
        if !self.committed {
            // Best-effort: the file may already be gone (e.g. a concurrent
            // cleanup), and there is no client left to report a failure to.
            let _ = std::fs::remove_file(self.path);
        }
    }
}

/// Turn a `validate_candidate` failure into the client-facing rejection,
/// without leaking the staged file's filesystem path.
///
/// `ConfigError::Parse` and `ConfigError::Validation` both have a `path`
/// field carrying that path, and their own `Display` (used by `{error}`
/// below, never by this function) names it - correctly, since that `Display`
/// is what reaches the log line just below, not the client. This function
/// instead takes each variant's `message` field alone: `Validation`'s is
/// hand-written by `Config::validate` and never contained a path to begin
/// with, and `Parse`'s no longer does either (`describe_figment_error` in
/// `config.rs` strips figment's own trailing `" in {source} {name}"`, which
/// is the fragment that used to carry it). `RegistryError`'s variants never
/// embed a path, so those pass through unchanged.
///
/// The full detail - path included - still needs to reach a human somewhere,
/// since a rejected apply is exactly the kind of thing an operator wants a
/// record of. It does NOT reach it via `ApiError`'s `IntoResponse`
/// (`crate::error`): that only logs `code` and `status` for a non-5xx
/// response, at `debug`, never the message or this error's `Display`. So
/// this function logs it directly, at `warn`, before scrubbing the message
/// down to what the client is allowed to see.
fn describe_rejection(path: &std::path::Path, error: &crate::reload::ReloadError) -> ApiError {
    tracing::warn!(
        config_path = %path.display(),
        error = %error,
        "config apply rejected"
    );
    let message = match error {
        crate::reload::ReloadError::Config(config_error) => match config_error {
            crate::config::ConfigError::Parse { message, .. }
            | crate::config::ConfigError::Validation { message, .. } => message.clone(),
            // The staged file was just written by this handler; it going
            // missing before validation runs is not a client mistake to
            // explain away as a bad document.
            crate::config::ConfigError::NotFound { .. } => {
                return GatewayError::Internal(
                    "staged config file disappeared before it could be validated".to_owned(),
                )
                .into();
            }
        },
        crate::reload::ReloadError::Registry(registry_error) => registry_error.to_string(),
    };
    GatewayError::InvalidRequest(format!("config rejected: {message}")).into()
}

/// The config file path this process booted from.
///
/// `None` only in tests: `main.rs` always sets it. A 500 is therefore the
/// honest answer, not a client error, because nothing the caller sent caused
/// it. `GatewayError` has no `NotFound(String)` variant, and inventing one
/// for a condition that cannot occur in production would be noise.
fn config_path(state: &AppState) -> Result<Arc<std::path::PathBuf>, ApiError> {
    state
        .config_path
        .clone()
        .ok_or_else(|| GatewayError::Internal("no config file backs this server".to_owned()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_key_debug_never_shows_the_plaintext() {
        let created = CreatedKey {
            key: "fg-super-secret-plaintext".to_owned(),
            record: VirtualKeyRecord {
                id: "id-1".to_owned(),
                name: "debug-test".to_owned(),
                group_id: None,
                budget_max: None,
                budget_spent: 0.0,
                rpm_limit: None,
                tpm_limit: None,
                expires_at: None,
                disabled: false,
                created_at: 0,
                deleted_at: None,
            },
        };
        let dbg = format!("{created:?}");
        assert!(
            !dbg.contains("fg-super-secret-plaintext"),
            "Debug output leaked the plaintext: {dbg}"
        );
        assert!(dbg.contains("REDACTED"), "Debug output was: {dbg}");
    }

    #[test]
    fn unix_seconds_pass_through() {
        assert_eq!(parse_time_param("0"), Some(0));
        assert_eq!(parse_time_param("1752537600"), Some(1_752_537_600));
    }

    #[test]
    fn rfc3339_utc_matches_known_epochs() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2001-09-09T01:46:40Z"), Some(1_000_000_000));
        // Leap-year day.
        assert_eq!(parse_rfc3339("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        assert_eq!(parse_rfc3339("2026-07-15T00:00:00Z"), Some(1_784_073_600));
    }

    #[test]
    fn rfc3339_offsets_and_fractions_are_honored() {
        // +02:00 means two hours EARLIER in UTC.
        assert_eq!(
            parse_rfc3339("2026-07-15T02:00:00+02:00"),
            Some(1_784_073_600)
        );
        assert_eq!(
            parse_rfc3339("2026-07-14T22:00:00-02:00"),
            Some(1_784_073_600)
        );
        // Fractional seconds truncate; lowercase t/z accepted.
        assert_eq!(parse_rfc3339("1970-01-01t00:00:00.999z"), Some(0));
    }

    #[test]
    fn malformed_timestamps_are_rejected() {
        for bad in [
            "",
            "not-a-time",
            "2026-07-15",
            "2026-07-15T00:00:00",       // no offset
            "2026-13-01T00:00:00Z",      // month 13
            "2026-02-30T00:00:00Z",      // Feb 30
            "2025-02-29T00:00:00Z",      // not a leap year
            "2026-07-15T24:00:00Z",      // hour 24
            "2026-07-15T00:00:00+25:00", // offset hour 25
            "2026-07-15T00:00:00.Z",     // empty fraction
            "2026-07-15T00:00:00Zx",     // trailing garbage
            "-123",                      // negative unix seconds
        ] {
            assert_eq!(parse_time_param(bad), None, "{bad} must be rejected");
        }
    }
}
