//! Enforcement tests for the in-memory key state (M5 §5.2).
//!
//! Everything here is memory-only — no database in sight — because that is
//! the whole point: admission control never touches SQLite.

use lumen_auth::key::hash_key;
use lumen_auth::state::{usd_to_micro, AuthState};
use lumen_auth::store::VirtualKeyRecord;
use lumen_core::{GatewayError, QuotaKind};
use std::sync::{Arc, Barrier};

const NOW: i64 = 1_800_000_000;

fn record(id: &str) -> VirtualKeyRecord {
    VirtualKeyRecord {
        id: id.to_owned(),
        name: id.to_owned(),
        budget_max: None,
        budget_spent: 0.0,
        rpm_limit: None,
        tpm_limit: None,
        expires_at: None,
        disabled: false,
        created_at: 0,
    }
}

fn state_with(plaintext: &str, rec: VirtualKeyRecord) -> AuthState {
    AuthState::load(vec![(hash_key(plaintext), rec)])
}

#[test]
fn race_50_concurrent_requests_on_budget_for_10_exactly_10_pass() {
    // Acceptance criterion 1: a hard budget can never be overrun by
    // concurrency — the reservation is an atomic CAS.
    let state = state_with(
        "fg-race",
        VirtualKeyRecord {
            budget_max: Some(10.0),
            ..record("race")
        },
    );
    let entry = state.authenticate("fg-race", NOW).expect("key valid");

    let barrier = Arc::new(Barrier::new(50));
    let handles: Vec<_> = (0..50)
        .map(|_| {
            let entry = Arc::clone(&entry);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                match entry.admit(NOW, 100, usd_to_micro(1.0)) {
                    Ok(reservation) => {
                        // Simulate the upstream call completing at exactly the
                        // estimated cost.
                        reservation.settle(usd_to_micro(1.0));
                        true
                    }
                    Err(_) => false,
                }
            })
        })
        .collect();

    let admitted = handles
        .into_iter()
        .filter_map(|h| h.join().ok())
        .filter(|passed| *passed)
        .count();
    assert_eq!(admitted, 10, "exactly the budget-covered requests pass");

    // The final spent counter equals the budget — zero overrun.
    assert_eq!(entry.spent_micro(), usd_to_micro(10.0));

    // And the 51st request is refused with the budget error.
    assert!(matches!(
        entry.admit(NOW, 100, usd_to_micro(1.0)),
        Err(GatewayError::BudgetExceeded)
    ));
}

#[test]
fn settle_adjusts_the_reservation_to_the_real_cost() {
    let state = state_with(
        "fg-adjust",
        VirtualKeyRecord {
            budget_max: Some(10.0),
            ..record("adjust")
        },
    );
    let entry = state.authenticate("fg-adjust", NOW).expect("key valid");

    let reservation = entry.admit(NOW, 10, usd_to_micro(5.0)).expect("admitted");
    reservation.settle(usd_to_micro(2.0));
    assert_eq!(entry.spent_micro(), usd_to_micro(2.0));

    // 8 USD still fits: the over-reservation was released on settle.
    assert!(entry.admit(NOW, 10, usd_to_micro(8.0)).is_ok());
}

#[test]
fn dropping_an_unsettled_reservation_refunds_it() {
    let state = state_with(
        "fg-refund",
        VirtualKeyRecord {
            budget_max: Some(1.0),
            ..record("refund")
        },
    );
    let entry = state.authenticate("fg-refund", NOW).expect("key valid");

    let reservation = entry.admit(NOW, 10, usd_to_micro(1.0)).expect("admitted");
    drop(reservation); // upstream call failed / was cancelled — no cost
    assert_eq!(entry.spent_micro(), 0);
    assert!(entry.admit(NOW, 10, usd_to_micro(1.0)).is_ok());
}

#[test]
fn rpm_quota_rejects_within_the_window_and_resets_next_minute() {
    let state = state_with(
        "fg-rpm",
        VirtualKeyRecord {
            rpm_limit: Some(2),
            ..record("rpm")
        },
    );
    let entry = state.authenticate("fg-rpm", NOW).expect("key valid");

    assert!(entry.admit(NOW, 1, 0).is_ok());
    assert!(entry.admit(NOW, 1, 0).is_ok());
    match entry.admit(NOW, 1, 0) {
        Err(GatewayError::QuotaExceeded {
            quota: QuotaKind::Rpm,
            retry_after,
        }) => {
            let secs = retry_after.expect("retry-after advertised").as_secs();
            assert!((1..=60).contains(&secs), "points at the next window");
        }
        other => panic!("expected RPM rejection, got {other:?}"),
    }

    // A new minute opens a fresh window.
    assert!(entry.admit(NOW + 60, 1, 0).is_ok());
}

