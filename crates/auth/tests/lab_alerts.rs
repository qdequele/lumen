//! ADR 015 amendment 2026-11-20: Lab budget alerts, detected on the settle and
//! the refusal, carried by the flush batch and resumed after a restart.

use lumen_auth::billing::BillingPolicy;
use lumen_auth::events::EventScope;
use lumen_auth::key::hash_key;
use lumen_auth::lab_alerts::{AlertData, AlertKind, BudgetAlertState, LabAlert, LabAlerts};
use lumen_auth::state::{usd_to_micro, AuthState};
use lumen_auth::store::{GroupRecord, KeyStore, VirtualKeyRecord};
use lumen_core::{BudgetScope, GatewayError};
use std::sync::Arc;

const ACCOUNT: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";
const NOW: i64 = 1_000;

fn group(id: &str, budget_max: Option<f64>, parent: Option<&str>) -> GroupRecord {
    GroupRecord {
        id: id.to_owned(),
        name: if parent.is_none() {
            format!("lab-{ACCOUNT}")
        } else {
            "staging".to_owned()
        },
        budget_max,
        budget_spent: 0.0,
        created_at: 0,
        deleted_at: None,
        account_ref: Some(ACCOUNT.to_owned()),
        bandwidth_micro_usd_per_gb: 0,
        parent_id: parent.map(str::to_owned),
    }
}

fn key(group_id: &str, budget_max: Option<f64>, budget_spent: f64) -> VirtualKeyRecord {
    VirtualKeyRecord {
        id: "k".to_owned(),
        name: "ci".to_owned(),
        group_id: Some(group_id.to_owned()),
        budget_max,
        budget_spent,
        rpm_limit: None,
        tpm_limit: None,
        expires_at: None,
        disabled: false,
        created_at: 0,
        deleted_at: None,
        external_ref: Some("lab-key".to_owned()),
        billed_micro: 0,
    }
}

/// Lab mode: billing on and the alert sink installed, as at boot.
fn lab_mode(state: &AuthState, stored: &[BudgetAlertState]) {
    state.set_billing(Some(Arc::new(BillingPolicy {
        source: "eu-1".to_owned(),
    })));
    state.set_lab_alerts(Some(Arc::new(LabAlerts::default())), stored);
}

fn spend(state: &AuthState, usd: f64) {
    let cost = usd_to_micro(usd);
    let entry = state.authenticate("sk", NOW).expect("key");
    entry.admit(NOW, 1, cost).expect("fits").settle(cost, 1);
}

fn taken(state: &AuthState) -> Vec<LabAlert> {
    let batch = state.drain_flush(2_000);
    let alerts = batch.alerts().to_vec();
    state.commit_flush(batch);
    alerts
}

fn summary(alerts: &[LabAlert]) -> Vec<(&'static str, EventScope, Option<u8>)> {
    alerts
        .iter()
        .map(|a| match &a.data {
            AlertData::Budget(b) => (a.kind.as_str(), b.scope, b.threshold),
            other => panic!("not a budget alert: {other:?}"),
        })
        .collect()
}

#[test]
fn a_team_key_budget_reports_each_threshold_once() {
    let state = AuthState::load(
        vec![group("lease", Some(1_000.0), None)],
        vec![(hash_key("sk"), key("lease", Some(10.0), 0.0))],
    );
    lab_mode(&state, &[]);

    spend(&state, 6.0);
    spend(&state, 0.5);
    let alerts = taken(&state);
    assert_eq!(
        summary(&alerts),
        [("budget.threshold", EventScope::Key, Some(50))]
    );
    let alert = &alerts[0];
    assert_eq!(alert.team.account_id, ACCOUNT);
    assert_eq!(alert.team.api_key_id.as_deref(), Some("lab-key"));
    let body: serde_json::Value = serde_json::from_str(&alert.body().unwrap()).unwrap();
    assert_eq!(body["data"]["budget_max_micro_usd"], 10_000_000);
    assert_eq!(body["data"]["spent_micro_usd"], 6_000_000);
    assert_eq!(body["data"]["subject_id"], "k");
    assert_eq!(body["data"]["subject_name"], "ci");
    assert_eq!(
        alert.state,
        Some(BudgetAlertState {
            scope: EventScope::Key,
            subject_id: "k".to_owned(),
            budget_max_micro: 10_000_000,
            crossed: 1,
            exhausted: false,
        })
    );

    // A jump past 80 and 95 reports both, in order, then nothing more.
    spend(&state, 3.2);
    spend(&state, 0.1);
    assert_eq!(
        summary(&taken(&state)),
        [
            ("budget.threshold", EventScope::Key, Some(80)),
            ("budget.threshold", EventScope::Key, Some(95)),
        ]
    );
}

