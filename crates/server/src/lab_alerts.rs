//! The `key.expiring` scan (ADR 015, amendment 2026-11-20).
//!
//! In Lab mode an hourly task reads the live, enabled keys of Lab teams that
//! expire within a week (or expired within a day) and reports each milestone
//! (7 days left, 1 day left, expired) once per key and expiry. The notice and
//! its outbox row are written in one transaction guarded by a unique row, so
//! a restart, an overlapping scan or a second replica never sends one twice;
//! the event id is the UUIDv5 the schema prescribes, so even a duplicate
//! would be deduplicated by the Lab. Never on the request path.

use std::time::Duration;

use lumen_auth::lab_alerts::{expiry_milestone, lab_account, LabAlert, Team};
use lumen_auth::store::KeyStore;
use lumen_auth::AuthError;

/// How often the scan runs.
pub const SCAN_INTERVAL: Duration = Duration::from_secs(3_600);
/// The widest milestone: keys expiring later than this are not read.
const HORIZON_SECS: i64 = 7 * 86_400;
/// How long after its expiry a key is still announced as expired: one scan
/// interval plus slack for a restart, so a key that expired long ago (or
/// before this feature shipped) is never announced.
const EXPIRED_GRACE_SECS: i64 = 86_400;

/// Run one scan at `now_ms`: report every milestone not reported yet.
/// Returns the number of events enqueued.
///
/// # Errors
/// A store error; the scan stops there and the next one resumes (notices
/// already recorded stay recorded).
pub async fn scan_key_expiry(store: &KeyStore, now_ms: i64) -> Result<usize, AuthError> {
    let now_secs = now_ms.div_euclid(1000);
    let keys = store
        .lab_expiring_keys(
            now_secs.saturating_sub(EXPIRED_GRACE_SECS),
            now_secs.saturating_add(HORIZON_SECS),
        )
        .await?;
    let mut sent = 0;
    for key in keys {
        let Some(account_id) = lab_account(key.account_ref.as_deref()) else {
            continue;
        };
        let Some(days_left) = expiry_milestone(key.expires_at, now_secs, EXPIRED_GRACE_SECS) else {
            continue;
        };
        let alert = LabAlert::key_expiring(
            Team {
                account_id,
                api_key_id: key.external_ref.clone(),
            },
            &key.id,
            &key.name,
            key.expires_at,
            days_left,
            now_ms,
        );
        let insert = match alert.outbox_insert(now_ms) {
            Ok(insert) => insert,
            Err(error) => {
                // Not expected for these plain structs; the next scan retries.
                tracing::warn!(%error, key_id = %key.id, "could not serialize key.expiring");
                continue;
            }
        };
        if store
            .record_key_expiry_notice(&key.id, key.expires_at, days_left, &insert)
            .await?
        {
            tracing::info!(key_id = %key.id, days_left, "key.expiring reported to the Lab");
            sent += 1;
        }
    }
    Ok(sent)
}

/// Spawn the hourly scan (Lab mode only). The first scan runs at boot.
pub fn spawn_key_expiry_task(store: KeyStore) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SCAN_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(error) = scan_key_expiry(&store, lumen_auth::now_unix_ms()).await {
                tracing::warn!(%error, "key.expiring scan failed; will retry next hour");
            }
        }
    })
}
