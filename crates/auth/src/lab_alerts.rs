//! Lab alert events (ADR 015, amendment 2026-11-20).
//!
//! In Lab mode (`[usage_events]` configured) the gateway reports four alert
//! types through the same signed outbox as `usage.recorded`, so the Lab can
//! email the team: `budget.threshold`, `budget.exhausted`, `key.expiring` and
//! `provider_key.status_changed`. Only subjects that belong to a Lab team are
//! reported: a key or group whose top-level group (the team's lease,
//! `lab-<account id>`) carries a UUID `account_ref`. The lease itself is never
//! a budget subject: the Lab tracks credits.
//!
//! Detection happens where the decision is made, in memory: budget alerts on
//! the settle and the refusal (the same edge-trigger state machine as the
//! webhooks, over its own state and the fixed [`BUDGET_THRESHOLDS`]), and
//! provider-key changes on the key rotation. Nothing here touches the
//! database: a detected alert waits in [`LabAlerts`] and the next budget
//! flush writes it to the outbox in the same transaction as the spend that
//! crossed it, with the per-subject state that makes it once-only across
//! restarts ([`BudgetAlertState`]).

use std::sync::Mutex;

use serde::Serialize;

use crate::billing::{rfc3339_ms, uuid_v5, uuid_v7, PRODUCT};
use crate::events::{EventScope, Subject};
use crate::store::OutboxInsert;

/// The budget shares (percent) reported as `budget.threshold`, ascending.
/// Fixed, unlike the webhook thresholds: the Lab words the email.
pub const BUDGET_THRESHOLDS: [u8; 3] = [50, 80, 95];

/// Most alerts held between two flushes. Alerts are edge-triggered (one per
/// subject and threshold per budget period), so this is only reached when
/// the database has been failing for a long time; past it new alerts are
/// dropped with an error log rather than growing memory without bound.
const PENDING_CAP: usize = 100_000;

/// The `type` of each Lab alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    /// A team budget passed a share of its maximum.
    BudgetThreshold,
    /// A team budget ran out: requests on it are refused.
    BudgetExhausted,
    /// A team key expires soon or has expired.
    KeyExpiring,
    /// A team provider key (BYOK) started failing or works again.
    ProviderKeyStatus,
}

impl AlertKind {
    /// The wire `type`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BudgetThreshold => "budget.threshold",
            Self::BudgetExhausted => "budget.exhausted",
            Self::KeyExpiring => "key.expiring",
            Self::ProviderKeyStatus => "provider_key.status_changed",
        }
    }
}

/// `data` of `budget.threshold` (`threshold` set) and `budget.exhausted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BudgetData {
    /// `key` or `group`.
    pub scope: EventScope,
    /// The LUMEN key or group id.
    pub subject_id: String,
    /// The key or group name; omitted when empty.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub subject_name: String,
    /// The budget, micro-USD.
    pub budget_max_micro_usd: i64,
    /// The spend when the alert was detected, micro-USD.
    pub spent_micro_usd: i64,
    /// The percent passed; `budget.threshold` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<u8>,
}

/// `data` of `key.expiring`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeyExpiringData {
    /// The LUMEN key id.
    pub key_id: String,
    /// The key name; omitted when empty.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub key_name: String,
    /// The expiry, RFC 3339 UTC.
    pub expires_at: String,
    /// 7, 1 or 0 (expired).
    pub days_left: u32,
}

/// A provider key's reported status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKeyStatus {
    /// The provider refuses the key.
    Failing,
    /// The key works again.
    Ok,
}

/// `data` of `provider_key.status_changed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderKeyStatusData {
    /// The provider instance name.
    pub provider: String,
    /// The team key id (`gpk_...`), never its secret.
    pub key_id: String,
    /// The team key's label; omitted when empty.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub key_name: String,
    /// `failing` or `ok`.
    pub status: ProviderKeyStatus,
    /// The provider's HTTP status that made it fail; `failing` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    /// `provider_auth` (401/403) or `provider_quota` (402); `failing` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    /// A secret-free explanation; `failing` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The reason code of a provider key the provider rejected (401/403).
