//! ADR 015: in-memory billing watermark, unit counters and the flush batch.

use lumen_auth::billing::BillingPolicy;
use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::{GroupRecord, VirtualKeyRecord};
use std::sync::Arc;
use std::time::Duration;

const ACCOUNT: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";
const NOW: i64 = 1_000;

fn group(account_ref: Option<&str>) -> GroupRecord {
    GroupRecord {
        id: "g".to_owned(),
        name: "lease".to_owned(),
        budget_max: Some(100.0),
        budget_spent: 0.0,
        created_at: 0,
        deleted_at: None,
        account_ref: account_ref.map(str::to_owned),
    }
}

fn key(group_id: Option<&str>, billed_micro: i64) -> VirtualKeyRecord {
    VirtualKeyRecord {
        id: "k".to_owned(),
        name: "k".to_owned(),
        group_id: group_id.map(str::to_owned),
        budget_max: None,
        budget_spent: 0.0,
        rpm_limit: None,
        tpm_limit: None,
        expires_at: None,
        disabled: false,
        created_at: 0,
        deleted_at: None,
        external_ref: Some("lab-key".to_owned()),
        billed_micro,
    }
}

fn state(account_ref: Option<&str>, group_id: Option<&str>, billing: bool) -> AuthState {
    let state = AuthState::load(
        vec![group(account_ref)],
        vec![(hash_key("sk"), key(group_id, 0))],
    );
    if billing {
        state.set_billing(Some(Arc::new(BillingPolicy {
            source: "eu-1".to_owned(),
        })));
    }
    state
}

/// Admit and settle one request of `cost` micro-USD and `tokens` tokens.
fn spend(state: &AuthState, cost: i64, tokens: i64) {
    let entry = state.authenticate("sk", NOW).unwrap();
    entry.admit(NOW, tokens, cost).unwrap().settle(cost, tokens);
}

#[test]
fn a_billable_key_emits_its_positive_delta_with_units() {
    let s = state(Some(ACCOUNT), Some("g"), true);
    spend(&s, 1_500, 40);
    spend(&s, 500, 2);
    let batch = s.drain_flush(2_000);
    let deltas = batch.deltas();
    assert_eq!(deltas.len(), 1);
    let d = deltas[0];
    assert_eq!(d.cost_micro, 2_000);
    assert_eq!((d.units.requests, d.units.tokens_in), (2, 42));
    assert_eq!(d.account_ref, ACCOUNT);
    assert_eq!(d.external_ref.as_deref(), Some("lab-key"));
    assert_eq!(d.window_end_ms, 2_000);
    assert_eq!(batch.rows()[0].billed_micro, 2_000);
    s.commit_flush(batch);

    // Only new spend is billed next time, and the window starts at the last flush.
    spend(&s, 300, 1);
    let next = s.drain_flush(3_000);
    assert_eq!(next.deltas()[0].cost_micro, 300);
    assert_eq!(next.deltas()[0].window_start_ms, 2_000);
}

#[test]
fn reservations_are_never_billed_only_settled_cost() {
    let s = state(Some(ACCOUNT), Some("g"), true);
    let entry = s.authenticate("sk", NOW).unwrap();
    // Reserve 1000 and flush while in flight: the estimate is not billed.
    let reservation = entry.admit(NOW, 0, 1_000).unwrap();
    let first = s.drain_flush(2_000);
    assert_eq!(first.deltas(), [] as [&lumen_auth::billing::UsageDelta; 0]);
    assert_eq!(first.rows()[0].billed_micro, 0);
    s.commit_flush(first);
    // The call fails: the reservation is refunded, nothing was ever billed.
    drop(reservation);
    let after_refund = s.drain_flush(3_000);
    assert_eq!(
        after_refund.deltas(),
        [] as [&lumen_auth::billing::UsageDelta; 0]
    );
    s.commit_flush(after_refund);
    // Later settled spend is billed in full.
    spend(&s, 1_200, 0);
    let billed = s.drain_flush(4_000);
    assert_eq!(billed.deltas()[0].cost_micro, 1_200);
}

#[test]
fn non_billable_keys_keep_the_watermark_caught_up() {
    for (account, group, billing) in [
        (Some(ACCOUNT), Some("g"), false), // billing off
        (None, Some("g"), true),           // group without account_ref
        (Some(ACCOUNT), None, true),       // key without group
    ] {
        let s = state(account, group, billing);
        spend(&s, 700, 5);
        let batch = s.drain_flush(2_000);
        assert_eq!(batch.deltas(), [] as [&lumen_auth::billing::UsageDelta; 0]);
        assert_eq!(batch.rows()[0].billed_micro, 700);
        assert_eq!(
            batch.outbox_inserts(2_000).unwrap(),
            [] as [lumen_auth::store::OutboxInsert; 0]
        );
    }
}

