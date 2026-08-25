//! Budget-event detection through the real admission/settle path (ADR 011).
//!
//! Memory-only, like `enforce.rs`: detection rides the atomic settle that
//! already happens per request, so no database and no HTTP receiver is
//! involved here. Delivery is tested end-to-end in
//! `crates/server/tests/webhooks.rs`.

// Micro-USD values converted to f64 and compared back: exact at these
// magnitudes, so strict equality is the correct assertion.
#![allow(clippy::float_cmp)]

use lumen_auth::events::{
    BudgetEvent, BudgetSignals, EventKind, EventScope, NoopCounters, SignalQueue,
};
use lumen_auth::key::hash_key;
use lumen_auth::state::{usd_to_micro, AuthState};
use lumen_auth::store::{GroupRecord, VirtualKeyRecord};
use std::sync::Arc;
use tokio::sync::mpsc::Receiver;

const NOW: i64 = 1_800_000_000;

fn key_record(id: &str, budget_max: Option<f64>) -> VirtualKeyRecord {
    VirtualKeyRecord {
        id: id.to_owned(),
        name: format!("{id}-name"),
        group_id: None,
        budget_max,
        budget_spent: 0.0,
        rpm_limit: None,
        tpm_limit: None,
        expires_at: None,
        disabled: false,
        created_at: 0,
        deleted_at: None,
    }
}

fn group_record(id: &str, budget_max: Option<f64>) -> GroupRecord {
    GroupRecord {
        id: id.to_owned(),
        name: format!("{id}-name"),
        budget_max,
        budget_spent: 0.0,
        created_at: 0,
        deleted_at: None,
    }
}

/// Install a signalling policy with the given thresholds on `state` and hand
/// back the receiver a webhook sender task would drain.
fn arm(state: &AuthState, thresholds: &[u8]) -> Receiver<BudgetEvent> {
    arm_with(state, thresholds, &EventKind::ALL)
}

fn arm_with(state: &AuthState, thresholds: &[u8], events: &[EventKind]) -> Receiver<BudgetEvent> {
    let (queue, rx) = SignalQueue::new(64, Arc::new(NoopCounters));
    state.set_signals(Some(Arc::new(BudgetSignals::new(
        queue, events, thresholds,
    ))));
    rx
}

fn drain(rx: &mut Receiver<BudgetEvent>) -> Vec<BudgetEvent> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

/// One request at `cost` USD, admitted and settled at exactly that cost.
fn spend(entry: &Arc<lumen_auth::state::KeyEntry>, cost: f64) {
    let reservation = entry
        .admit(NOW, 0, usd_to_micro(cost))
        .expect("admission should succeed");
    reservation.settle(usd_to_micro(cost), 0);
}

#[tokio::test]
async fn a_settle_that_crosses_a_threshold_fires_exactly_one_event() {
    let state = AuthState::load(
        Vec::new(),
        vec![(hash_key("fg-t"), key_record("k1", Some(100.0)))],
    );
    let mut rx = arm(&state, &[50, 80, 95]);
    let entry = state.authenticate("fg-t", NOW).expect("key valid");

    // $40: under every threshold.
    spend(&entry, 40.0);
    assert!(drain(&mut rx).is_empty(), "40% must not signal");

    // $51 total: crosses 50 %.
    spend(&entry, 11.0);
    let fired = drain(&mut rx);
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].event, EventKind::BudgetThreshold);
    assert_eq!(fired[0].threshold, Some(50));
    assert_eq!(fired[0].scope, EventScope::Key);
    assert_eq!(fired[0].subject_id, "k1");
    assert_eq!(fired[0].subject_name, "k1-name");
    assert_eq!(fired[0].budget_max, Some(100.0));
    assert_eq!(fired[0].budget_spent, 51.0);

    // Every further request under 80 % is silent - edge-triggered, not
    // re-fired per request.
    for _ in 0..5 {
        spend(&entry, 1.0);
    }
    assert!(drain(&mut rx).is_empty());
}