#[test]
fn an_exhausted_team_key_is_reported_once_until_a_grant() {
    let state = AuthState::load(
        vec![group("lease", Some(1_000.0), None)],
        vec![(hash_key("sk"), key("lease", Some(1.0), 0.0))],
    );
    lab_mode(&state, &[]);
    spend(&state, 1.0);
    let _ = taken(&state);

    let entry = state.authenticate("sk", NOW).expect("key");
    for _ in 0..3 {
        assert!(matches!(
            entry.admit(NOW, 1, 10),
            Err(GatewayError::BudgetExceeded {
                scope: BudgetScope::Key
            })
        ));
    }
    let alerts = taken(&state);
    assert_eq!(
        summary(&alerts),
        [("budget.exhausted", EventScope::Key, None)]
    );
    assert!(alerts[0].state.as_ref().is_some_and(|s| s.exhausted));

    // A grant buys headroom (50 % of the new cap): the thresholds above it
    // and the next exhaustion belong to a new period.
    assert!(state.grant_key("k", usd_to_micro(1.0)));
    spend(&state, 1.0);
    assert!(entry.admit(NOW, 1, 10).is_err());
    let kinds: Vec<_> = taken(&state).iter().map(|a| a.kind).collect();
    assert_eq!(
        kinds,
        [
            AlertKind::BudgetThreshold,
            AlertKind::BudgetThreshold,
            AlertKind::BudgetExhausted,
        ]
    );
}

#[test]
fn the_team_lease_is_never_a_budget_subject() {
    // The key has no budget of its own; the lease runs out.
    let state = AuthState::load(
        vec![group("lease", Some(1.0), None)],
        vec![(hash_key("sk"), key("lease", None, 0.0))],
    );
    lab_mode(&state, &[]);
    spend(&state, 0.99);
    let entry = state.authenticate("sk", NOW).expect("key");
    assert!(matches!(
        entry.admit(NOW, 1, usd_to_micro(0.5)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Group
        })
    ));
    assert_eq!(taken(&state), [], "the Lab tracks credits itself");
}

#[test]
fn a_sub_group_budget_is_reported_but_not_its_parent_lease() {
    let state = AuthState::load(
        vec![
            group("lease", Some(2.0), None),
            group("sub", Some(1.0), Some("lease")),
        ],
        vec![(hash_key("sk"), key("sub", None, 0.0))],
    );
    lab_mode(&state, &[]);

    // 0.9 of the sub-group (90 %) is 45 % of the lease: only the sub-group.
    spend(&state, 0.9);
    let alerts = taken(&state);
    assert_eq!(
        summary(&alerts),
        [
            ("budget.threshold", EventScope::Group, Some(50)),
            ("budget.threshold", EventScope::Group, Some(80)),
        ]
    );
    assert_eq!(alerts[0].team.account_id, ACCOUNT);
    assert_eq!(alerts[0].team.api_key_id, None);

    let entry = state.authenticate("sk", NOW).expect("key");
    assert!(matches!(
        entry.admit(NOW, 1, usd_to_micro(0.5)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Group
        })
    ));
    assert_eq!(
        summary(&taken(&state)),
        [("budget.exhausted", EventScope::Group, None)]
    );

    // Past the lease (the sub-group has room again): a parent refusal is the
    // team's credits, never reported.
    state.apply_group(&group("sub", Some(100.0), Some("lease")));
    spend(&state, 1.05);
    let _ = taken(&state);
    assert!(matches!(
        entry.admit(NOW, 1, usd_to_micro(0.5)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::ParentGroup
        })
    ));
    assert_eq!(taken(&state), []);
}

#[test]
fn nothing_is_reported_outside_lab_mode_or_for_a_non_lab_account() {
    let state = AuthState::load(
        vec![group("lease", Some(1_000.0), None)],
        vec![(hash_key("sk"), key("lease", Some(1.0), 0.0))],
    );
    spend(&state, 0.9);
    assert_eq!(taken(&state), [], "no sink installed");

    let mut operator = group("lease", Some(1_000.0), None);
    operator.account_ref = Some("team-a".to_owned());
    let state = AuthState::load(
        vec![operator],
        vec![(hash_key("sk"), key("lease", Some(1.0), 0.0))],
    );
    lab_mode(&state, &[]);
    spend(&state, 0.9);
    assert_eq!(taken(&state), [], "not a Lab account id");
}

