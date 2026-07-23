//! Enforcement tests for the in-memory key state (M5 §5.2).
//!
//! Everything here is memory-only - no database in sight - because that is
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
        group_id: None,
        budget_max: None,
        budget_spent: 0.0,
        rpm_limit: None,
        tpm_limit: None,
        expires_at: None,
        disabled: false,
        created_at: 0,
        deleted_at: None,
    }
}

fn state_with(plaintext: &str, rec: VirtualKeyRecord) -> AuthState {
    AuthState::load(Vec::new(), vec![(hash_key(plaintext), rec)])
}

#[test]
fn race_50_concurrent_requests_on_budget_for_10_exactly_10_pass() {
    // Acceptance criterion 1: a hard budget can never be overrun by
    // concurrency - the reservation is an atomic CAS.
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
                        reservation.settle(usd_to_micro(1.0), 100);
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

    // The final spent counter equals the budget - zero overrun.
    assert_eq!(entry.spent_micro(), usd_to_micro(10.0));

    // And the 51st request is refused with the budget error.
    assert!(matches!(
        entry.admit(NOW, 100, usd_to_micro(1.0)),
        Err(GatewayError::BudgetExceeded { .. })
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
    reservation.settle(usd_to_micro(2.0), 10);
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
    drop(reservation); // upstream call failed / was cancelled - no cost
    assert_eq!(entry.spent_micro(), 0);
    assert!(entry.admit(NOW, 10, usd_to_micro(1.0)).is_ok());
}

#[test]
fn tpm_settles_to_real_usage_freeing_the_window() {
    // M5 point 1: the TPM window debits the estimate at admit, then settle
    // adjusts it down to the real token count, freeing room for later
    // requests within the same window.
    let state = state_with(
        "fg-tpm-settle",
        VirtualKeyRecord {
            tpm_limit: Some(100),
            ..record("tpm-settle")
        },
    );
    let entry = state.authenticate("fg-tpm-settle", NOW).expect("key valid");

    // Reserve an 80-token estimate; only 40 tokens were really used.
    entry.admit(NOW, 80, 0).expect("admitted").settle(0, 40);

    // 40 tokens are booked; the remaining 60 still fit...
    assert!(entry.admit(NOW, 60, 0).is_ok());
    // ...and the window is now full (40 + 60 = 100), so 1 more is refused.
    assert!(matches!(
        entry.admit(NOW, 1, 0),
        Err(GatewayError::QuotaExceeded {
            quota: QuotaKind::Tpm,
            ..
        })
    ));
}

#[test]
fn tpm_settle_absorbs_a_real_usage_above_the_estimate() {
    // The real count wins even past the limit (like the budget): the window
    // overshoots and the NEXT request is refused.
    let state = state_with(
        "fg-tpm-over",
        VirtualKeyRecord {
            tpm_limit: Some(100),
            ..record("tpm-over")
        },
    );
    let entry = state.authenticate("fg-tpm-over", NOW).expect("key valid");

    entry.admit(NOW, 10, 0).expect("admitted").settle(0, 130);
    assert!(matches!(
        entry.admit(NOW, 1, 0),
        Err(GatewayError::QuotaExceeded {
            quota: QuotaKind::Tpm,
            ..
        })
    ));
}

#[test]
fn dropping_an_unsettled_reservation_keeps_the_tpm_debit() {
    // The TPM window is a rate limiter: a request that hit the gateway counts
    // even when the upstream call then fails (the reservation is dropped
    // without settling). The budget IS refunded on drop (no money was spent);
    // the TPM estimate is deliberately kept. Only settle() - a successful call
    // with a known real usage - adjusts the debit.
    let state = state_with(
        "fg-tpm-drop",
        VirtualKeyRecord {
            budget_max: Some(100.0),
            tpm_limit: Some(100),
            ..record("tpm-drop")
        },
    );
    let entry = state.authenticate("fg-tpm-drop", NOW).expect("key valid");

    let reservation = entry.admit(NOW, 100, usd_to_micro(5.0)).expect("admitted");
    drop(reservation);
    // Budget refunded...
    assert_eq!(entry.spent_micro(), 0);
    // ...but the TPM estimate stays: the window is full, so 1 more is refused.
    assert!(matches!(
        entry.admit(NOW, 1, 0),
        Err(GatewayError::QuotaExceeded {
            quota: QuotaKind::Tpm,
            ..
        })
    ));
}

#[test]
fn budget_rejection_unwinds_the_rpm_and_tpm_bumps() {
    // M5 point 2: when the budget step refuses, the RPM and TPM bumps made
    // earlier in the same admit are rolled back - a rejected request burns no
    // quota.
    let state = state_with(
        "fg-unwind",
        VirtualKeyRecord {
            budget_max: Some(1.0),
            rpm_limit: Some(5),
            tpm_limit: Some(100),
            ..record("unwind")
        },
    );
    let entry = state.authenticate("fg-unwind", NOW).expect("key valid");

    // $2 estimate against a $1 budget: refused at the key-budget step.
    assert!(matches!(
        entry.admit(NOW, 10, usd_to_micro(2.0)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Key
        })
    ));

    // The failed attempt consumed neither RPM nor TPM: 5 real requests fit.
    for _ in 0..5 {
        entry.admit(NOW, 10, 0).expect("admitted").settle(0, 10);
    }
    // The 6th is the first genuine RPM rejection.
    assert!(matches!(
        entry.admit(NOW, 1, 0),
        Err(GatewayError::QuotaExceeded {
            quota: QuotaKind::Rpm,
            ..
        })
    ));
}

#[test]
fn tpm_rejection_unwinds_the_rpm_bump() {
    // M5 point 2 (RPM before TPM): a TPM rejection rolls back the RPM bump it
    // made first, so it does not burn a request slot either.
    let state = state_with(
        "fg-unwind2",
        VirtualKeyRecord {
            rpm_limit: Some(5),
            tpm_limit: Some(50),
            ..record("unwind2")
        },
    );
    let entry = state.authenticate("fg-unwind2", NOW).expect("key valid");

    // 60 tokens > 50 TPM: refused at TPM, after RPM was already bumped.
    assert!(matches!(
        entry.admit(NOW, 60, 0),
        Err(GatewayError::QuotaExceeded {
            quota: QuotaKind::Tpm,
            ..
        })
    ));

    // RPM was not consumed: 5 real (small) requests still fit.
    for _ in 0..5 {
        entry.admit(NOW, 1, 0).expect("admitted").settle(0, 1);
    }
    assert!(matches!(
        entry.admit(NOW, 1, 0),
        Err(GatewayError::QuotaExceeded {
            quota: QuotaKind::Rpm,
            ..
        })
    ));
}

#[test]
fn concurrent_tpm_settle_and_new_debits_stay_consistent() {
    // Racy path: 50 threads each admit an over-estimate then settle to the
    // real (smaller) usage concurrently. Both bump and settle are CAS loops,
    // so no update is lost and the window lands on the exact settled total.
    let state = state_with(
        "fg-tpm-race",
        VirtualKeyRecord {
            tpm_limit: Some(1_000_000),
            ..record("tpm-race")
        },
    );
    let entry = state.authenticate("fg-tpm-race", NOW).expect("key valid");

    let barrier = Arc::new(Barrier::new(50));
    let handles: Vec<_> = (0..50)
        .map(|_| {
            let entry = Arc::clone(&entry);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                // Estimate 100 tokens, really use 10.
                entry.admit(NOW, 100, 0).expect("admitted").settle(0, 10);
            })
        })
        .collect();
    for h in handles {
        h.join().expect("thread");
    }

    // 50 requests x 10 real tokens = exactly 500 booked: the remainder fits
    // to the token, then one more is refused.
    assert!(entry.admit(NOW, 1_000_000 - 500, 0).is_ok());
    assert!(matches!(
        entry.admit(NOW, 1, 0),
        Err(GatewayError::QuotaExceeded {
            quota: QuotaKind::Tpm,
            ..
        })
    ));
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
        r.settle(usd_to_micro(1.0), 1_000);
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
    let state = AuthState::load(
        Vec::new(),
        vec![
            (hash_key("fg-disabled"), disabled),
            (hash_key("fg-expired"), expired),
        ],
    );

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
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Key
        })
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
        .settle(usd_to_micro(3.0), 1);

    let first = state.drain_dirty();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].0, "flush");
    assert!((first[0].1 - 3.0).abs() < 1e-9);

    // Unchanged since the flush → clean.
    assert!(state.drain_dirty().is_empty());

    entry
        .admit(NOW, 1, usd_to_micro(1.0))
        .expect("admitted")
        .settle(usd_to_micro(1.0), 1);
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
        .settle(usd_to_micro(1.0), 1);
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