#[tokio::test]
async fn a_refused_request_fires_budget_exhausted_once() {
    let state = AuthState::load(
        Vec::new(),
        vec![(hash_key("fg-x"), key_record("k1", Some(10.0)))],
    );
    let mut rx = arm_with(&state, &[], &[EventKind::BudgetExhausted]);
    let entry = state.authenticate("fg-x", NOW).expect("key valid");

    spend(&entry, 10.0);
    assert!(
        drain(&mut rx).is_empty(),
        "spending the budget is not a refusal"
    );

    // Three refusals, one event.
    for _ in 0..3 {
        assert!(entry.admit(NOW, 0, usd_to_micro(1.0)).is_err());
    }
    let fired = drain(&mut rx);
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].event, EventKind::BudgetExhausted);
    assert_eq!(fired[0].budget_spent, 10.0);
    assert_eq!(fired[0].threshold, None);

    // A grant re-arms it: the next exhaustion is a new event.
    assert!(state.grant_key("k1", usd_to_micro(5.0)));
    spend(&entry, 5.0);
    assert!(entry.admit(NOW, 0, usd_to_micro(1.0)).is_err());
    let fired = drain(&mut rx);
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].event, EventKind::BudgetExhausted);
}

#[tokio::test]
async fn a_group_pool_signals_in_its_own_scope() {
    let state = AuthState::load(
        vec![group_record("g1", Some(100.0))],
        vec![(
            hash_key("fg-g"),
            VirtualKeyRecord {
                group_id: Some("g1".to_owned()),
                ..key_record("k1", None)
            },
        )],
    );
    let mut rx = arm(&state, &[50]);
    let entry = state.authenticate("fg-g", NOW).expect("key valid");

    spend(&entry, 60.0);
    let fired = drain(&mut rx);
    // The key itself is uncapped, so only the pool signals.
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].scope, EventScope::Group);
    assert_eq!(fired[0].subject_id, "g1");
    assert_eq!(fired[0].subject_name, "g1-name");
    assert_eq!(fired[0].threshold, Some(50));

    // A pool refusal is attributed to the group, not the key.
    assert!(state.grant_group("g1", 0));
    spend(&entry, 40.0);
    let _ = drain(&mut rx);
    assert!(entry.admit(NOW, 0, usd_to_micro(1.0)).is_err());
    let fired = drain(&mut rx);
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].event, EventKind::BudgetExhausted);
    assert_eq!(fired[0].scope, EventScope::Group);
}

#[tokio::test]
async fn a_grant_reopens_the_thresholds_it_drops_below() {
    let state = AuthState::load(
        Vec::new(),
        vec![(hash_key("fg-r"), key_record("k1", Some(100.0)))],
    );
    let mut rx = arm(&state, &[50, 80]);
    let entry = state.authenticate("fg-r", NOW).expect("key valid");

    spend(&entry, 85.0);
    let fired: Vec<Option<u8>> = drain(&mut rx).into_iter().map(|e| e.threshold).collect();
    assert_eq!(fired, [Some(50), Some(80)]);

    // Cap raised to $300: 85/300 = 28 %, below both thresholds again.
    assert!(state.grant_key("k1", usd_to_micro(200.0)));
    spend(&entry, 70.0); // $155 of $300 = 51 %
    let fired: Vec<Option<u8>> = drain(&mut rx).into_iter().map(|e| e.threshold).collect();
    assert_eq!(fired, [Some(50)], "the new epoch re-fires 50% only");
}

#[tokio::test]
async fn installing_a_policy_does_not_replay_thresholds_already_crossed() {
    // A key loaded from the database at 90 % of its budget must not fire 50 %
    // and 80 % on its first settle: the flushed spend IS the epoch it is in.
    let state = AuthState::load(
        Vec::new(),
        vec![(
            hash_key("fg-b"),
            VirtualKeyRecord {
                budget_spent: 90.0,
                ..key_record("k1", Some(100.0))
            },
        )],
    );
    let mut rx = arm(&state, &[50, 80, 95]);
    let entry = state.authenticate("fg-b", NOW).expect("key valid");

    spend(&entry, 1.0); // 91 %
    assert!(drain(&mut rx).is_empty());

    // Crossing 95 % after the restart still signals.
    spend(&entry, 5.0);
    let fired: Vec<Option<u8>> = drain(&mut rx).into_iter().map(|e| e.threshold).collect();
    assert_eq!(fired, [Some(95)]);
}

