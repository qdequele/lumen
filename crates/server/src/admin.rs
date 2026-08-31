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
//! * `GET /admin/webhooks` - the live outbound-webhook configuration, plus
//!   which source it came from and whether deliveries are signed. Never the
//!   signing secret (ADR 011 amendment).
//! * `PUT /admin/webhooks` - replace every webhook setting. Applied
//!   immediately and stored, so it survives a restart and is not undone by
//!   the next config reload.
//! * `DELETE /admin/webhooks` - stop emitting events, persistently.
//! * `PUT /admin/webhooks/signing-key` - store the HMAC signing secret,
//!   sealed at rest (AES-256-GCM under the master key), for a control plane
//!   that cannot set the gateway's environment.
//! * `DELETE /admin/webhooks/signing-key` - forget the stored secret.
//! * `GET /admin/usage` - aggregated usage and spend reporting over the
//!   `usage_log` table (issue #64).
//! * `GET /admin/usage/export` - cursor-paginated raw `usage_log` rows, for a
//!   control plane building its own multi-dimensional view (ADR 010).
//! * `GET /admin/config` - the current dynamic config document verbatim, plus
//!   a BLAKE3 content hash to be echoed as `If-Match` on the `PUT` that
//!   applies a new one (ADR 010, generalised to any [`ConfigSource`] by
//!   ADR 012). File mode: the whole config file, byte for byte. DB mode: the
//!   document stored in `config_versions`, or the empty string with the
//!   empty document's hash before the very first `PUT`. Never a serialisation
//!   of the merged in-memory `Config`: that would render environment
//!   overrides, or (in DB mode) the boot-layer file, as if they were dynamic
//!   document content.
//! * `PUT /admin/config` - apply a new config document, in either mode
//!   (ADR 010, ADR 012). Requires an `If-Match` header carrying the current
//!   hash from `GET /admin/config`; a missing header is a 400 (`LM-1001`), a
//!   stale hash a 412 (`LM-1004`). The candidate's boot-layer fields
//!   (`server.*`, `log_format`, `auth.enabled`, `auth.db_path`,
//!   `config_source`) are compared against the current document's first
//!   ([`boot_layer_diff`](crate::config::boot_layer_diff)): any difference is
//!   a 400 (`LM-1001`) naming the changed keys, since a boot-layer value only
//!   takes effect on a restart and this route applies everything it accepts
//!   immediately. File mode: since the candidate IS the whole document, an
//!   unchanged boot-layer block passes and only an actual edit to one of
//!   those fields is refused. DB mode: the current document (read from
//!   `ConfigSource`) never holds boot keys at all (ADR 012 §1), so any
//!   boot-layer key present in the CANDIDATE that differs from the built-in
//!   default is refused the same way - a well-formed db-mode candidate
//!   carries no boot-layer keys whatsoever. The candidate is then validated
//!   (parse, semantic validation, a throwaway registry build) and persisted
//!   through the configured `ConfigSource` (a staged write with a `.bak`
//!   backup in file mode; an immutable, CAS-guarded row insert in DB mode).
//!   This is the highest-privilege route in the gateway: it can repoint a
//!   provider's `base_url` and thereby redirect customer traffic.
//!
//! Every change is applied to the database AND the in-memory state, so it
//! takes effect immediately without a restart.

use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use lumen_auth::events::{EventKind, SettingsOrigin, SettingsSource, WebhookSettings};
use lumen_auth::key::hash_key;
use lumen_auth::state::micro_to_usd;
use lumen_auth::store::{
    DeleteGroupOutcome, GroupPatch, GroupRecord, KeyPatch, NewGroup, NewKey, UsageAggregate,
    UsageFilter, UsageGroupBy, VirtualKeyRecord,
};
use lumen_core::GatewayError;
use serde::{Deserialize, Serialize};
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

// ---- Outbound webhooks (ADR 011 amendment) ----------------------------------

/// `GET /admin/webhooks` response: what the gateway is actually doing, and
/// where that came from.
///
/// Deliberately reports the signing *state* (`signed`, `signing_key_stored`)
/// and the variable *name*, never the secret - the same contract the provider
/// surface keeps for API keys.
#[derive(Debug, Serialize)]
pub struct WebhookStatus {
    /// Whether budget events are currently being emitted.
    pub enabled: bool,
    /// Which source the live settings came from: a stored row beats the
    /// `[webhooks]` config block.
    pub source: SettingsSource,
    /// The settings in force; omitted when webhooks are off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<WebhookSettings>,
    /// Whether deliveries carry an `x-lumen-signature` header.
    pub signed: bool,
    /// Whether a signing secret is sealed in the database.
    pub signing_key_stored: bool,
    /// When the stored row was last written, unix seconds; omitted when the
    /// live settings come from the config file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
}