#[test]
fn remove_evicts_a_key_from_the_live_table_immediately() {
    let state = state_with("fg-victim", record("victim"));
    let entry = state.authenticate("fg-victim", NOW).expect("key valid");
    entry
        .admit(NOW, 1, usd_to_micro(2.0))
        .expect("admitted")
        .settle(usd_to_micro(2.0), 1);

    let removed = state.remove("victim").expect("entry was live");
    assert_eq!(
        removed.spent_micro(),
        usd_to_micro(2.0),
        "spend readable for flushing"
    );
    assert!(state.authenticate("fg-victim", NOW).is_none());
    assert!(state.is_empty(), "both maps are emptied");

    // Removing an unknown (or already-removed) id is a harmless no-op.
    assert!(state.remove("victim").is_none());
}

#[test]
fn rotate_swaps_the_hash_and_keeps_the_accrued_spend() {
    let state = state_with(
        "fg-old",
        VirtualKeyRecord {
            budget_max: Some(2.0),
            ..record("spin")
        },
    );
    let entry = state.authenticate("fg-old", NOW).expect("key valid");
    entry
        .admit(NOW, 1, usd_to_micro(1.0))
        .expect("admitted")
        .settle(usd_to_micro(1.0), 1);

    state.rotate(
        hash_key("fg-new"),
        &VirtualKeyRecord {
            budget_max: Some(2.0),
            ..record("spin")
        },
    );

    // The old plaintext dies on the spot; the new one resolves to the SAME
    // entry, spend included: one more dollar fits, a third does not.
    assert!(state.authenticate("fg-old", NOW).is_none());
    let rotated = state.authenticate("fg-new", NOW).expect("new key valid");
    assert_eq!(rotated.spent_micro(), usd_to_micro(1.0));
    rotated
        .admit(NOW, 1, usd_to_micro(1.0))
        .expect("second dollar fits")
        .settle(usd_to_micro(1.0), 1);
    assert!(matches!(
        rotated.admit(NOW, 1, usd_to_micro(1.0)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Key
        })
    ));

    // Rotating an id the table has never seen inserts it fresh (defensive).
    state.rotate(hash_key("fg-ghost"), &record("ghost"));
    assert!(state.authenticate("fg-ghost", NOW).is_some());
}