#[test]
fn a_failed_flush_keeps_the_alerts_for_the_next_one() {
    let state = AuthState::load(
        vec![group("lease", Some(1_000.0), None)],
        vec![(hash_key("sk"), key("lease", Some(10.0), 0.0))],
    );
    lab_mode(&state, &[]);
    spend(&state, 5.0);
    let failed = state.drain_flush(2_000);
    assert_eq!(failed.alerts().len(), 1);
    let inserts = failed.outbox_inserts(2_000).unwrap();
    assert!(
        inserts.iter().any(|i| i.id == failed.alerts()[0].id),
        "the alert rides in the flush's outbox rows"
    );
    state.rollback_flush(failed);
    let retry = state.drain_flush(3_000);
    assert_eq!(
        summary(retry.alerts()),
        [("budget.threshold", EventScope::Key, Some(50))]
    );
    state.commit_flush(retry);
    assert_eq!(state.drain_flush(4_000).alerts(), []);
}

#[test]
fn a_key_loaded_past_a_threshold_does_not_replay_it() {
    // No stored state (first boot with the feature): armed to current spend.
    let state = AuthState::load(
        vec![group("lease", Some(1_000.0), None)],
        vec![(hash_key("sk"), key("lease", Some(10.0), 8.5))],
    );
    lab_mode(&state, &[]);
    spend(&state, 0.1);
    assert_eq!(taken(&state), []);
    spend(&state, 1.0);
    assert_eq!(
        summary(&taken(&state)),
        [("budget.threshold", EventScope::Key, Some(95))]
    );
}

#[tokio::test]
async fn persisted_states_resume_the_budget_period_after_a_restart() {
    let store = KeyStore::in_memory().await.unwrap();
    let state = AuthState::load(
        vec![group("lease", Some(1_000.0), None)],
        vec![(hash_key("sk"), key("lease", Some(10.0), 0.0))],
    );
    lab_mode(&state, &[]);
    spend(&state, 5.5);
    let batch = state.drain_flush(2_000);
    let events = batch.outbox_inserts(2_000).unwrap();
    store
        .persist_flush_with_alerts(&batch.rows(), &events, &batch.alert_states(), 2_000)
        .await
        .unwrap();
    state.commit_flush(batch);
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM usage_outbox WHERE body LIKE '%\"type\":\"budget.threshold\"%'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(pending, 1, "the alert is in the outbox with the spend");

    let stored = store.load_lab_budget_states().await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].crossed, 1);

    // Restart with spend past 80 % (flushed, but its alert was not): the
    // restored state still owes 80, and only 80.
    let restarted = AuthState::load(
        vec![group("lease", Some(1_000.0), None)],
        vec![(hash_key("sk"), key("lease", Some(10.0), 8.2))],
    );
    lab_mode(&restarted, &stored);
    spend(&restarted, 0.1);
    assert_eq!(
        summary(&taken(&restarted)),
        [("budget.threshold", EventScope::Key, Some(80))]
    );

    // A state recorded under another cap is another period: armed to spend.
    let new_cap = AuthState::load(
        vec![group("lease", Some(1_000.0), None)],
        vec![(hash_key("sk"), key("lease", Some(20.0), 8.2))],
    );
    lab_mode(&new_cap, &stored);
    spend(&new_cap, 0.1);
    assert_eq!(taken(&new_cap), [], "41 %: nothing owed");
}

#[tokio::test]
async fn a_persisted_exhaustion_is_not_reported_again_after_a_restart() {
    let store = KeyStore::in_memory().await.unwrap();
    let state = AuthState::load(
        vec![group("lease", Some(1_000.0), None)],
        vec![(hash_key("sk"), key("lease", Some(1.0), 1.0))],
    );
    lab_mode(&state, &[]);
    let entry = state.authenticate("sk", NOW).expect("key");
    assert!(entry.admit(NOW, 1, 10).is_err());
    let batch = state.drain_flush(2_000);
    assert_eq!(batch.alerts().len(), 1);
    store
        .persist_flush_with_alerts(
            &batch.rows(),
            &batch.outbox_inserts(2_000).unwrap(),
            &batch.alert_states(),
            2_000,
        )
        .await
        .unwrap();
    state.commit_flush(batch);

    let restarted = AuthState::load(
        vec![group("lease", Some(1_000.0), None)],
        vec![(hash_key("sk"), key("lease", Some(1.0), 1.0))],
    );
    lab_mode(&restarted, &store.load_lab_budget_states().await.unwrap());
    let entry = restarted.authenticate("sk", NOW).expect("key");
    assert!(entry.admit(NOW, 1, 10).is_err());
    assert_eq!(taken(&restarted), []);
}
