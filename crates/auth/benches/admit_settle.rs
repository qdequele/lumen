//! Request-path budget accounting micro-benchmarks (ADR 009, 015).
//!
//! Every authenticated request admits a reservation against its key (and,
//! when the key is in a group, against the group pool) and settles it with
//! the real cost. Both are lock-free atomics in memory; this measures that
//! pair on one thread, for a lone key and for a key in a group, so a change
//! to the accounting (such as the ADR 015 settled-cost counter) shows up as
//! a number.
//!
//! A third bench runs one thread per core, each on its OWN key, picking keys
//! that sit next to each other in memory. The keys never contend logically,
//! so any slowdown against the single-thread figure is false sharing: two
//! entries' hot atomics (or one entry's tail and the next entry's `Arc`
//! refcount) landing on one cache line.
//!
//! Run with `cargo bench -p auth --bench admit_settle`.

use std::hint::black_box;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

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

/// How many keys the store loads before picking neighbours for the
/// multi-thread bench: enough that the allocator lays out a long run of
/// back-to-back entries to choose from.
const LOADED_KEYS: usize = 64;

/// One key per worker thread, chosen as close together in memory as the
/// allocator placed them: all `LOADED_KEYS` keys are loaded as boot does,
/// sorted by address, and the window of `threads` consecutive entries with
/// the smallest address span is kept. That is the multi-tenant layout the
/// gateway gets for real (keys loaded in one pass at boot); other
/// allocations may sit between two entries, so "neighbours" means nearest
/// available, not guaranteed adjacent.
fn neighbour_entries(threads: usize) -> Vec<Arc<KeyEntry>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let store = KeyStore::in_memory().await.expect("in-memory store");
        let mut plains = Vec::with_capacity(LOADED_KEYS);
        for i in 0..LOADED_KEYS {
            let (plain, _) = store
                .create_key(NewKey {
                    name: format!("bench-{i}"),
                    budget_max: Some(1e9),
                    ..NewKey::default()
                })
                .await
                .expect("create key");
            plains.push(plain);
        }
        let state = AuthState::load(
            store.load_groups().await.expect("load groups"),
            store.load_auth_entries().await.expect("load keys"),
        );
        let mut entries: Vec<Arc<KeyEntry>> = plains
            .iter()
            .map(|plain| {
                state
                    .authenticate(plain.reveal(), NOW)
                    .expect("the key authenticates")
            })
            .collect();
        entries.sort_by_key(|entry| Arc::as_ptr(entry) as usize);
        let span = |w: &[Arc<KeyEntry>]| {
            Arc::as_ptr(&w[w.len() - 1]) as usize - Arc::as_ptr(&w[0]) as usize
        };
        let start = entries
            .windows(threads)
            .enumerate()
            .min_by_key(|(_, w)| span(w))
            .map_or(0, |(i, _)| i);
        entries.drain(start..start + threads).collect()
    })
}

/// Wall time for every thread to run `iters` admit+settle pairs on its own
/// key, started together on a barrier (thread spawn is not timed).
fn run_parallel(entries: &[Arc<KeyEntry>], iters: u64) -> Duration {
    let start = Arc::new(Barrier::new(entries.len() + 1));
    let handles: Vec<_> = entries
        .iter()
        .map(|entry| {
            let entry = Arc::clone(entry);
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                for _ in 0..iters {
                    let reservation = entry
                        .admit(black_box(NOW), black_box(500), black_box(2_000))
                        .expect("admitted");
                    reservation.settle(black_box(1_500), black_box(420));
                }
            })
        })
        .collect();
    start.wait();
    let began = Instant::now();
    for handle in handles {
        handle.join().expect("worker thread");
    }
    began.elapsed()
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

    let threads = std::thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get)
        .min(8);
    if threads < 2 {
        // One core cannot show false sharing; skip rather than report a
        // single-thread number under a parallel name.
        return;
    }
    let entries = neighbour_entries(threads);
    // Reported per admit+settle pair on one thread: every thread runs
    // `iters` pairs concurrently, so the wall time is the per-thread cost.
    c.bench_function("admit_settle_neighbour_keys_parallel", |b| {
        b.iter_custom(|iters| run_parallel(&entries, iters));
    });
}

criterion_group!(admit_settle, benches);
criterion_main!(admit_settle);
