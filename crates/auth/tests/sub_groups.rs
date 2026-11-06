//! ADR 020: sub-groups. A key in a sub-group is admitted against its own
//! budget, the sub-group's and the parent's; the parent always binds, a
//! refusal at any step consumes nothing, settle and refund reach all three,
//! bandwidth and billing resolve through the parent, and a sub-group whose
//! parent is missing fails closed. Memory-only, like `enforce.rs`.

use lumen_auth::billing::BillingPolicy;
use lumen_auth::key::hash_key;
use lumen_auth::state::{usd_to_micro, AuthState};
use lumen_auth::store::{GroupRecord, VirtualKeyRecord};
use lumen_core::{BudgetScope, GatewayError};
use std::sync::Arc;

const NOW: i64 = 1_800_000_000;
const ACCOUNT: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";

fn group(id: &str, budget_max: Option<f64>, parent_id: Option<&str>) -> GroupRecord {
    GroupRecord {
        id: id.to_owned(),
        name: id.to_owned(),
        budget_max,
        budget_spent: 0.0,
        created_at: 0,
        deleted_at: None,
        account_ref: None,
        bandwidth_micro_usd_per_gb: 0,
        parent_id: parent_id.map(str::to_owned),
    }
}

fn key(id: &str, group_id: &str) -> VirtualKeyRecord {
    VirtualKeyRecord {
        id: id.to_owned(),
        name: id.to_owned(),
        group_id: Some(group_id.to_owned()),
        budget_max: None,
        budget_spent: 0.0,
        rpm_limit: None,
        tpm_limit: None,
        expires_at: None,
        disabled: false,
        created_at: 0,
        deleted_at: None,
        external_ref: None,
        billed_micro: 0,
    }
}

/// A lease `team` capped at `team_max`, a sub-group `sub` capped at
/// `sub_max`, and key `k` in the sub-group.
fn nested(team_max: Option<f64>, sub_max: Option<f64>) -> AuthState {
    AuthState::load(
        // Child listed first on purpose: load order must not matter.
        vec![
            group("sub", sub_max, Some("team")),
            group("team", team_max, None),
        ],
        vec![(hash_key("sk"), key("k", "sub"))],
    )
}

fn spent(state: &AuthState, id: &str) -> i64 {
    state.group(id).expect("live group").spent_micro()
}

#[test]
fn the_parent_binds_below_a_larger_sub_group_cap() {
    // The sub-group says $100, the team only has $10: the team wins.
    let state = nested(Some(10.0), Some(100.0));
    let k = state.authenticate("sk", NOW).expect("key");

    k.admit(NOW, 1, usd_to_micro(8.0))
        .expect("fits both pools")
        .settle(usd_to_micro(8.0), 1);
    let refused = k.admit(NOW, 1, usd_to_micro(5.0));
    assert!(matches!(
        refused,
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::ParentGroup
        })
    ));
    // The refusal consumed nothing anywhere.
    assert_eq!(k.spent_micro(), usd_to_micro(8.0));
    assert_eq!(spent(&state, "sub"), usd_to_micro(8.0));
    assert_eq!(spent(&state, "team"), usd_to_micro(8.0));
}

#[test]
fn the_sub_group_cap_binds_before_the_parent() {
    let state = nested(Some(100.0), Some(5.0));
    let k = state.authenticate("sk", NOW).expect("key");

    let refused = k.admit(NOW, 1, usd_to_micro(6.0));
    assert!(matches!(
        refused,
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Group
        })
    ));
    assert_eq!(spent(&state, "team"), 0, "the parent was never reserved");
    assert_eq!(spent(&state, "sub"), 0);
    assert_eq!(k.spent_micro(), 0);
}

#[test]
fn an_uncapped_sub_group_is_attribution_bounded_by_the_parent() {
    let state = nested(Some(3.0), None);
    let k = state.authenticate("sk", NOW).expect("key");
    k.admit(NOW, 1, usd_to_micro(3.0))
        .expect("fits")
        .settle(usd_to_micro(3.0), 1);
    assert!(matches!(
        k.admit(NOW, 1, 1),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::ParentGroup
        })
    ));
}

#[test]
fn settle_moves_all_three_and_drop_refunds_all_three() {
    let state = nested(Some(100.0), Some(50.0));
    let k = state.authenticate("sk", NOW).expect("key");

    // Over-reserved estimate, settled lower: every pool sees the real cost.
    k.admit(NOW, 1, usd_to_micro(4.0))
        .expect("fits")
        .settle(usd_to_micro(1.5), 1);
    for id in ["sub", "team"] {
        assert_eq!(spent(&state, id), usd_to_micro(1.5), "{id}");
    }
    assert_eq!(k.spent_micro(), usd_to_micro(1.5));

    // Dropped unsettled (the call failed): nothing is spent anywhere.
    drop(k.admit(NOW, 1, usd_to_micro(2.0)).expect("fits"));
    for id in ["sub", "team"] {
        assert_eq!(spent(&state, id), usd_to_micro(1.5), "{id}");
    }
    assert_eq!(k.spent_micro(), usd_to_micro(1.5));
}

#[test]
fn sibling_sub_groups_share_the_parent() {
    let state = AuthState::load(
        vec![
            group("team", Some(10.0), None),
            group("a", None, Some("team")),
            group("b", None, Some("team")),
        ],
        vec![
            (hash_key("ka"), key("ka", "a")),
            (hash_key("kb"), key("kb", "b")),
        ],
    );
    let a = state.authenticate("ka", NOW).expect("a");
    let b = state.authenticate("kb", NOW).expect("b");
    a.admit(NOW, 1, usd_to_micro(7.0))
        .expect("fits")
        .settle(usd_to_micro(7.0), 1);
    assert!(matches!(
        b.admit(NOW, 1, usd_to_micro(4.0)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::ParentGroup
        })
    ));
    b.admit(NOW, 1, usd_to_micro(3.0))
        .expect("the rest of the team budget fits")
        .settle(usd_to_micro(3.0), 1);
    assert_eq!(spent(&state, "team"), usd_to_micro(10.0));
    assert_eq!(spent(&state, "a"), usd_to_micro(7.0));
    assert_eq!(spent(&state, "b"), usd_to_micro(3.0));
}