pub const REASON_PROVIDER_AUTH: &str = "provider_auth";
/// The reason code of a provider key out of quota or credit (402).
pub const REASON_PROVIDER_QUOTA: &str = "provider_quota";

/// The `data` of one alert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum AlertData {
    /// `budget.threshold` / `budget.exhausted`.
    Budget(BudgetData),
    /// `key.expiring`.
    KeyExpiring(KeyExpiringData),
    /// `provider_key.status_changed`.
    ProviderKey(ProviderKeyStatusData),
}

/// The budget edge-trigger state of one subject after an alert, persisted
/// with the alert so a restart in the same budget period resumes from what
/// was really emitted (no replay, no loss). `budget_max_micro` identifies the
/// period: a state recorded under another cap is ignored at boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetAlertState {
    /// `key` or `group`.
    pub scope: EventScope,
    /// The key or group id.
    pub subject_id: String,
    /// The cap of the period this state belongs to, micro-USD.
    pub budget_max_micro: i64,
    /// Thresholds of [`BUDGET_THRESHOLDS`] already emitted.
    pub crossed: u32,
    /// Whether `budget.exhausted` was emitted.
    pub exhausted: bool,
}

/// The team an alert belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Team {
    /// The Lab account id (the lease's `account_ref`), a UUID.
    pub account_id: String,
    /// The Lab key id (`external_ref`) of a key subject, if any.
    pub api_key_id: Option<String>,
}

/// One detected alert, waiting for the next flush.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabAlert {
    /// Event id, minted at detection and kept across retries.
    pub id: String,
    /// The event type.
    pub kind: AlertKind,
    /// Detection time, unix ms.
    pub occurred_ms: i64,
    /// The team.
    pub team: Team,
    /// The payload.
    pub data: AlertData,
    /// The budget state to persist with it (budget alerts only).
    pub state: Option<BudgetAlertState>,
}

/// The Lab envelope, as the schema requires it.
#[derive(Serialize)]
struct Envelope<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    occurred_at: String,
    account_id: &'a str,
    api_key_id: Option<&'a str>,
    product: &'static str,
    data: &'a AlertData,
}

impl LabAlert {
    /// A `budget.threshold` alert for `subject` passing `threshold`, with the
    /// subject's state once it is emitted.
    #[must_use]
    pub fn budget_threshold(
        team: Team,
        subject: &Subject<'_>,
        threshold: u8,
        state: BudgetAlertState,
        now_ms: i64,
    ) -> Self {
        Self::budget(
            AlertKind::BudgetThreshold,
            team,
            subject,
            Some(threshold),
            state,
            now_ms,
        )
    }

    /// A `budget.exhausted` alert for `subject`.
    #[must_use]
    pub fn budget_exhausted(
        team: Team,
        subject: &Subject<'_>,
        state: BudgetAlertState,
        now_ms: i64,
    ) -> Self {
        Self::budget(
            AlertKind::BudgetExhausted,
            team,
            subject,
            None,
            state,
            now_ms,
        )
    }

    fn budget(
        kind: AlertKind,
        team: Team,
        subject: &Subject<'_>,
        threshold: Option<u8>,
        state: BudgetAlertState,
        now_ms: i64,
    ) -> Self {
        Self {
            id: uuid_v7(now_ms),
            kind,
            occurred_ms: now_ms,
            team,
            data: AlertData::Budget(BudgetData {
                scope: subject.scope,
                subject_id: subject.id.to_owned(),
                subject_name: subject.name.to_owned(),
                budget_max_micro_usd: subject.max_micro.unwrap_or(0).max(0),
                spent_micro_usd: subject.spent_micro.max(0),
                threshold,
            }),
            state: Some(state),
        }
    }