// ---- Shared parent budgets (ADR 009) ----------------------------------------
//
// A budget group is a shared pool any number of keys draw from. Admission
// reserves against the key budget AND the group pool; a refusal at any step
// consumes nothing (the ADR 007 rule), settle debits both sides, and dropping
// an unsettled reservation refunds both (the TPM debit stays, per ADR 007).

use lumen_auth::store::GroupRecord;
use lumen_core::BudgetScope;

fn group(id: &str, budget_max: Option<f64>) -> GroupRecord {
    GroupRecord {
        id: id.to_owned(),
        name: id.to_owned(),
        budget_max,
        budget_spent: 0.0,
        created_at: 0,
        deleted_at: None,
    }
}

fn member(id: &str, group_id: &str) -> VirtualKeyRecord {
    VirtualKeyRecord {
        group_id: Some(group_id.to_owned()),
        ..record(id)
    }
}

#[test]
fn two_keys_drain_the_shared_pool_and_the_refusal_is_group_scoped() {
    // ADR 009 acceptance: the pool is the boundary, not the keys. Both keys
    // keep ample personal headroom; only the shared $10 pool runs dry.
    let state = AuthState::load(
        vec![group("pool", Some(10.0))],
        vec![
            (
                hash_key("fg-share-a"),
                VirtualKeyRecord {
                    budget_max: Some(100.0),
                    ..member("share-a", "pool")
                },
            ),
            (
                hash_key("fg-share-b"),
                VirtualKeyRecord {
                    budget_max: Some(100.0),
                    ..member("share-b", "pool")
                },
            ),
        ],
    );
    let a = state.authenticate("fg-share-a", NOW).expect("key a valid");
    let b = state.authenticate("fg-share-b", NOW).expect("key b valid");

    a.admit(NOW, 1, usd_to_micro(6.0))
        .expect("a fits the pool")
        .settle(usd_to_micro(6.0), 1);
    b.admit(NOW, 1, usd_to_micro(4.0))
        .expect("b fits the pool")
        .settle(usd_to_micro(4.0), 1);

    // The pool is dry: EITHER key is refused, with the group scope...
    assert!(matches!(
        a.admit(NOW, 1, usd_to_micro(1.0)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Group
        })
    ));
    assert!(matches!(
        b.admit(NOW, 1, usd_to_micro(1.0)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Group
        })
    ));
    // ...while each key's own $100 budget still has plenty of headroom.
    assert_eq!(a.spent_micro(), usd_to_micro(6.0));
    assert_eq!(b.spent_micro(), usd_to_micro(4.0));
}