#[test]
fn a_parent_cap_change_binds_sub_group_keys_on_the_next_request() {
    let state = nested(Some(100.0), None);
    let k = state.authenticate("sk", NOW).expect("key");
    // The lease sync lowers the team budget to $1.
    state.apply_group(&group("team", Some(1.0), None));
    assert!(matches!(
        k.admit(NOW, 1, usd_to_micro(2.0)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::ParentGroup
        })
    ));
}

#[test]
fn a_sub_group_with_a_missing_parent_fails_closed_then_heals() {
    let state = AuthState::load(
        vec![group("orphan", Some(50.0), Some("gone"))],
        vec![(hash_key("sk"), key("k", "orphan"))],
    );
    let k = state.authenticate("sk", NOW).expect("key");
    // An internal error, not a budget refusal: the team is not out of money.
    assert!(matches!(
        k.admit(NOW, 1, usd_to_micro(1.0)),
        Err(GatewayError::Internal(_))
    ));
    assert_eq!(k.spent_micro(), 0);
    assert_eq!(
        spent(&state, "orphan"),
        0,
        "the sub-group reservation unwound"
    );

    // A later reload that finds the parent links it, no restart needed.
    state.upsert_groups(&[
        group("gone", Some(2.0), None),
        group("orphan", Some(50.0), Some("gone")),
    ]);
    k.admit(NOW, 1, usd_to_micro(1.0))
        .expect("linked")
        .settle(usd_to_micro(1.0), 1);
    assert_eq!(spent(&state, "gone"), usd_to_micro(1.0));
}

#[test]
fn a_reload_batch_links_sub_groups_listed_before_their_parent() {
    let state = AuthState::load(Vec::new(), Vec::new());
    state.upsert_groups(&[
        group("sub", None, Some("team")),
        group("team", Some(1.0), None),
    ]);
    let sub = state.group("sub").expect("sub live");
    assert_eq!(
        sub.parent().map(|p| p.id().to_owned()).as_deref(),
        Some("team")
    );
}

#[test]
fn a_parent_refusal_gives_back_the_quota_windows() {
    let state = AuthState::load(
        vec![
            group("team", Some(1.0), None),
            group("sub", None, Some("team")),
        ],
        vec![(
            hash_key("sk"),
            VirtualKeyRecord {
                rpm_limit: Some(1),
                tpm_limit: Some(10),
                ..key("k", "sub")
            },
        )],
    );
    let k = state.authenticate("sk", NOW).expect("key");
    // Refused by the parent with 10 tokens and the one request of the minute...
    assert!(matches!(
        k.admit(NOW, 10, usd_to_micro(5.0)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::ParentGroup
        })
    ));
    // ...so both are still free for a request that fits.
    k.admit(NOW, 10, usd_to_micro(0.5))
        .expect("RPM and TPM were unwound")
        .settle(usd_to_micro(0.5), 10);
}

#[test]
fn team_keys_and_billing_resolve_through_the_parent() {
    let mut team = group("team", None, None);
    team.account_ref = Some(ACCOUNT.to_owned());
    // The sub-group row carries no account of its own here: billing must
    // still come from the parent.
    let state = AuthState::load(
        vec![team, group("sub", None, Some("team"))],
        vec![(hash_key("sk"), key("k", "sub"))],
    );
    state.set_billing(Some(Arc::new(BillingPolicy {
        source: "eu-1".to_owned(),
    })));
    let k = state.authenticate("sk", NOW).expect("key");
    assert_eq!(k.group_id().as_deref(), Some("sub"));
    assert_eq!(k.root_group_id().as_deref(), Some("team"));

    k.admit(NOW, 1, 700).expect("fits").settle(700, 1);
    let batch = state.drain_flush(1_000);
    let deltas = batch.deltas();
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].account_ref, ACCOUNT);
    assert_eq!(deltas[0].cost_micro, 700);
}

#[test]
fn bandwidth_is_charged_at_the_parent_rate_to_all_three() {
    let mut team = group("team", None, None);
    team.bandwidth_micro_usd_per_gb = 2_000_000;
    let mut sub = group("sub", None, Some("team"));
    // Ignored: a sub-group is charged at its parent's rate.
    sub.bandwidth_micro_usd_per_gb = 9_000_000;
    let state = AuthState::load(vec![team, sub], vec![(hash_key("sk"), key("k", "sub"))]);
    let k = state.authenticate("sk", NOW).expect("key");

    k.add_bytes(500_000_000, 500_000_000); // 1 GB at $2/GB
    assert_eq!(k.spent_micro(), 2_000_000);
    assert_eq!(spent(&state, "sub"), 2_000_000);
    assert_eq!(spent(&state, "team"), 2_000_000);
}

#[test]
fn a_top_level_group_is_its_own_billing_root() {
    let state = AuthState::load(
        vec![group("team", Some(5.0), None)],
        vec![(hash_key("sk"), key("k", "team"))],
    );
    let k = state.authenticate("sk", NOW).expect("key");
    assert_eq!(k.root_group_id().as_deref(), Some("team"));
    assert!(state.group("team").expect("live").parent().is_none());
}