/// `PUT /admin/webhooks/signing-key` request body.
#[derive(Deserialize)]
pub struct WebhookSecretBody {
    /// The HMAC-SHA256 signing secret. Sealed at rest immediately; never
    /// logged, never returned by any route.
    pub secret: String,
}

// STRICT rule 5: the secret must be unrepresentable through `Debug`, so a
// stray `{:?}` on the extracted body cannot leak it.
impl std::fmt::Debug for WebhookSecretBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookSecretBody")
            .field("secret", &"REDACTED")
            .finish()
    }
}

/// The webhook controller, or a 401 when auth (and therefore the whole
/// `/admin` surface) is off.
fn webhooks(state: &AppState) -> Result<&Arc<crate::webhooks::WebhookController>, ApiError> {
    state
        .webhooks
        .as_ref()
        .ok_or_else(|| GatewayError::Unauthorized.into())
}

/// Build the current status snapshot.
fn webhook_status(controller: &crate::webhooks::WebhookController) -> WebhookStatus {
    let live = controller.live_settings();
    let source = controller.source();
    WebhookStatus {
        enabled: live.is_some(),
        // A stored row's timestamp is only meaningful when that row is what is
        // in force; a config-file deployment has no such moment.
        updated_at: match source {
            SettingsSource::Database => controller.stored().map(|s| s.updated_at),
            SettingsSource::Config | SettingsSource::None => None,
        },
        source,
        settings: live,
        signed: controller.is_signed(),
        signing_key_stored: controller.has_stored_secret(),
    }
}

/// Report the live webhook configuration.
pub async fn get_webhooks(State(state): State<AppState>) -> Result<Json<WebhookStatus>, ApiError> {
    let controller = webhooks(&state)?;
    Ok(Json(webhook_status(controller)))
}

/// Replace every webhook setting. Validated, persisted, then applied - in that
/// order, so a rejected document never reaches the live pipeline and a
/// successful one survives a restart.
///
/// Invalid settings are a 400 `LM-1001` naming the field, like every other
/// admin write. An unresolvable signing secret (a `signing_key_env` naming a
/// variable this process cannot see, with nothing stored) is the same: it
/// would silently downgrade a billing integration to unsigned deliveries.
pub async fn put_webhooks(
    State(state): State<AppState>,
    payload: Result<Json<WebhookSettings>, JsonRejection>,
) -> Result<Json<WebhookStatus>, ApiError> {
    let Json(settings) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    let auth = runtime(&state)?;
    let controller = webhooks(&state)?;
    settings
        .validate(SettingsOrigin::AdminApi)
        .map_err(GatewayError::InvalidRequest)?;

    // Persist, refresh the cache, then apply. Applying first would leave the
    // database disagreeing with a running pipeline if the write then failed;
    // this order means a failed apply leaves the stored row ahead of the live
    // one, which `GET /admin/webhooks` makes visible and the next reload
    // reconciles.
    auth.store
        .save_webhook_config(&settings)
        .await
        .map_err(|e| internal(&e))?;
    controller
        .refresh_from_store(&auth.store, auth.master.as_ref())
        .await;
    controller
        .apply(&settings, &auth.keys)
        .map_err(GatewayError::InvalidRequest)?;
    Ok(Json(webhook_status(controller)))
}

/// Stop emitting budget events, persistently.
///
/// Idempotent: a 200 whether or not anything was enabled. The decision is
/// always persisted - as a *disabled* row rather than an absent one - so
/// neither a `[webhooks]` config block nor an enabled-but-unappliable stored
/// row can bring delivery back on the next reload. Keeping the settings on
/// that row means a later re-enable does not have to resend them.
pub async fn delete_webhooks(
    State(state): State<AppState>,
) -> Result<Json<WebhookStatus>, ApiError> {
    let auth = runtime(&state)?;
    let controller = webhooks(&state)?;
    match controller.live_settings() {
        // Store the live settings as a DISABLED row: the `[webhooks]` config
        // block cannot then re-enable them on the next reload, and a later
        // re-enable does not have to resend them.
        Some(settings) => auth.store.save_webhook_config_disabled(&settings).await,
        // Nothing is live, but a stored row may still say `enabled = 1`: the
        // boot or reload apply can fail (a `signing_key_env` this process
        // cannot resolve, say) and leave an enabled row with no pipeline
        // behind it. Clearing that flag is what makes this DELETE outlive the
        // next reload instead of being retried by it.
        None => auth
            .store
            .disable_webhook_config()
            .await
            .map(|_disabled| ()),
    }
    .map_err(|e| internal(&e))?;
    controller.disable(&auth.keys);
    controller
        .refresh_from_store(&auth.store, auth.master.as_ref())
        .await;
    Ok(Json(webhook_status(controller)))
}