    /// A `key.expiring` alert. Its id is the UUIDv5 of
    /// `key:{key_id}:{expires_at}:{days_left}` (the schema's rule), so a scan
    /// repeated on another replica reports the same event.
    #[must_use]
    pub fn key_expiring(
        team: Team,
        key_id: &str,
        key_name: &str,
        expires_at_secs: i64,
        days_left: u32,
        now_ms: i64,
    ) -> Self {
        let expires_at = rfc3339_ms(expires_at_secs.saturating_mul(1000));
        Self {
            id: uuid_v5(&format!("key:{key_id}:{expires_at}:{days_left}")),
            kind: AlertKind::KeyExpiring,
            occurred_ms: now_ms,
            team,
            data: AlertData::KeyExpiring(KeyExpiringData {
                key_id: key_id.to_owned(),
                key_name: key_name.to_owned(),
                expires_at,
                days_left,
            }),
            state: None,
        }
    }

    /// A `provider_key.status_changed` alert.
    #[must_use]
    pub fn provider_key(account_id: String, data: ProviderKeyStatusData, now_ms: i64) -> Self {
        Self {
            id: uuid_v7(now_ms),
            kind: AlertKind::ProviderKeyStatus,
            occurred_ms: now_ms,
            team: Team {
                account_id,
                api_key_id: None,
            },
            data: AlertData::ProviderKey(data),
            state: None,
        }
    }

    /// The event in the Lab envelope, serialized.
    ///
    /// # Errors
    /// Serialization failure (not expected for these plain structs).
    pub fn body(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&Envelope {
            id: &self.id,
            kind: self.kind.as_str(),
            occurred_at: rfc3339_ms(self.occurred_ms),
            account_id: &self.team.account_id,
            api_key_id: self.team.api_key_id.as_deref(),
            product: PRODUCT,
            data: &self.data,
        })
    }

    /// The outbox row for this alert, due at `now_ms`.
    ///
    /// # Errors
    /// Serialization failure.
    pub fn outbox_insert(&self, now_ms: i64) -> Result<OutboxInsert, serde_json::Error> {
        Ok(OutboxInsert {
            id: self.id.clone(),
            body: self.body()?,
            created_ms: now_ms,
        })
    }
}

/// Alerts detected since the last flush. Present (in a [`LabAlertCell`])
/// only in Lab mode; pushing is a short critical section on an edge, never
/// per request.
#[derive(Debug, Default)]
pub struct LabAlerts {
    pending: Mutex<Vec<LabAlert>>,
}

/// The live Lab alert sink every key and group entry shares; `None` = not
/// in Lab mode, and no detection work at all.
pub type LabAlertCell = arc_swap::ArcSwapOption<LabAlerts>;

impl LabAlerts {
    /// Queue one alert for the next flush.
    pub fn push(&self, alert: LabAlert) {
        let mut pending = self.lock();
        if pending.len() >= PENDING_CAP {
            tracing::error!(
                event_id = %alert.id,
                kind = alert.kind.as_str(),
                "Lab alert dropped: too many alerts waiting for a budget flush"
            );
            return;
        }
        pending.push(alert);
    }

    /// Take every pending alert, in detection order (the flush).
    #[must_use]
    pub fn take(&self) -> Vec<LabAlert> {
        std::mem::take(&mut *self.lock())
    }

    /// Give back alerts a failed flush took, ahead of any detected since.
    pub fn restore(&self, mut alerts: Vec<LabAlert>) {
        if alerts.is_empty() {
            return;
        }
        let mut pending = self.lock();
        alerts.append(&mut pending);
        *pending = alerts;
    }