#[test]
fn a_grouped_keys_own_budget_refusal_stays_key_scoped() {
    // The key budget is checked before the pool, and its refusal keeps the
    // key scope - the message stays byte-identical to the ungrouped one
    // (ADR 009 §2).
    let state = AuthState::load(
        vec![group("wide", Some(100.0))],
        vec![(
            hash_key("fg-tight-key"),
            VirtualKeyRecord {
                budget_max: Some(1.0),
                ..member("tight-key", "wide")
            },
        )],
    );
    let entry = state.authenticate("fg-tight-key", NOW).expect("key valid");
    assert!(matches!(
        entry.admit(NOW, 1, usd_to_micro(2.0)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Key
        })
    ));
}

#[test]
fn group_refusal_unwinds_the_rpm_and_tpm_bumps() {
    // The ADR 007 rule extended to the group step: a request the POOL
    // refuses consumes neither RPM nor TPM (mirror of
    // `budget_rejection_unwinds_the_rpm_and_tpm_bumps`).
    let state = AuthState::load(
        vec![group("tight-pool", Some(1.0))],
        vec![(
            hash_key("fg-unwind-group"),
            VirtualKeyRecord {
                budget_max: Some(50.0),
                rpm_limit: Some(5),
                tpm_limit: Some(50),
                ..member("unwind-group", "tight-pool")
            },
        )],
    );
    let entry = state
        .authenticate("fg-unwind-group", NOW)
        .expect("key valid");

    // $2 estimate against the $1 pool: refused at the group step, after the
    // RPM, TPM and key-budget steps had all already bumped.
    assert!(matches!(
        entry.admit(NOW, 10, usd_to_micro(2.0)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Group
        })
    ));

    // The refused attempt burned nothing: 5 requests of 10 tokens each fill
    // the RPM (5) and TPM (50) windows EXACTLY - only possible if the
    // refused request's bumps were rolled back.
    for _ in 0..5 {
        entry.admit(NOW, 10, 0).expect("admitted").settle(0, 10);
    }
    assert!(matches!(
        entry.admit(NOW, 1, 0),
        Err(GatewayError::QuotaExceeded {
            quota: QuotaKind::Rpm,
            ..
        })
    ));
}

#[test]
fn group_refusal_unwinds_the_key_budget_reservation() {
    let state = AuthState::load(
        vec![group("small-pool", Some(1.0))],
        vec![(
            hash_key("fg-key-unwind"),
            VirtualKeyRecord {
                budget_max: Some(50.0),
                ..member("key-unwind", "small-pool")
            },
        )],
    );
    let entry = state.authenticate("fg-key-unwind", NOW).expect("key valid");

    assert!(matches!(
        entry.admit(NOW, 1, usd_to_micro(2.0)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Group
        })
    ));
    // The key-budget reservation made before the group step was unwound...
    assert_eq!(entry.spent_micro(), 0);
    // ...and a fresh estimate that fits the pool is admitted: the refused
    // request left no residue on the pool either.
    assert!(entry.admit(NOW, 1, usd_to_micro(1.0)).is_ok());
}

#[test]
fn settle_debits_the_key_and_the_group_by_the_same_delta() {
    let state = AuthState::load(
        vec![group("settle-pool", Some(100.0))],
        vec![(
            hash_key("fg-settle-both"),
            VirtualKeyRecord {
                budget_max: Some(100.0),
                ..member("settle-both", "settle-pool")
            },
        )],
    );
    let entry = state
        .authenticate("fg-settle-both", NOW)
        .expect("key valid");

    // Reserve $5, settle at the real $2: both sides land on exactly $2.
    entry
        .admit(NOW, 1, usd_to_micro(5.0))
        .expect("admitted")
        .settle(usd_to_micro(2.0), 1);
    assert_eq!(entry.spent_micro(), usd_to_micro(2.0));

    let dirty = state.drain_dirty_groups();
    assert_eq!(dirty.len(), 1);
    assert_eq!(dirty[0].0, "settle-pool");
    assert!(
        (dirty[0].1 - 2.0).abs() < 1e-9,
        "the pool settled to the same $2, got {}",
        dirty[0].1
    );
}

#[test]
fn dropping_an_unsettled_reservation_refunds_the_key_and_the_group() {
    let state = AuthState::load(
        vec![group("refund-pool", Some(10.0))],
        vec![(
            hash_key("fg-refund-both"),
            VirtualKeyRecord {
                budget_max: Some(10.0),
                ..member("refund-both", "refund-pool")
            },
        )],
    );
    let entry = state
        .authenticate("fg-refund-both", NOW)
        .expect("key valid");

    let reservation = entry.admit(NOW, 1, usd_to_micro(10.0)).expect("admitted");
    drop(reservation); // upstream failed / was cancelled - no money moved

    // The key was refunded...
    assert_eq!(entry.spent_micro(), 0);
    // ...and so was the pool: the FULL pool fits again.
    assert!(entry.admit(NOW, 1, usd_to_micro(10.0)).is_ok());

    // Read the pool counter directly: back to zero after both refunds (the
    // second reservation above was dropped unsettled too).
    let evicted = state.remove_group("refund-pool").expect("group was live");
    assert_eq!(evicted.id(), "refund-pool");
    assert_eq!(evicted.spent_micro(), 0);
}

