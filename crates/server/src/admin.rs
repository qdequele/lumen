//! Minimal admin API (M5 §5.5): key management under `/admin`, protected by
//! the master key (see `auth::require_master_key`).
//!
//! * `POST /admin/keys` - create a key. The response is the ONLY place the
//!   plaintext key ever appears; it is never stored and never logged.
//! * `GET /admin/keys` - list keys (records only, no hashes, no plaintext).
//! * `PATCH /admin/keys/{id}` - adjust budgets/limits, enable/disable.
//! * `PUT /admin/provider-keys/{name}` - store a provider API key encrypted
//!   at rest (AES-256-GCM under the master key) and apply it without a restart
//!   by requesting a hot reload; used for providers whose `api_key_env` is
//!   unset or empty (env keeps precedence when set).
//! * `GET /admin/usage` - aggregated usage and spend reporting over the
//!   `usage_log` table (issue #64).
//!
//! Every change is applied to the database AND the in-memory state, so it
//! takes effect immediately without a restart.

use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use lumen_auth::key::hash_key;
use lumen_auth::store::{
    KeyPatch, NewKey, UsageAggregate, UsageFilter, UsageGroupBy, VirtualKeyRecord,
};
use lumen_core::GatewayError;
use serde::{Deserialize, Serialize};

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
// (the one intended exposure) goes through `Serialize` only.
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
    let (plaintext, record) = auth
        .store
        .create_key(params)
        .await
        .map_err(|e| internal(&e))?;
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

/// List every key (no secrets: ids, names, budgets, limits, flags).
pub async fn list_keys(
    State(state): State<AppState>,
) -> Result<Json<Vec<VirtualKeyRecord>>, ApiError> {
    let auth = runtime(&state)?;
    let keys = auth.store.list_keys().await.map_err(|e| internal(&e))?;
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
    let updated = auth
        .store
        .update_key(&id, patch)
        .await
        .map_err(|e| internal(&e))?
        .ok_or_else(|| GatewayError::InvalidRequest(format!("unknown key id '{id}'")))?;
    // Reflect the change in the live table (spend is preserved).
    auth.keys.apply(&updated);
    Ok(Json(updated))
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
    /// `capability` | `key_id` | `status` | `total`.
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
                 provider, capability, key_id, status, total"
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
                budget_max: None,
                budget_spent: 0.0,
                rpm_limit: None,
                tpm_limit: None,
                expires_at: None,
                disabled: false,
                created_at: 0,
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