#[test]
fn joining_a_billable_group_does_not_bill_the_past() {
    let s = state(Some(ACCOUNT), None, true);
    spend(&s, 5_000, 0);
    let caught_up = s.drain_flush(2_000);
    s.commit_flush(caught_up); // operator key: watermark catches up
    s.apply(&key(Some("g"), 0)); // joins the Lab-linked group
    spend(&s, 100, 0);
    let batch = s.drain_flush(3_000);
    assert_eq!(batch.deltas()[0].cost_micro, 100);
}

#[test]
fn rollback_redirties_and_restores_counters() {
    let s = state(Some(ACCOUNT), Some("g"), true);
    spend(&s, 900, 9);
    let failed = s.drain_flush(2_000);
    s.rollback_flush(failed);
    // No new spend, yet the key is flushed again with the same money and units.
    let retry = s.drain_flush(3_000);
    let d = retry.deltas()[0];
    assert_eq!(
        (d.cost_micro, d.units.requests, d.units.tokens_in),
        (900, 1, 9)
    );
}

#[test]
fn a_retired_key_is_billed_and_retried_after_a_failed_flush() {
    let s = state(Some(ACCOUNT), Some("g"), true);
    spend(&s, 400, 4);
    let entry = s.remove("k").unwrap();
    s.retire(entry);
    let failed = s.drain_flush(2_000);
    assert_eq!(failed.deltas()[0].cost_micro, 400);
    s.rollback_flush(failed);
    // The failed flush lost nothing: the same money is offered again.
    let retry = s.drain_flush(3_000);
    assert_eq!(retry.deltas()[0].cost_micro, 400);
    s.commit_flush(retry);
    assert!(s.drain_flush(4_000).is_empty(), "committed: nothing left");
}

#[test]
fn a_retired_key_with_a_request_in_flight_stays_until_it_settles() {
    let s = state(Some(ACCOUNT), Some("g"), true);
    let entry = s.authenticate("sk", NOW).unwrap();
    let reservation = entry.admit(NOW, 0, 100).unwrap();
    drop(entry);
    let evicted = s.remove("k").unwrap();
    s.retire(evicted);
    // Committing while the request is in flight must not forget the key.
    let first = s.drain_flush(2_000);
    assert_eq!(first.deltas(), [] as [&lumen_auth::billing::UsageDelta; 0]);
    s.commit_flush(first);
    let still_retired = s.drain_flush(3_000);
    assert!(
        !still_retired.is_empty(),
        "in-flight request keeps it retired"
    );
    s.commit_flush(still_retired);
    // The request settles after the delete: its cost is billed, then the key goes.
    reservation.settle(100, 1);
    let settled = s.drain_flush(4_000);
    assert_eq!(settled.deltas()[0].cost_micro, 100);
    s.commit_flush(settled);
    assert!(
        s.drain_flush(5_000).is_empty(),
        "settled and billed: forgotten"
    );
}

#[tokio::test]
async fn a_flush_guard_serializes_flushers() {
    let s = state(Some(ACCOUNT), Some("g"), true);
    let first = s.flush_guard().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), s.flush_guard())
            .await
            .is_err(),
        "a second flusher must wait while the first holds the guard"
    );
    drop(first);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), s.flush_guard())
            .await
            .is_ok(),
        "the guard is free again once released"
    );
}

#[test]
fn outbox_inserts_carry_valid_events() {
    let s = state(Some(ACCOUNT), Some("g"), true);
    spend(&s, 42, 1);
    let batch = s.drain_flush(2_000);
    let inserts = batch.outbox_inserts(2_000).unwrap();
    assert_eq!(inserts.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&inserts[0].body).unwrap();
    assert_eq!(body["id"], inserts[0].id.as_str());
    assert_eq!(body["data"]["provider_cost_micro_usd"], 42);
    assert_eq!(inserts[0].created_ms, 2_000);
}

/// Admit and settle one request with a full usage breakdown.
fn spend_usage(state: &AuthState, usage: lumen_auth::state::SettledUsage) {
    let entry = state.authenticate("sk", NOW).unwrap();
    entry
        .admit(NOW, usage.tokens_in + usage.tokens_out, usage.cost_micro)
        .unwrap()
        .settle_usage(usage);
}

#[test]
fn units_split_tokens_and_count_estimates() {
    use lumen_auth::state::SettledUsage;
    let s = state(Some(ACCOUNT), Some("g"), true);
    // Upstream usage: exact counts, nothing estimated.
    spend_usage(
        &s,
        SettledUsage {
            cost_micro: 1_000,
            tokens_in: 30,
            tokens_out: 10,
            estimated: false,
        },
    );
    // No upstream usage: the local estimate is flagged (ADR 003).
    spend_usage(
        &s,
        SettledUsage {
            cost_micro: 500,
            tokens_in: 7,
            tokens_out: 3,
            estimated: true,
        },
    );
    let batch = s.drain_flush(2_000);
    let d = batch.deltas()[0];
    assert_eq!(d.cost_micro, 1_500);
    assert_eq!(
        d.units,
        lumen_auth::billing::UsageUnits {
            requests: 2,
            tokens_in: 37,
            tokens_out: 13,
            tokens_estimated: 10,
        }
    );
}