#[test]
fn an_unlimited_key_in_a_limited_group_is_still_group_capped() {
    let state = AuthState::load(
        vec![group("cap-pool", Some(2.0))],
        // No budget_max on the key itself: only the pool can refuse.
        vec![(hash_key("fg-freewheel"), member("freewheel", "cap-pool"))],
    );
    let entry = state.authenticate("fg-freewheel", NOW).expect("key valid");

    entry
        .admit(NOW, 1, usd_to_micro(2.0))
        .expect("the pool covers it")
        .settle(usd_to_micro(2.0), 1);
    assert!(matches!(
        entry.admit(NOW, 1, usd_to_micro(0.01)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Group
        })
    ));
}

#[test]
fn drain_dirty_groups_reports_group_spend_once_until_it_changes_again() {
    let state = AuthState::load(
        vec![group("flush-pool", Some(100.0))],
        vec![(
            hash_key("fg-group-flush"),
            member("group-flush", "flush-pool"),
        )],
    );
    let entry = state
        .authenticate("fg-group-flush", NOW)
        .expect("key valid");

    // Nothing spent yet -> nothing to flush.
    assert!(state.drain_dirty_groups().is_empty());

    entry
        .admit(NOW, 1, usd_to_micro(3.0))
        .expect("admitted")
        .settle(usd_to_micro(3.0), 1);
    let first = state.drain_dirty_groups();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].0, "flush-pool");
    assert!((first[0].1 - 3.0).abs() < 1e-9);

    // Unchanged since the flush -> clean.
    assert!(state.drain_dirty_groups().is_empty());

    // New spend re-arms the dirty flag.
    entry
        .admit(NOW, 1, usd_to_micro(1.0))
        .expect("admitted")
        .settle(usd_to_micro(1.0), 1);
    let second = state.drain_dirty_groups();
    assert_eq!(second.len(), 1);
    assert!((second[0].1 - 4.0).abs() < 1e-9);
}

#[test]
fn apply_group_raises_the_pool_without_resetting_in_memory_spend() {
    let state = AuthState::load(
        vec![group("grow-pool", Some(1.0))],
        vec![(hash_key("fg-grower"), member("grower", "grow-pool"))],
    );
    let entry = state.authenticate("fg-grower", NOW).expect("key valid");
    entry
        .admit(NOW, 1, usd_to_micro(1.0))
        .expect("admitted")
        .settle(usd_to_micro(1.0), 1);
    assert!(matches!(
        entry.admit(NOW, 1, usd_to_micro(1.0)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Group
        })
    ));

    // Admin raises the pool. The record's budget_spent (0.0) must NOT
    // overwrite the $1 already accrued in memory (limits only, ADR 008).
    state.apply_group(&group("grow-pool", Some(5.0)));

    // $4 fits ($1 accrued + $4 = the new $5 cap held exactly)...
    let reservation = entry
        .admit(NOW, 1, usd_to_micro(4.0))
        .expect("headroom after the raise");
    // ...and one more cent does not: the in-memory spend survived the apply.
    assert!(matches!(
        entry.admit(NOW, 1, usd_to_micro(0.01)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Group
        })
    ));
    drop(reservation);
}

#[test]
fn a_key_patched_out_of_its_group_stops_debiting_the_pool() {
    let state = AuthState::load(
        vec![group("leave-pool", Some(5.0))],
        vec![(hash_key("fg-leaver"), member("leaver", "leave-pool"))],
    );
    let entry = state.authenticate("fg-leaver", NOW).expect("key valid");
    assert_eq!(entry.group_id().as_deref(), Some("leave-pool"));
    entry
        .admit(NOW, 1, usd_to_micro(1.0))
        .expect("admitted")
        .settle(usd_to_micro(1.0), 1);

    // Admin patches the key out of the group: upsert with group_id = None.
    state.upsert(hash_key("fg-leaver"), &record("leaver"));
    assert_eq!(entry.group_id(), None, "the live group pointer is cleared");

    // $10 dwarfs the $5 pool, but the key no longer belongs to it.
    entry
        .admit(NOW, 1, usd_to_micro(10.0))
        .expect("no group cap any more")
        .settle(usd_to_micro(10.0), 1);

    // The pool kept only the pre-departure dollar.
    let evicted = state.remove_group("leave-pool").expect("group was live");
    assert_eq!(evicted.spent_micro(), usd_to_micro(1.0));
}

