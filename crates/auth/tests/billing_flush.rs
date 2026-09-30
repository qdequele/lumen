//! ADR 015: in-memory billing watermark, unit counters and the flush batch.

use lumen_auth::billing::BillingPolicy;
use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::{GroupRecord, VirtualKeyRecord};
use std::sync::Arc;

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
    assert_eq!((d.requests, d.tokens), (2, 42));
    assert_eq!(d.account_ref, ACCOUNT);
    assert_eq!(d.external_ref.as_deref(), Some("lab-key"));
    assert_eq!(d.window_end_ms, 2_000);
    assert_eq!(d.group_id, "g");
    assert_eq!(d.group_spent_micro, 2_000);
    assert_eq!(d.group_budget_max_micro, Some(100_000_000));
    assert_eq!(batch.rows()[0].billed_micro, 2_000);
    batch.commit();

    // Only new spend is billed next time, and the window starts at the last flush.
    spend(&s, 300, 1);
    let next = s.drain_flush(3_000);
    assert_eq!(next.deltas()[0].cost_micro, 300);
    assert_eq!(next.deltas()[0].window_start_ms, 2_000);
}

#[test]
fn a_refund_below_the_watermark_is_held_back_then_netted() {
    let s = state(Some(ACCOUNT), Some("g"), true);
    let entry = s.authenticate("sk", NOW).unwrap();
    // Reserve 1000 and flush while in flight: the reservation is billed.
    let reservation = entry.admit(NOW, 0, 1_000).unwrap();
    let first = s.drain_flush(2_000);
    assert_eq!(first.deltas()[0].cost_micro, 1_000);
    first.commit();
    // The call fails: the reservation is refunded, spend drops to 0.
    drop(reservation);
    let held = s.drain_flush(3_000);
    assert!(held.deltas().is_empty());
    assert_eq!(held.rows()[0].billed_micro, 1_000, "watermark holds");
    held.commit();
    // Later spend is measured from the unchanged watermark.
    spend(&s, 1_200, 0);
    let netted = s.drain_flush(4_000);
    assert_eq!(netted.deltas()[0].cost_micro, 200);
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
        assert!(batch.deltas().is_empty());
        assert_eq!(batch.rows()[0].billed_micro, 700);
        assert!(batch.outbox_inserts("eu-1", 2_000).unwrap().is_empty());
    }
}

#[test]
fn joining_a_billable_group_does_not_bill_the_past() {
    let s = state(Some(ACCOUNT), None, true);
    spend(&s, 5_000, 0);
    s.drain_flush(2_000).commit(); // operator key: watermark catches up
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
    failed.rollback();
    // No new spend, yet the key is flushed again with the same money and units.
    let retry = s.drain_flush(3_000);
    let d = retry.deltas()[0];
    assert_eq!((d.cost_micro, d.requests, d.tokens), (900, 1, 9));
}

#[test]
fn an_evicted_key_is_flushed_and_billed() {
    let s = state(Some(ACCOUNT), Some("g"), true);
    spend(&s, 400, 4);
    let entry = s.remove("k").unwrap();
    assert!(
        s.drain_flush(2_000).is_empty(),
        "evicted keys leave the table"
    );
    let batch = s.flush_evicted(&entry, 2_000);
    assert_eq!(batch.deltas()[0].cost_micro, 400);
}

#[test]
fn outbox_inserts_carry_valid_events() {
    let s = state(Some(ACCOUNT), Some("g"), true);
    spend(&s, 42, 1);
    let batch = s.drain_flush(2_000);
    let inserts = batch.outbox_inserts("eu-1", 2_000).unwrap();
    assert_eq!(inserts.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&inserts[0].body).unwrap();
    assert_eq!(body["id"], inserts[0].id.as_str());
    assert_eq!(body["data"]["cost_micro_usd"], 42);
    assert_eq!(inserts[0].created_ms, 2_000);
}