/// Store (or rotate) the HMAC signing secret, sealed with the master key.
///
/// The secret is sealed before it can reach a log line, and no route ever
/// returns it. A rotation applies to the next delivery attempt: the sender
/// reads its key cell per event, so nothing restarts.
pub async fn put_webhook_signing_key(
    State(state): State<AppState>,
    payload: Result<Json<WebhookSecretBody>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(body) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    if body.secret.is_empty() {
        return Err(GatewayError::InvalidRequest("`secret` must not be empty".to_owned()).into());
    }
    let auth = runtime(&state)?;
    let controller = webhooks(&state)?;
    let master = auth.master.as_ref().ok_or_else(|| {
        ApiError::from(GatewayError::Internal(
            "no master key is available to seal the webhook secret".to_owned(),
        ))
    })?;
    auth.store
        .store_webhook_secret(&body.secret, master)
        .await
        .map_err(|e| internal(&e))?;
    controller
        .refresh_from_store(&auth.store, auth.master.as_ref())
        .await;
    // Re-apply so the rotation takes effect now rather than at the next
    // reload. A no-op when webhooks are off - the secret simply waits.
    if let Some(settings) = controller.live_settings() {
        controller
            .apply(&settings, &auth.keys)
            .map_err(GatewayError::InvalidRequest)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Forget the stored signing secret.
///
/// Refused with a 400 when that would leave the live configuration unable to
/// sign at all *and* it names an environment variable this process cannot see:
/// dropping to unsigned billing events by accident is the failure mode the
/// whole signing scheme exists to prevent. Deliberately unsigned deliveries
/// (no `signing_key_env`, no stored secret) remain allowed.
pub async fn delete_webhook_signing_key(
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    let auth = runtime(&state)?;
    let controller = webhooks(&state)?;
    auth.store
        .delete_webhook_secret()
        .await
        .map_err(|e| internal(&e))?;
    controller
        .refresh_from_store(&auth.store, auth.master.as_ref())
        .await;
    if let Some(settings) = controller.live_settings() {
        // The live pipeline keeps its old key on failure (`apply` swaps
        // nothing when it errors), so delivery stays signed until the operator
        // supplies a working source - the refusal is advisory, not a rollback.
        controller
            .apply(&settings, &auth.keys)
            .map_err(GatewayError::InvalidRequest)?;
    }
    Ok(StatusCode::NO_CONTENT)
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

// ---- Config read and apply (ADR 010, ADR 012) -------------------------------

use crate::config::{
    boot_layer_diff, AuthDynamicKnobs, Config, ConfigError, ImageFetchConfig, ProviderConfig,
    ResilienceConfig, TelemetryConfig, TokenizerConfig, WebhooksConfig,
};
use crate::config_edit;
use crate::config_source::{ConfigContext, ConfigLoadError, ConfigSourceError};

/// `GET /admin/config` response.
#[derive(Debug, Serialize)]
pub struct ConfigDocument {
    /// The current dynamic document's contents, byte for byte (empty before
    /// the first `PUT` in DB mode).
    pub config: String,
    /// BLAKE3 hash of those bytes, to be echoed as `If-Match` on a PUT.
    pub hash: String,
}

/// Return the current dynamic config document verbatim, in either mode.
///
/// Deliberately NOT a serialisation of the in-memory `Config`: `Config::load`
/// (file mode) merges the TOML file with `LUMEN_`-prefixed environment
/// variables and (DB mode) the separate boot-layer file, so rendering the
/// merged struct would show environment overrides or boot-layer content as if
/// they were part of the dynamic document, and a subsequent PUT would write
/// them permanently into it. The operator must see and edit exactly what
/// [`ConfigSource::load`](crate::config_source::ConfigSource::load) reports.
pub async fn get_config(State(state): State<AppState>) -> Result<Json<ConfigDocument>, ApiError> {
    let ctx = config_ctx(&state)?;
    let doc = ctx
        .source
        .load()
        .await
        .map_err(|e| source_internal_error(&e))?;
    Ok(Json(ConfigDocument {
        config: doc.toml,
        hash: doc.hash,
    }))
}

/// Apply a new config document.
///
/// The submitted `body` is required to be a well-formed candidate document
/// for `ctx`'s mode (file mode: the whole document; DB mode: the dynamic
/// document alone, no boot-layer keys). `PUT /admin/config` is a thin
/// wrapper: header extraction here, then the shared [`apply_document`]
/// pipeline (also used by every granular config endpoint, Task 8).
///
/// # Errors
///
/// A 400 (`LM-1001`) when `If-Match` is missing, a boot-layer key differs
/// from the current document, or the candidate is invalid TOML or fails
/// registry construction; a 412 (`LM-1004`) when `If-Match` does not match
/// the current document's hash.
pub async fn put_config(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Result<StatusCode, ApiError> {
    let if_match = require_if_match(&headers)?;
    apply_document(&state, body, &if_match).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Extract and unquote the `If-Match` header every mutating config write
/// requires (the whole-document `PUT` and every granular endpoint, Task 8).
/// Missing is `LM-1001` (400) - the identical message on every path that
/// requires it, so a client cannot tell which endpoint rejected it by text
/// alone.
fn require_if_match(headers: &axum::http::HeaderMap) -> Result<String, ApiError> {
    let Some(if_match) = headers.get("if-match").and_then(|v| v.to_str().ok()) else {
        return Err(GatewayError::InvalidRequest(
            "`If-Match` is required: GET /admin/config first and echo its hash".to_owned(),
        )
        .into());
    };
    Ok(if_match.trim_matches('"').to_owned())
}

/// Apply a candidate config document through `state.config`'s
/// [`ConfigSource`](crate::config_source::ConfigSource), in either mode.
/// Shared by `PUT /admin/config` and every granular config endpoint
/// (Task 8, ADR 012).
///
/// The whole sequence runs under `state.config_apply_lock` (a
/// `tokio::sync::Mutex`, held across every `.await` below): two concurrent
/// applies must never interleave, or the second could pass its own
/// `If-Match` check against a hash the first has already moved past,
/// defeating the very lost-update guarantee `If-Match` exists to provide.
///
/// 1. Compare `if_match` against the current document's hash
///    ([`ConfigSource::load`](crate::config_source::ConfigSource::load)),
///    BEFORE any validation work - so a stale request never even glances at
///    the candidate body. A mismatch is `LM-1004` (412).
/// 2. Diff the current and candidate documents' boot-layer fields
///    ([`boot_layer_diff`]): any difference is `LM-1001` (400) naming the
///    changed keys. A boot-layer value (`server.*`, `log_format`,
///    `auth.enabled`, `auth.db_path`, `config_source`) only takes effect on a
///    restart, and this route applies everything it accepts immediately.
///    File mode: the candidate is the whole document, so an UNCHANGED
///    boot-layer block passes and only an actual edit is refused. DB mode:
///    the current document never holds boot keys at all (ADR 012 §1), so any
///    boot-layer key the candidate DOES carry, if it differs from the
///    built-in default, is refused the same way.
/// 3. Validate the candidate
///    ([`ConfigContext::validate_document`](crate::config_source::ConfigContext::validate_document):
///    parse, semantic validation, a throwaway registry build) - `LM-1001`
///    (400) on failure, logged in full at `warn` and scrubbed of any
///    filesystem path before it reaches the client
///    ([`describe_validation_rejection`]).
/// 4. Persist through `ctx.source`. Its own compare-and-swap is a second,
///    independent line of defense against a write racing a CONCURRENT
///    EXTERNAL edit (a human editor or GitOps sync outside this lock, in
///    file mode) - `LM-1004` (412) on that race too.
/// 5. Ping the hot-reload trigger, if one is armed, so the new document
///    applies without a restart.
async fn apply_document(
    state: &AppState,
    candidate: String,
    if_match: &str,
) -> Result<(), ApiError> {
    let ctx = config_ctx(state)?;
    let _guard = state.config_apply_lock.lock().await;

    let current = ctx
        .source
        .load()
        .await
        .map_err(|e| source_internal_error(&e))?;
    if current.hash != if_match {
        tracing::warn!(
            current_hash = %current.hash,
            provided_if_match = %if_match,
            "config apply rejected: If-Match does not match the current document hash"
        );
        return Err(stale_config_error());
    }

    let diff = boot_layer_diff(&current.toml, &candidate).map_err(|error| {
        // `boot_layer_diff` labels both sides "current"/"candidate" (never a
        // real path - see its own doc comment), so `error.to_string()` is
        // already safe to hand to the client directly; still logged at `warn`
        // for the same reason every other rejection below is (see
        // `describe_validation_rejection`'s doc comment): `ApiError`'s
        // `IntoResponse` only logs at `debug` for a non-5xx response.
        tracing::warn!(%error, "config apply rejected: candidate document failed to parse");
        GatewayError::InvalidRequest(error.to_string())
    })?;
    if !diff.is_empty() {
        tracing::warn!(
            keys = %diff.join(", "),
            "config apply rejected: restart-only boot-layer keys changed"
        );
        return Err(GatewayError::InvalidRequest(format!(
            "restart-only keys changed: {}; edit the boot config file and restart",
            diff.join(", ")
        ))
        .into());
    }

    ctx.validate_document(&candidate)
        .await
        .map_err(|error| describe_validation_rejection(&error))?;

    let new_hash = match ctx.source.persist(&candidate, &current.hash).await {
        Ok(hash) => hash,
        Err(ConfigSourceError::Stale { current_hash }) => {
            tracing::warn!(
                current_hash = %current_hash,
                provided_if_match = %if_match,
                "config apply rejected: If-Match does not match the current document hash \
                 (lost the race at persist time)"
            );
            return Err(stale_config_error());
        }
        Err(other) => return Err(source_internal_error(&other)),
    };

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
    // for logging secrets or provider topology. `old_hash` is `if_match`
    // itself: `persist` only returns `Ok` when the document it replaced
    // still hashed to exactly that.
    tracing::info!(
        old_hash = %if_match,
        new_hash = %new_hash,
        "config applied through the admin API"
    );

    if let Some(trigger) = &state.reload_trigger {
        trigger.notify_one();
        tracing::info!("config applied through the admin API; hot reload requested");
    } else {
        tracing::info!(
            "config applied through the admin API; no reloader armed, applies at restart"
        );
    }
    Ok(())
}

/// `LM-1004` (412), the same message on every path that produces it (the
/// fast `If-Match` check in [`apply_document`] and `ConfigSource::persist`'s
/// own compare-and-swap alike).
fn stale_config_error() -> ApiError {
    GatewayError::ConfigStale(
        "config changed since it was read; GET /admin/config and re-apply".to_owned(),
    )
    .into()
}

/// Map a [`ConfigSourceError`] from `load`/`persist` (I/O, DB, not-UTF-8) to
/// an opaque 500: never a client mistake, and never safe to echo back
/// (a filesystem or database detail).
fn source_internal_error(error: &ConfigSourceError) -> ApiError {
    GatewayError::Internal(error.to_string()).into()
}

/// Turn a [`ConfigContext::validate_document`](crate::config_source::ConfigContext::validate_document)
/// failure into the client-facing rejection, without leaking a filesystem
/// path.
///
/// `ConfigError::Parse` and `ConfigError::Validation` both have a `path`
/// field - in file mode, `ConfigContext::validate_document` labels it with
/// the REAL boot file path (there is no staging file to leak instead, but
/// the real path is no more the client's business than a staging path was) -
/// and their own `Display` (used by `{error}` below, never by this function)
/// names it. This function instead takes each variant's `message` field
/// alone: `Validation`'s is hand-written by `Config::validate` and never
/// contained a path to begin with, and `Parse`'s no longer does either
/// (`describe_figment_error` in `config.rs` strips figment's own trailing
/// `" in {source} {name}"`, which is the fragment that used to carry it).
/// `RegistryError`'s variants never embed a path, so those pass through
/// unchanged.
///
/// The full detail - path included - still needs to reach a human somewhere,
/// since a rejected apply is exactly the kind of thing an operator wants a
/// record of. It does NOT reach it via `ApiError`'s `IntoResponse`
/// (`crate::error`): that only logs `code` and `status` for a non-5xx
/// response, at `debug`, never the message or this error's `Display`. So
/// this function logs it directly, at `warn`, before scrubbing the message
/// down to what the client is allowed to see.
fn describe_validation_rejection(error: &ConfigLoadError) -> ApiError {
    tracing::warn!(error = %error, "config apply rejected: candidate document failed validation");
    match error {
        ConfigLoadError::Config(config_error) => match config_error {
            ConfigError::Parse { message, .. } | ConfigError::Validation { message, .. } => {
                GatewayError::InvalidRequest(format!("config rejected: {message}")).into()
            }
            // `validate_document` parses the candidate TEXT directly (file
            // mode) or merges it against `ctx.boot_path` (DB mode); either
            // way this can only mean the boot file itself vanished between
            // the hash check above and validation - not a client mistake to
            // explain away as a bad document.
            ConfigError::NotFound { .. } => GatewayError::Internal(
                "the boot config file this process needs to validate against is missing".to_owned(),
            )
            .into(),
            // Same shape as Parse/Validation above: name the field, drop the
            // `path` (this variant's own is the caller-supplied label, not a
            // path derived from the candidate, but there is nothing
            // client-useful in repeating it here either).
            ConfigError::DynamicKeyInBootConfig { key, .. } => {
                GatewayError::InvalidRequest(format!(
                    "config rejected: unexpected key '{key}' in boot-only document: remove it \
                     from the boot file, or set config_source = \"file\""
                ))
                .into()
            }
        },
        ConfigLoadError::Registry(registry_error) => {
            GatewayError::InvalidRequest(format!("config rejected: {registry_error}")).into()
        }
        // `validate_document`'s own doc comment guarantees it never calls a
        // `ConfigSource` method, so this arm is unreachable in practice;
        // treated as internal rather than assumed safe to show a client.
        ConfigLoadError::Source(source_error) => {
            GatewayError::Internal(source_error.to_string()).into()
        }
    }
}

// ---- Granular config endpoints (ADR 012 Task 8) -----------------------------
//
// Every route below shares `apply_document`'s pipeline: load the current
// document, run a `toml_edit` patch that touches only the field(s) this
// request named (comments and formatting elsewhere survive), then hand the
// resulting candidate to the exact same apply-lock / If-Match / boot-layer /
// validate / persist / hot-reload sequence the whole-document `PUT
// /admin/config` uses. A granular edit can therefore never produce a
// document a restart would refuse, and a provider a validation-breaking edit
// would orphan (e.g. still referenced by another model's fallback chain) is
// rejected the same way the whole-document PUT would reject it - `LM-1001`
// naming the dependent model, from `Config::validate_fallbacks`.

/// Parse the current document's raw text into a [`Config`] - deliberately
/// WITHOUT the `LUMEN_*` environment overlay `Config::load` applies (see
/// `get_config`'s doc comment): a granular read must show exactly the
/// document's own declared values, never a value an env var happens to
/// override.
///
/// Every document reaching this function was already validated by
/// [`ConfigContext::validate_document`] at the moment it was persisted (the
/// whole-document `PUT` and every granular write both route through
/// `apply_document`, which validates before persisting), so a parse failure
/// here can only mean the STORED document itself is corrupt - an internal
/// inconsistency, never a client mistake, hence the opaque 500 rather than
/// any `LM-1001`.
fn config_from_document(text: &str) -> Result<Config, ApiError> {
    toml::from_str::<Config>(text).map_err(|error| {
        tracing::error!(
            %error,
            "stored config document failed to parse while serving a granular admin read; \
             this should be unreachable, since every persisted document is validated first"
        );
        GatewayError::Internal("stored config document is not valid".to_owned()).into()
    })
}

/// Map a [`config_edit::EditError`] to an opaque 500.
///
/// `config_edit`'s own doc comment is explicit that it never validates the
/// edit it performs - a syntactically sound but semantically invalid result
/// (e.g. a dangling fallback reference) is caught by `apply_document`'s
/// later `validate_document` call, not here, and surfaces as `LM-1001`
/// through that path instead. Reaching THIS function at all means the edit
/// itself failed against a document that was already validated when it was
/// persisted - an internal inconsistency, not a client mistake.
fn edit_internal_error(error: &config_edit::EditError) -> ApiError {
    tracing::error!(%error, "config document edit failed against an already-validated document");
    GatewayError::Internal(error.to_string()).into()
}

/// One entry in [`ProvidersList`]: just enough to pick a provider without
/// fetching its full config (which may include a nested `models` array).
#[derive(Debug, Serialize)]
pub struct ProviderSummary {
    /// The provider's unique name.
    pub name: String,
    /// Its [`lumen_providers::ProviderKind`], as the same lowercase string
    /// the config file itself uses (`kind = "..."`).
    pub kind: String,
}

/// `GET /admin/config/providers` response.
#[derive(Debug, Serialize)]
pub struct ProvidersList {
    /// Every provider in the current document, in document order.
    pub providers: Vec<ProviderSummary>,
    /// BLAKE3 hash of the current document, to be echoed as `If-Match` on a
    /// write - identical semantics to `GET /admin/config`'s own `hash`.
    pub hash: String,
}

/// List every provider in the current document by name and kind: the picker
/// view: `GET /admin/config/providers/{name}` is the full detail view.
pub async fn list_providers(
    State(state): State<AppState>,
) -> Result<Json<ProvidersList>, ApiError> {
    let ctx = config_ctx(&state)?;
    let doc = ctx
        .source
        .load()
        .await
        .map_err(|e| source_internal_error(&e))?;
    let cfg = config_from_document(&doc.toml)?;
    Ok(Json(ProvidersList {
        providers: cfg
            .providers
            .iter()
            .map(|p| ProviderSummary {
                name: p.name.clone(),
                kind: p.kind.as_str().to_owned(),
            })
            .collect(),
        hash: doc.hash,
    }))
}

/// `GET /admin/config/providers/{name}` response: the provider's full config
/// (never a secret, `api_key_env` is a variable NAME, exactly like the
/// whole-document `GET`) plus the current hash.
#[derive(Debug, Serialize)]
pub struct ProviderDocument {
    /// The provider's own fields, flattened into the top level of the
    /// response (not nested under a `provider` key).
    #[serde(flatten)]
    pub provider: ProviderConfig,
    /// See [`ProvidersList::hash`].
    pub hash: String,
}

/// Fetch a single provider's full config by name.
///
/// # Errors
/// `LM-1003` (404, the same style every other per-entity admin 404 uses)
/// when no provider with that name exists in the current document.
pub async fn get_provider(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<ProviderDocument>, ApiError> {
    let ctx = config_ctx(&state)?;
    let doc = ctx
        .source
        .load()
        .await
        .map_err(|e| source_internal_error(&e))?;
    let cfg = config_from_document(&doc.toml)?;
    let provider = cfg
        .providers
        .into_iter()
        .find(|p| p.name == name)
        .ok_or(GatewayError::RouteNotFound)?;
    Ok(Json(ProviderDocument {
        provider,
        hash: doc.hash,
    }))
}

/// Insert or replace a single provider by name.
///
/// The path `{name}` must equal the body's own `name` field - `LM-1001`
/// otherwise, since silently retargeting `{name}` to a differently-named
/// body would be a confusing way to rename a provider (delete the old one
/// and PUT the new name instead). Every other field is policed by
/// [`ProviderConfig`]'s own `deny_unknown_fields`. Requires `If-Match`; runs
/// through the shared [`apply_document`] pipeline, so e.g. a fallback
/// reference this write would leave dangling is rejected exactly like the
/// whole-document `PUT` would reject it.
pub async fn put_provider(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: axum::http::HeaderMap,
    payload: Result<Json<ProviderConfig>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let if_match = require_if_match(&headers)?;
    let Json(provider) = payload.map_err(|e| GatewayError::InvalidRequest(e.body_text()))?;
    if provider.name != name {
        return Err(GatewayError::InvalidRequest(format!(
            "path provider name '{name}' does not match the request body's 'name' field '{}'",
            provider.name
        ))
        .into());
    }

    let ctx = config_ctx(&state)?;
    let current = ctx
        .source
        .load()
        .await
        .map_err(|e| source_internal_error(&e))?;
    let candidate = config_edit::upsert_provider(&current.toml, &provider)
        .map_err(|e| edit_internal_error(&e))?;
    apply_document(&state, candidate, &if_match).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Remove a single provider by name.
///
/// # Errors
/// `LM-1003` (404) when no provider with that name exists in the current
/// document. `LM-1001` (400), from [`apply_document`]'s validation pass,
/// when the provider is still referenced by another model's fallback chain
/// - the rejection names the dependent model (`Config::validate_fallbacks`).
pub async fn delete_provider(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<StatusCode, ApiError> {
    let if_match = require_if_match(&headers)?;
    let ctx = config_ctx(&state)?;
    let current = ctx
        .source
        .load()
        .await
        .map_err(|e| source_internal_error(&e))?;
    let candidate = config_edit::delete_provider(&current.toml, &name)
        .map_err(|e| edit_internal_error(&e))?
        .ok_or(GatewayError::RouteNotFound)?;
    apply_document(&state, candidate, &if_match).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Extract the JSON value of one scalar config section from a parsed
/// [`Config`]. `None` for a name outside the fixed set `GET|PUT
/// /admin/config/{section}` serves (`resilience`, `telemetry`, `tokenizer`,
/// `image_fetch`, `webhooks`, `auth`) - the caller maps that to `LM-1003`
/// (404). `auth` reports [`AuthDynamicKnobs`] - the 5
/// dynamic knobs only, never `enabled`/`db_path` - derived from
/// `cfg.auth`.
///
/// `webhooks` alone can render `null`: `Config.webhooks` is `Option<..>`
/// (absent by default, ADR 011), unlike every other section here, which
/// `Config`'s own `#[serde(default)]` always resolves to SOME value even
/// when the document's table is absent.
fn section_json(
    cfg: &Config,
    section: &str,
) -> Option<Result<serde_json::Value, serde_json::Error>> {
    Some(match section {
        "resilience" => serde_json::to_value(cfg.resilience),
        "telemetry" => serde_json::to_value(&cfg.telemetry),
        "tokenizer" => serde_json::to_value(cfg.tokenizer),
        "image_fetch" => serde_json::to_value(&cfg.image_fetch),
        "webhooks" => serde_json::to_value(&cfg.webhooks),
        "auth" => serde_json::to_value(AuthDynamicKnobs::from(&cfg.auth)),
        _ => return None,
    })
}

/// `GET /admin/config/{section}` for the fixed set of scalar sections
/// (`resilience`, `telemetry`, `tokenizer`, `image_fetch`, `webhooks`,
/// `auth`): the section's current value plus the document hash, keyed by the
/// section's own name, e.g. `{"tokenizer": {"mode": "heuristic"}, "hash":
/// "..."}`.
///
/// `webhooks` reports the `[webhooks]` block from the DOCUMENT alone (`null`
/// when absent) - never the DB-backed `/admin/webhooks` surface (ADR 011
/// amendment), which this route does not touch and which may report a
/// different value when a stored row overrides the file.
///
/// # Errors
/// `LM-1003` (404, the same style every other per-entity admin 404 uses) for
/// a section name outside the fixed set.
pub async fn get_config_section(
    State(state): State<AppState>,
    Path(section): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ctx = config_ctx(&state)?;
    let doc = ctx
        .source
        .load()
        .await
        .map_err(|e| source_internal_error(&e))?;
    let cfg = config_from_document(&doc.toml)?;
    let value = match section_json(&cfg, &section) {
        None => return Err(GatewayError::RouteNotFound.into()),
        Some(Ok(value)) => value,
        Some(Err(error)) => {
            tracing::error!(
                %error,
                section = %section,
                "failed to serialize a config section for a granular admin GET"
            );
            return Err(
                GatewayError::Internal("failed to serialize config section".to_owned()).into(),
            );
        }
    };
    let mut envelope = serde_json::Map::with_capacity(2);
    envelope.insert(section, value);
    envelope.insert("hash".to_owned(), serde_json::Value::String(doc.hash));
    Ok(Json(serde_json::Value::Object(envelope)))
}

/// Deserialize `body` as `T` and replace `section` in `doc` wholesale via
/// [`config_edit::replace_section`]. Shared by every [`put_config_section`]
/// arm except `auth`, which merges into the existing table instead of
/// replacing it (see [`config_edit::replace_auth_knobs`]).
fn replace_section_from_json<T: serde::de::DeserializeOwned + serde::Serialize>(
    doc: &str,
    section: &str,
    body: &str,
) -> Result<String, ApiError> {
    let value: T = serde_json::from_str(body)
        .map_err(|error| GatewayError::InvalidRequest(error.to_string()))?;
    config_edit::replace_section(doc, section, &value).map_err(|e| edit_internal_error(&e))
}

/// `PUT /admin/config/{section}` for the same fixed set [`get_config_section`]
/// serves. The body is that section's own type - its `deny_unknown_fields`
/// polices its fields, so an `auth` body can carry only the 5 dynamic knobs
/// ([`AuthDynamicKnobs`]): a body naming `enabled` or `db_path` (boot-layer,
/// restart-only) is rejected 400, never silently ignored.
///
/// `auth` applies via [`config_edit::replace_auth_knobs`] - a field-level
/// merge into the existing `[auth]` table, so `enabled`/`db_path` survive
/// untouched even though the request body never mentions them. Every other
/// section replaces wholesale via [`config_edit::replace_section`]. Requires
/// `If-Match`; runs through the shared [`apply_document`] pipeline like every
/// other write in this module.
///
/// # Errors
/// `LM-1003` (404) for a section name outside the fixed set.
pub async fn put_config_section(
    State(state): State<AppState>,
    Path(section): Path<String>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Result<StatusCode, ApiError> {
    let if_match = require_if_match(&headers)?;
    let ctx = config_ctx(&state)?;
    let current = ctx
        .source
        .load()
        .await
        .map_err(|e| source_internal_error(&e))?;

    let candidate = match section.as_str() {
        "resilience" => {
            replace_section_from_json::<ResilienceConfig>(&current.toml, &section, &body)?
        }
        "telemetry" => {
            replace_section_from_json::<TelemetryConfig>(&current.toml, &section, &body)?
        }
        "tokenizer" => {
            replace_section_from_json::<TokenizerConfig>(&current.toml, &section, &body)?
        }
        "image_fetch" => {
            replace_section_from_json::<ImageFetchConfig>(&current.toml, &section, &body)?
        }
        "webhooks" => replace_section_from_json::<WebhooksConfig>(&current.toml, &section, &body)?,
        "auth" => {
            let knobs: AuthDynamicKnobs = serde_json::from_str(&body)
                .map_err(|error| GatewayError::InvalidRequest(error.to_string()))?;
            config_edit::replace_auth_knobs(&current.toml, &knobs)
                .map_err(|e| edit_internal_error(&e))?
        }
        _ => return Err(GatewayError::RouteNotFound.into()),
    };

    apply_document(&state, candidate, &if_match).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The config context this process booted with (ADR 012).
///
/// `None` only in tests: `main.rs` always sets it. A 500 is therefore the
/// honest answer, not a client error, because nothing the caller sent caused
/// it. `GatewayError` has no `NotFound(String)` variant, and inventing one
/// for a condition that cannot occur in production would be noise.
fn config_ctx(state: &AppState) -> Result<Arc<ConfigContext>, ApiError> {
    state
        .config
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