#[test]
fn group_refusal_re_dirties_the_key_so_a_mid_admit_flush_is_corrected() {
    // Review finding on ADR 009: `reserve_micro` transiently inflates the
    // key's spend before the group step. If a flush drains exactly then, the
    // DB holds the inflated value; the group-refusal unwind must therefore
    // leave the key DIRTY again so the next flush rewrites the corrected
    // spend (mirroring `Reservation::drop`).
    let state = AuthState::load(
        vec![group("tight-pool", Some(1.0))],
        vec![(hash_key("fg-redirty"), member("redirty", "tight-pool"))],
    );
    let entry = state.authenticate("fg-redirty", NOW).expect("key valid");

    entry
        .admit(NOW, 1, usd_to_micro(1.0))
        .expect("first dollar fills the pool")
        .settle(usd_to_micro(1.0), 1);
    // Simulate the periodic flush right before the refused attempt: the key
    // is reported once and marked clean.
    assert_eq!(state.drain_dirty(), vec![("redirty".to_owned(), 1.0)]);
    assert!(
        state.drain_dirty().is_empty(),
        "key is clean after the drain"
    );

    // Group refusal: the key reservation is unwound...
    assert!(matches!(
        entry.admit(NOW, 1, usd_to_micro(1.0)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Group
        })
    ));
    // ...and the unwind itself re-dirties the key with the corrected value,
    // so a flush that raced the transient inflation gets overwritten.
    assert_eq!(state.drain_dirty(), vec![("redirty".to_owned(), 1.0)]);
}