#[test]
fn tpm_quota_counts_estimated_tokens() {
    let state = state_with(
        "fg-tpm",
        VirtualKeyRecord {
            tpm_limit: Some(100),
            ..record("tpm")
        },
    );
    let entry = state.authenticate("fg-tpm", NOW).expect("key valid");

    assert!(entry.admit(NOW, 60, 0).is_ok());
    assert!(matches!(
        entry.admit(NOW, 60, 0),
        Err(GatewayError::QuotaExceeded {
            quota: QuotaKind::Tpm,
            ..
        })
    ));
    // 40 more still fit in this window…
    assert!(entry.admit(NOW, 40, 0).is_ok());
    // …and the next minute starts clean.
    assert!(entry.admit(NOW + 60, 60, 0).is_ok());
}

#[test]
fn unlimited_keys_never_reject_but_still_track_spend() {
    let state = state_with("fg-unlimited", record("unlimited"));
    let entry = state.authenticate("fg-unlimited", NOW).expect("key valid");

    for _ in 0..100 {
        let r = entry.admit(NOW, 1_000, usd_to_micro(1.0)).expect("no caps");
        r.settle(usd_to_micro(1.0));
    }
    assert_eq!(entry.spent_micro(), usd_to_micro(100.0));
}

#[test]
fn authenticate_rejects_unknown_disabled_and_expired_keys() {
    let disabled = VirtualKeyRecord {
        disabled: true,
        ..record("disabled")
    };
    let expired = VirtualKeyRecord {
        expires_at: Some(NOW - 1),
        ..record("expired")
    };
    let state = AuthState::load(vec![
        (hash_key("fg-disabled"), disabled),
        (hash_key("fg-expired"), expired),
    ]);

    assert!(state.authenticate("fg-unknown", NOW).is_none());
    assert!(state.authenticate("fg-disabled", NOW).is_none());
    assert!(state.authenticate("fg-expired", NOW).is_none());

    // An expired key was valid a second before its expiry.
    assert!(state.authenticate("fg-expired", NOW - 2).is_some());
}

#[test]
fn boot_load_restores_spent_budget_an_exhausted_key_stays_exhausted() {
    // Acceptance criterion 6 (memory half): reloading a fully-spent key from
    // the DB keeps it exhausted.
    let state = state_with(
        "fg-restart",
        VirtualKeyRecord {
            budget_max: Some(10.0),
            budget_spent: 10.0,
            ..record("restart")
        },
    );
    let entry = state.authenticate("fg-restart", NOW).expect("key valid");
    assert!(matches!(
        entry.admit(NOW, 1, usd_to_micro(0.01)),
        Err(GatewayError::BudgetExceeded)
    ));
}

#[test]
fn drain_dirty_reports_spend_once_until_it_changes_again() {
    let state = state_with(
        "fg-flush",
        VirtualKeyRecord {
            budget_max: Some(100.0),
            ..record("flush")
        },
    );
    let entry = state.authenticate("fg-flush", NOW).expect("key valid");

    // Nothing spent yet → nothing to flush.
    assert!(state.drain_dirty().is_empty());

    entry
        .admit(NOW, 1, usd_to_micro(3.0))
        .expect("admitted")
        .settle(usd_to_micro(3.0));

    let first = state.drain_dirty();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].0, "flush");
    assert!((first[0].1 - 3.0).abs() < 1e-9);

    // Unchanged since the flush → clean.
    assert!(state.drain_dirty().is_empty());

    entry
        .admit(NOW, 1, usd_to_micro(1.0))
        .expect("admitted")
        .settle(usd_to_micro(1.0));
    let second = state.drain_dirty();
    assert_eq!(second.len(), 1);
    assert!((second[0].1 - 4.0).abs() < 1e-9);
}

#[test]
fn upsert_and_apply_reflect_admin_changes_immediately() {
    let state = state_with(
        "fg-admin",
        VirtualKeyRecord {
            budget_max: Some(1.0),
            ..record("admin")
        },
    );
    let entry = state.authenticate("fg-admin", NOW).expect("key valid");
    entry
        .admit(NOW, 1, usd_to_micro(1.0))
        .expect("admitted")
        .settle(usd_to_micro(1.0));
    assert!(entry.admit(NOW, 1, usd_to_micro(1.0)).is_err());

    // Admin raises the budget → more spend fits, accrued spend preserved.
    state.apply(&VirtualKeyRecord {
        budget_max: Some(5.0),
        ..record("admin")
    });
    assert!(entry.admit(NOW, 1, usd_to_micro(1.0)).is_ok());
    assert!(entry.spent_micro() >= usd_to_micro(1.0));

    // Admin disables the key → authentication refuses it on the spot.
    state.apply(&VirtualKeyRecord {
        disabled: true,
        ..record("admin")
    });
    assert!(state.authenticate("fg-admin", NOW).is_none());

    // A brand-new key becomes usable via upsert.
    let fresh = record("fresh");
    state.upsert(hash_key("fg-fresh"), &fresh);
    assert!(state.authenticate("fg-fresh", NOW).is_some());
}