    /// Alerts waiting for a flush.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Nothing waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<LabAlert>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The Lab account of a team: `account_ref` when it is a UUID (the Lab's
/// account ids are), else `None` (an operator's own ref is not a Lab team).
#[must_use]
pub fn lab_account(account_ref: Option<&str>) -> Option<String> {
    account_ref
        .filter(|r| crate::billing::is_uuid(r))
        .map(str::to_ascii_lowercase)
}

/// The `key.expiring` milestone of a key expiring at `expires_at_secs`, seen
/// at `now_secs`: 7 within a week, 1 within a day, 0 once expired (up to
/// `grace_secs` after, so a key expired long ago is not announced). `None`
/// outside those windows.
#[must_use]
pub fn expiry_milestone(expires_at_secs: i64, now_secs: i64, grace_secs: i64) -> Option<u32> {
    const DAY: i64 = 86_400;
    let left = expires_at_secs.saturating_sub(now_secs);
    if left <= 0 {
        (left > -grace_secs).then_some(0)
    } else if left <= DAY {
        Some(1)
    } else if left <= 7 * DAY {
        Some(7)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";

    fn validator() -> jsonschema::Validator {
        let schema: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contracts/vendor/lab/lab-events.schema.json"
        )))
        .unwrap();
        jsonschema::options()
            .should_validate_formats(true)
            .build(&schema)
            .unwrap()
    }

    fn team(api_key_id: Option<&str>) -> Team {
        Team {
            account_id: ACCOUNT.to_owned(),
            api_key_id: api_key_id.map(str::to_owned),
        }
    }

    fn subject(scope: EventScope) -> Subject<'static> {
        Subject {
            scope,
            id: "key-1",
            name: "staging",
            max_micro: Some(10_000_000),
            spent_micro: 8_100_000,
        }
    }

    fn state(scope: EventScope) -> BudgetAlertState {
        BudgetAlertState {
            scope,
            subject_id: "key-1".to_owned(),
            budget_max_micro: 10_000_000,
            crossed: 2,
            exhausted: false,
        }
    }

    fn samples() -> Vec<LabAlert> {
        vec![
            LabAlert::budget_threshold(
                team(Some("lab-key-1")),
                &subject(EventScope::Key),
                80,
                state(EventScope::Key),
                1_000,
            ),
            LabAlert::budget_exhausted(
                team(None),
                &subject(EventScope::Group),
                state(EventScope::Group),
                1_000,
            ),
            LabAlert::key_expiring(
                team(Some("lab-key-1")),
                "key-1",
                "ci",
                1_800_000_000,
                7,
                1_000,
            ),
            LabAlert::key_expiring(team(None), "key-1", "", 1_800_000_000, 0, 1_000),
            LabAlert::provider_key(
                ACCOUNT.to_owned(),
                ProviderKeyStatusData {
                    provider: "openai".to_owned(),
                    key_id: "gpk_1".to_owned(),
                    key_name: "prod".to_owned(),
                    status: ProviderKeyStatus::Failing,
                    status_code: Some(401),
                    reason: Some(REASON_PROVIDER_AUTH),
                    message: Some("the provider rejected this key (HTTP 401)".to_owned()),
                },
                1_000,
            ),
            LabAlert::provider_key(
                ACCOUNT.to_owned(),
                ProviderKeyStatusData {
                    provider: "openai".to_owned(),
                    key_id: "gpk_1".to_owned(),
                    key_name: String::new(),
                    status: ProviderKeyStatus::Ok,
                    status_code: None,
                    reason: None,
                    message: None,
                },
                1_000,
            ),
        ]
    }

    #[test]
    fn every_alert_validates_against_the_vendored_contract() {
        let validator = validator();
        for alert in samples() {
            let v: serde_json::Value = serde_json::from_str(&alert.body().unwrap()).unwrap();
            let errors: Vec<String> = validator.iter_errors(&v).map(|e| e.to_string()).collect();
            assert!(errors.is_empty(), "{}: {errors:?}", alert.kind.as_str());
            assert_eq!(v["product"], "lumen");
            assert_eq!(v["type"], alert.kind.as_str());
        }
    }

    #[test]
    fn the_contract_rejects_what_the_schema_forbids() {
        let validator = validator();
        let alerts = samples();
        let threshold: serde_json::Value =
            serde_json::from_str(&alerts[0].body().unwrap()).unwrap();
        assert_eq!(threshold["data"]["threshold"], 80);
        assert_eq!(threshold["data"]["budget_max_micro_usd"], 10_000_000);
        assert_eq!(threshold["data"]["spent_micro_usd"], 8_100_000);
        assert_eq!(threshold["api_key_id"], "lab-key-1");

        let mut missing = threshold.clone();
        missing["data"].as_object_mut().unwrap().remove("threshold");
        assert!(validator.iter_errors(&missing).next().is_some());
        let mut float = threshold.clone();
        float["data"]["spent_micro_usd"] = serde_json::json!(8.1);
        assert!(validator.iter_errors(&float).next().is_some());
        let mut extra = threshold;
        extra["data"]["key_id"] = serde_json::json!("k");
        assert!(validator.iter_errors(&extra).next().is_some());

        let mut bad_reason: serde_json::Value =
            serde_json::from_str(&alerts[4].body().unwrap()).unwrap();
        bad_reason["data"]["reason"] = serde_json::json!("Provider Auth");
        assert!(validator.iter_errors(&bad_reason).next().is_some());
    }

    #[test]
    fn key_expiring_ids_are_stable_per_key_expiry_and_milestone() {
        let a = LabAlert::key_expiring(team(None), "k", "", 1_800_000_000, 7, 1);
        let b = LabAlert::key_expiring(team(None), "k", "", 1_800_000_000, 7, 2);
        let c = LabAlert::key_expiring(team(None), "k", "", 1_800_000_000, 1, 1);
        let d = LabAlert::key_expiring(team(None), "k", "", 1_800_000_060, 7, 1);
        assert_eq!(a.id, b.id);
        assert_ne!(a.id, c.id);
        assert_ne!(a.id, d.id);
        assert_eq!(
            a.id,
            uuid_v5("key:k:2027-01-15T08:00:00.000Z:7"),
            "UUIDv5 of key:{{key_id}}:{{expires_at}}:{{days_left}}"
        );
    }

    #[test]
    fn milestones_follow_the_time_left() {
        let day = 86_400;
        let now = 1_000_000_000;
        assert_eq!(expiry_milestone(now + 8 * day, now, day), None);
        assert_eq!(expiry_milestone(now + 7 * day, now, day), Some(7));
        assert_eq!(expiry_milestone(now + day + 1, now, day), Some(7));
        assert_eq!(expiry_milestone(now + day, now, day), Some(1));
        assert_eq!(expiry_milestone(now + 1, now, day), Some(1));
        assert_eq!(expiry_milestone(now, now, day), Some(0));
        assert_eq!(expiry_milestone(now - day + 1, now, day), Some(0));
        assert_eq!(expiry_milestone(now - day, now, day), None);
    }

    #[test]
    fn only_uuid_account_refs_are_lab_teams() {
        assert_eq!(
            lab_account(Some("0192F3C1-7C2E-7B1A-9F00-3C9D2E4A5B61")).as_deref(),
            Some(ACCOUNT)
        );
        assert_eq!(lab_account(Some("team-a")), None);
        assert_eq!(lab_account(None), None);
    }

    #[test]
    fn a_failed_flush_gives_alerts_back_in_order() {
        let alerts = LabAlerts::default();
        let mut all = samples();
        let later = all.split_off(3);
        for alert in all.clone() {
            alerts.push(alert);
        }
        let taken = alerts.take();
        assert!(alerts.is_empty());
        for alert in later.clone() {
            alerts.push(alert);
        }
        alerts.restore(taken);
        let back = alerts.take();
        all.extend(later);
        assert_eq!(back, all);
    }
}