#[test]
fn race_two_keys_on_a_shared_pool_for_10_exactly_10_pass_with_no_residue() {
    // ADR 009 acceptance criterion: the pool can never be overrun by
    // concurrency, even across DIFFERENT member keys, and a refused thread
    // leaves zero residue on its key (the unwind holds under contention).
    let state = AuthState::load(
        vec![group("race-pool", Some(10.0))],
        vec![
            (hash_key("fg-race-a"), member("race-a", "race-pool")),
            (hash_key("fg-race-b"), member("race-b", "race-pool")),
        ],
    );
    let a = state.authenticate("fg-race-a", NOW).expect("key a valid");
    let b = state.authenticate("fg-race-b", NOW).expect("key b valid");

    let barrier = Arc::new(Barrier::new(50));
    let handles: Vec<_> = (0..50)
        .map(|i| {
            let entry = if i % 2 == 0 {
                Arc::clone(&a)
            } else {
                Arc::clone(&b)
            };
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                match entry.admit(NOW, 1, usd_to_micro(1.0)) {
                    Ok(reservation) => {
                        reservation.settle(usd_to_micro(1.0), 1);
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

    // Exactly the pool-covered requests passed, whichever keys carried them.
    assert_eq!(admitted, 10, "the $10 pool admits exactly 10 x $1");
    // The pool landed exactly at its cap, and the keys' combined spend
    // equals the pool's: refused threads left zero residue anywhere.
    let pool = state.remove_group("race-pool").expect("group is live");
    assert_eq!(pool.spent_micro(), usd_to_micro(10.0));
    assert_eq!(
        a.spent_micro() + b.spent_micro(),
        usd_to_micro(10.0),
        "no key residue from refused threads"
    );
}

// ---- Atomic budget grants (prepaid top-ups) ----------------------------------
//
// The grant routes top a budget up while traffic flows: the state half is an
// atomic add on the live entry's cap, so an exhausted key or pool reopens on
// the very next admit - no reload, no restart, no lost concurrent increment.
// An UNLIMITED cap is a sentinel, not a number: granting onto it must stay a
// no-op (a naive fetch_add would wrap it negative and refuse everything).

#[test]
fn grant_key_reopens_an_exhausted_key_for_the_very_next_admit() {
    let state = state_with(
        "fg-grant-key",
        VirtualKeyRecord {
            budget_max: Some(10.0),
            budget_spent: 10.0,
            ..record("grant-key")
        },
    );
    let entry = state.authenticate("fg-grant-key", NOW).expect("key valid");
    // Cap = spent: the key is exhausted, with the key-scoped refusal.
    assert!(matches!(
        entry.admit(NOW, 1, usd_to_micro(1.0)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Key
        })
    ));

    assert!(
        state.grant_key("grant-key", usd_to_micro(5.0)),
        "a live id reports true"
    );

    // The raise landed IN PLACE: the same entry admits on the very next
    // request, and the accrued spend was not touched by the grant.
    assert_eq!(entry.spent_micro(), usd_to_micro(10.0));
    entry
        .admit(NOW, 1, usd_to_micro(5.0))
        .expect("granted headroom admits")
        .settle(usd_to_micro(5.0), 1);
    // The grant added exactly $5: one more cent is refused.
    assert!(matches!(
        entry.admit(NOW, 1, usd_to_micro(0.01)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Key
        })
    ));
}

#[test]
fn grant_group_reopens_a_drained_pool_for_both_member_keys() {
    let state = AuthState::load(
        vec![group("topup-pool", Some(10.0))],
        vec![
            (hash_key("fg-topup-a"), member("topup-a", "topup-pool")),
            (hash_key("fg-topup-b"), member("topup-b", "topup-pool")),
        ],
    );
    let a = state.authenticate("fg-topup-a", NOW).expect("key a valid");
    let b = state.authenticate("fg-topup-b", NOW).expect("key b valid");

    a.admit(NOW, 1, usd_to_micro(6.0))
        .expect("a fits the pool")
        .settle(usd_to_micro(6.0), 1);
    b.admit(NOW, 1, usd_to_micro(4.0))
        .expect("b fits the pool")
        .settle(usd_to_micro(4.0), 1);
    // The pool is dry: either key gets the group-scoped refusal.
    for entry in [&a, &b] {
        assert!(matches!(
            entry.admit(NOW, 1, usd_to_micro(1.0)),
            Err(GatewayError::BudgetExceeded {
                scope: BudgetScope::Group
            })
        ));
    }

    assert!(
        state.grant_group("topup-pool", usd_to_micro(4.0)),
        "a live id reports true"
    );

    // BOTH members see the top-up on their very next admit (they share the
    // pool entry through an Arc)...
    a.admit(NOW, 1, usd_to_micro(2.0))
        .expect("a reopened")
        .settle(usd_to_micro(2.0), 1);
    b.admit(NOW, 1, usd_to_micro(2.0))
        .expect("b reopened")
        .settle(usd_to_micro(2.0), 1);
    // ...and the top-up was exactly $4: the pool is dry again.
    assert!(matches!(
        a.admit(NOW, 1, usd_to_micro(0.01)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Group
        })
    ));
}

#[test]
fn grant_key_on_an_unlimited_cap_keeps_it_unlimited() {
    // budget_max None loads as the UNLIMITED sentinel. A grant onto it is a
    // deliberate no-op that still reports the id as live - it must NOT
    // fetch_add onto i64::MAX (wrapping negative) and must NOT turn the
    // sentinel into a finite $5 cap.
    let state = state_with("fg-grant-unlim", record("grant-unlim"));
    let entry = state
        .authenticate("fg-grant-unlim", NOW)
        .expect("key valid");

    assert!(
        state.grant_key("grant-unlim", usd_to_micro(5.0)),
        "a live id reports true even when the grant is a no-op"
    );

    // Still unlimited: costs far beyond the granted $5 keep passing.
    for _ in 0..2 {
        entry
            .admit(NOW, 1, usd_to_micro(1_000_000.0))
            .expect("an unlimited key admits any cost")
            .settle(usd_to_micro(1_000_000.0), 1);
    }
    assert_eq!(
        entry.spent_micro(),
        usd_to_micro(2_000_000.0),
        "spend is still tracked"
    );
}

#[test]
fn grant_group_on_an_unlimited_pool_keeps_it_unlimited() {
    // The group half of the sentinel rule: an uncapped pool stays uncapped.
    let state = AuthState::load(
        vec![group("free-pool", None)],
        vec![(hash_key("fg-free-rider"), member("free-rider", "free-pool"))],
    );
    let entry = state.authenticate("fg-free-rider", NOW).expect("key valid");

    assert!(
        state.grant_group("free-pool", usd_to_micro(5.0)),
        "a live id reports true even when the grant is a no-op"
    );

    entry
        .admit(NOW, 1, usd_to_micro(1_000_000.0))
        .expect("an unlimited pool admits any cost")
        .settle(usd_to_micro(1_000_000.0), 1);
    let pool = state.remove_group("free-pool").expect("group is live");
    assert_eq!(
        pool.spent_micro(),
        usd_to_micro(1_000_000.0),
        "pool spend is still tracked"
    );
}

#[test]
fn grants_return_false_for_ids_that_are_not_live() {
    let state = AuthState::load(
        vec![group("gone-pool", Some(10.0))],
        vec![(
            hash_key("fg-goner"),
            VirtualKeyRecord {
                budget_max: Some(10.0),
                ..record("goner")
            },
        )],
    );

    // Unknown ids are not live.
    assert!(!state.grant_key("never-seen", usd_to_micro(1.0)));
    assert!(!state.grant_group("never-seen", usd_to_micro(1.0)));

    // Evicted entries are not live either.
    state.remove("goner").expect("key was live");
    state.remove_group("gone-pool").expect("group was live");
    assert!(!state.grant_key("goner", usd_to_micro(1.0)));
    assert!(!state.grant_group("gone-pool", usd_to_micro(1.0)));
}

#[test]
fn race_concurrent_grants_all_land_alongside_concurrent_admits() {
    // The route's whole reason to exist: grants are atomic increments, so
    // N racing top-ups ALL land - none is lost to a read-modify-write -
    // even while admissions hammer the same key and pool. 20 granters add
    // $1 each to a $5 cap and pool; 20 admitters try $1 each. Whatever
    // interleaving happens, cap and pool both end at exactly $25, and the
    // spend equals the number of admitted requests.
    let state = AuthState::load(
        vec![group("grant-pool", Some(5.0))],
        vec![(
            hash_key("fg-grant-race"),
            VirtualKeyRecord {
                budget_max: Some(5.0),
                ..member("grant-race", "grant-pool")
            },
        )],
    );
    let entry = state.authenticate("fg-grant-race", NOW).expect("key valid");

    let barrier = Arc::new(Barrier::new(40));
    let mut admitted = 0_usize;
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..40)
            .map(|i| {
                let entry = Arc::clone(&entry);
                let state = &state;
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    if i % 2 == 0 {
                        assert!(state.grant_key("grant-race", usd_to_micro(1.0)));
                        assert!(state.grant_group("grant-pool", usd_to_micro(1.0)));
                        false
                    } else {
                        match entry.admit(NOW, 1, usd_to_micro(1.0)) {
                            Ok(reservation) => {
                                reservation.settle(usd_to_micro(1.0), 1);
                                true
                            }
                            Err(_) => false,
                        }
                    }
                })
            })
            .collect();
        admitted = handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .filter(|passed| *passed)
            .count();
    });

    // Every grant landed: $5 initial + 20 x $1 on both the key cap and the
    // pool cap. Provable through admission headroom: total capacity is $25
    // on each side, `admitted` requests consumed that much already, and the
    // remainder admits to the dollar - then one more cent is refused.
    let spent = usd_to_micro(1.0) * i64::try_from(admitted).expect("fits");
    assert_eq!(entry.spent_micro(), spent, "spend equals admitted requests");
    let headroom = usd_to_micro(25.0) - spent;
    let remaining = headroom / usd_to_micro(1.0);
    for _ in 0..remaining {
        entry
            .admit(NOW, 1, usd_to_micro(1.0))
            .expect("headroom to the granted caps")
            .settle(usd_to_micro(1.0), 1);
    }
    assert!(
        matches!(
            entry.admit(NOW, 1, usd_to_micro(0.01)),
            Err(GatewayError::BudgetExceeded { .. })
        ),
        "both caps land at exactly $25: the next cent is refused"
    );
}

