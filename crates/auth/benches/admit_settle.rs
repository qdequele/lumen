//! Request-path budget accounting micro-benchmarks (ADR 009, 015).
//!
//! Every authenticated request admits a reservation against its key (and,
//! when the key is in a group, against the group pool) and settles it with
//! the real cost. Both are lock-free atomics in memory; this measures that
//! pair on one thread, for a lone key and for a key in a group, so a change
//! to the accounting (such as the ADR 015 settled-cost counter) shows up as
//! a number.
//!
//! Run with `cargo bench -p auth --bench admit_settle`.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion};
use lumen_auth::state::{AuthState, KeyEntry};
use lumen_auth::store::{KeyStore, NewGroup, NewKey};

/// Unix seconds passed to `admit` (fixed: the minute window never rolls).
const NOW: i64 = 1_800_000_000;

/// One key with a budget, optionally in a budgeted group, loaded as boot
/// does. Budgets are large enough that no iteration count exhausts them.
fn entry(in_group: bool) -> Arc<KeyEntry> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let store = KeyStore::in_memory().await.expect("in-memory store");
        let group_id = if in_group {
            let group = store
                .create_group(NewGroup {
                    name: "lease".to_owned(),
                    budget_max: Some(1e9),
                    account_ref: None,
                })
                .await
                .expect("create group");
            Some(group.id)
        } else {
            None
        };
        let (plain, _) = store
            .create_key(NewKey {
                name: "bench".to_owned(),
                group_id,
                budget_max: Some(1e9),
                ..NewKey::default()
            })
            .await
            .expect("create key");
        let state = AuthState::load(
            store.load_groups().await.expect("load groups"),
            store.load_auth_entries().await.expect("load keys"),
        );
        state
            .authenticate(plain.reveal(), NOW)
            .expect("the key authenticates")
    })
}

fn benches(c: &mut Criterion) {
    for (name, in_group) in [
        ("admit_settle_key", false),
        ("admit_settle_key_in_group", true),
    ] {
        let key = entry(in_group);
        c.bench_function(name, |b| {
            b.iter(|| {
                let reservation = key
                    .admit(black_box(NOW), black_box(500), black_box(2_000))
                    .expect("admitted");
                reservation.settle(black_box(1_500), black_box(420));
            });
        });
    }
}

criterion_group!(admit_settle, benches);
criterion_main!(admit_settle);