#[tokio::test]
async fn an_unsettled_reservation_refund_does_not_flap_a_threshold() {
    // A failed upstream call refunds the reservation. The threshold it may
    // have transiently crossed stays fired: signals must not oscillate with
    // in-flight reservations.
    let state = AuthState::load(
        Vec::new(),
        vec![(hash_key("fg-f"), key_record("k1", Some(100.0)))],
    );
    let mut rx = arm(&state, &[50]);
    let entry = state.authenticate("fg-f", NOW).expect("key valid");

    spend(&entry, 60.0);
    assert_eq!(drain(&mut rx).len(), 1);

    // Admit and DROP without settling: the budget is refunded.
    {
        let _reservation = entry.admit(NOW, 0, usd_to_micro(20.0)).expect("admitted");
    }
    assert!(drain(&mut rx).is_empty());

    // Back over 50 % again: still no second event for the same threshold.
    spend(&entry, 5.0);
    assert!(drain(&mut rx).is_empty());
}

#[tokio::test]
async fn no_policy_means_no_events_at_all() {
    let state = AuthState::load(
        Vec::new(),
        vec![(hash_key("fg-o"), key_record("k1", Some(10.0)))],
    );
    // Never armed: the default is off (sovereignty pillar).
    assert!(state.signals().is_none());
    let entry = state.authenticate("fg-o", NOW).expect("key valid");
    spend(&entry, 10.0);
    assert!(entry.admit(NOW, 0, usd_to_micro(1.0)).is_err());

    // Arming afterwards must not deliver the events that were never queued.
    let mut rx = arm(&state, &[50]);
    assert!(drain(&mut rx).is_empty());
}

#[tokio::test]
async fn clearing_the_policy_stops_detection() {
    let state = AuthState::load(
        Vec::new(),
        vec![(hash_key("fg-c"), key_record("k1", Some(100.0)))],
    );
    let mut rx = arm(&state, &[50]);
    let entry = state.authenticate("fg-c", NOW).expect("key valid");

    state.set_signals(None);
    spend(&entry, 60.0);
    assert!(drain(&mut rx).is_empty());
}

#[tokio::test]
async fn a_lifecycle_event_carries_the_current_name_and_budget() {
    let state = AuthState::load(
        Vec::new(),
        vec![(hash_key("fg-l"), key_record("k1", Some(100.0)))],
    );
    let mut rx = arm(&state, &[]);
    let entry = state.authenticate("fg-l", NOW).expect("key valid");
    spend(&entry, 25.0);

    entry.signal_lifecycle(EventKind::KeyDeleted);
    let fired = drain(&mut rx);
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].event, EventKind::KeyDeleted);
    assert_eq!(fired[0].subject_id, "k1");
    assert_eq!(fired[0].subject_name, "k1-name");
    assert_eq!(fired[0].budget_spent, 25.0);
    assert_eq!(fired[0].threshold, None);
}

#[tokio::test]
async fn concurrent_settles_across_a_threshold_fire_it_once() {
    let state = Arc::new(AuthState::load(
        Vec::new(),
        vec![(hash_key("fg-p"), key_record("k1", Some(1000.0)))],
    ));
    let mut rx = arm(&state, &[50]);
    let entry = state.authenticate("fg-p", NOW).expect("key valid");

    // 40 threads each spend $20: total $800, well past 50 % of $1000, with
    // many settles landing on both sides of the crossing.
    let barrier = Arc::new(std::sync::Barrier::new(40));
    let handles: Vec<_> = (0..40)
        .map(|_| {
            let entry = Arc::clone(&entry);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                spend(&entry, 20.0);
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("worker thread");
    }

    let fired = drain(&mut rx);
    assert_eq!(fired.len(), 1, "one crossing, one event: {fired:?}");
    assert_eq!(fired[0].threshold, Some(50));
}