#[test]
fn a_huge_grant_saturates_below_the_unlimited_sentinel() {
    // grant_cap saturates at UNLIMITED - 1: even a grant whose micro-USD
    // amount clamps to i64::MAX must leave the key CAPPED. A finite cap
    // refuses an i64::MAX-cost admission; the sentinel would admit it.
    let state = AuthState::load(
        Vec::new(),
        vec![(
            hash_key("fg-saturate"),
            VirtualKeyRecord {
                budget_max: Some(1.0),
                ..record("saturate")
            },
        )],
    );
    let entry = state.authenticate("fg-saturate", NOW).expect("key valid");

    // usd_to_micro(9.3e12) clamps to i64::MAX (the sentinel value itself).
    assert!(state.grant_key("saturate", usd_to_micro(9.3e12)));
    assert!(
        matches!(
            entry.admit(NOW, 1, i64::MAX),
            Err(GatewayError::BudgetExceeded {
                scope: BudgetScope::Key
            })
        ),
        "the cap saturated below UNLIMITED and still refuses"
    );
    // A normal admission still fits: saturation kept the cap huge, not zero.
    entry
        .admit(NOW, 1, usd_to_micro(1.0))
        .expect("a sane cost admits against the saturated cap")
        .settle(usd_to_micro(1.0), 1);
}
