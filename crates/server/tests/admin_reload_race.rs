//! A hot reload must never undo an admin key or group mutation that already
//! completed. The reload reads the tables and then upserts its snapshot into
//! the live state; an admin write landing between the two used to be
//! overwritten by the stale snapshot (a deleted key came back to life until
//! the next restart). Both sides now hold `AuthState::sync_guard` across
//! their (database, memory) pair, which these tests pin by holding the guard
//! themselves to freeze one side at the exact point of the race.

mod common;

use std::sync::Arc;
use std::time::Duration;

use lumen_auth::key::hash_key;
use lumen_auth::state::{usd_to_micro, AuthState};
use lumen_auth::store::{KeyPatch, KeyStore, NewKey};
use lumen_server::auth::AuthRuntime;
use lumen_server::reload::refresh_auth_tables;
use lumen_server::AppState;
use lumen_telemetry::{LatencyMetrics, Metrics, TokenMetrics};
use serde_json::json;

const LIMIT: usize = 1024 * 1024;

/// How long a task must stay blocked for the test to call it "waiting on the
/// guard". Generous against a loaded CI box: the unguarded code finishes an
/// in-memory SQLite read in well under a millisecond.
const BLOCKED_FOR: Duration = Duration::from_millis(200);

fn master() -> String {
    "a".repeat(64)
}

/// A live auth runtime over an in-memory store holding one $100 key.
/// Returns the runtime, the key id and the key's plaintext.
async fn runtime_with_key() -> (Arc<AuthRuntime>, String, String) {
    let store = KeyStore::in_memory().await.expect("open store");
    let (plaintext, record) = store
        .create_key(NewKey {
            name: "victim".to_owned(),
            budget_max: Some(100.0),
            ..NewKey::default()
        })
        .await
        .expect("create key");
    let entries = store.load_auth_entries().await.expect("load entries");
    let runtime = Arc::new(AuthRuntime {
        keys: AuthState::load(Vec::new(), entries),
        store,
        admin_token_hash: hash_key(&master()),
        master: None,
    });
    (runtime, record.id, plaintext.reveal().to_owned())
}

async fn spawn_gateway(runtime: Arc<AuthRuntime>) -> String {
    let metrics = Metrics::new();
    let tokens = TokenMetrics::register(&metrics, &[]).expect("register token metrics");
    let latency = LatencyMetrics::register(&metrics).expect("register latency metrics");
    let state =
        AppState::new(metrics, common::empty_registry(), tokens, latency).with_auth(runtime);
    common::spawn_state(state, LIMIT).await
}

#[tokio::test]
async fn a_reload_waits_for_an_admin_delete_and_does_not_resurrect_the_key() {
    let (runtime, id, plaintext) = runtime_with_key().await;

    // An admin mutation is in progress: it holds the guard across its DB
    // write and its in-memory apply.
    let guard = runtime.keys.sync_guard().await;

    // A reload starts now. Unguarded, it would read the table (the key is
    // still live in the DB) and upsert that snapshot whenever it gets to it.
    let reload_runtime = Arc::clone(&runtime);
    let mut reload = tokio::spawn(async move { refresh_auth_tables(&reload_runtime).await });
    assert!(
        tokio::time::timeout(BLOCKED_FOR, &mut reload)
            .await
            .is_err(),
        "the reload must wait for the admin mutation instead of reading a snapshot it will \
         apply after the mutation completed"
    );

    // The admin delete completes: tombstone, then eviction.
    runtime
        .store
        .delete_key(&id)
        .await
        .expect("delete")
        .expect("the key existed");
    assert!(runtime.keys.remove(&id).is_some());
    drop(guard);

    reload.await.expect("reload task");
    assert!(
        runtime.keys.authenticate(&plaintext, 0).is_none(),
        "a key deleted before the reload's upsert pass must stay dead"
    );
    assert!(runtime.keys.is_empty());
}

#[tokio::test]
async fn a_reload_waits_for_an_admin_patch_and_keeps_the_lowered_budget() {
    let (runtime, id, plaintext) = runtime_with_key().await;

    let guard = runtime.keys.sync_guard().await;
    let reload_runtime = Arc::clone(&runtime);
    let mut reload = tokio::spawn(async move { refresh_auth_tables(&reload_runtime).await });
    assert!(tokio::time::timeout(BLOCKED_FOR, &mut reload)
        .await
        .is_err());

    // The admin PATCH lowers the cap from $100 to $1 and disables nothing.
    let updated = runtime
        .store
        .update_key(
            &id,
            KeyPatch {
                budget_max: Some(1.0),
                ..KeyPatch::default()
            },
        )
        .await
        .expect("patch")
        .expect("the key exists");
    runtime.keys.apply(&updated);
    drop(guard);

    reload.await.expect("reload task");
    let entry = runtime
        .keys
        .authenticate(&plaintext, 0)
        .expect("still live");
    assert!(
        entry.admit(0, 1, usd_to_micro(5.0)).is_err(),
        "the reload must not restore the $100 cap it could have read before the PATCH"
    );
}

#[tokio::test]
async fn admin_key_mutations_wait_for_a_reload_in_progress() {
    let (runtime, id, plaintext) = runtime_with_key().await;
    let base = spawn_gateway(Arc::clone(&runtime)).await;
    let client = reqwest::Client::new();

    // A reload is between its DB read and its upsert pass.
    let guard = runtime.keys.sync_guard().await;

    let url = format!("{base}/admin/keys/{id}");
    let delete_client = client.clone();
    let delete_url = url.clone();
    let mut delete = tokio::spawn(async move {
        delete_client
            .delete(delete_url)
            .bearer_auth(master())
            .send()
            .await
            .expect("delete request")
            .status()
    });
    let patch_client = client.clone();
    let mut patch = tokio::spawn(async move {
        patch_client
            .patch(url)
            .bearer_auth(master())
            .json(&json!({ "disabled": true }))
            .send()
            .await
            .expect("patch request")
            .status()
    });
    assert!(
        tokio::time::timeout(BLOCKED_FOR, &mut delete)
            .await
            .is_err(),
        "DELETE /admin/keys/{{id}} must not write while a reload holds its snapshot"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut patch)
            .await
            .is_err(),
        "PATCH /admin/keys/{{id}} must not write while a reload holds its snapshot"
    );
    assert!(
        runtime.keys.authenticate(&plaintext, 0).is_some(),
        "nothing was applied while the guard was held"
    );
    let row = runtime
        .store
        .get_key(&id)
        .await
        .expect("read key")
        .expect("the row exists");
    assert!(
        !row.disabled,
        "the DB write itself must wait, not only the in-memory apply"
    );
    drop(guard);

    let delete_status = delete.await.expect("delete task");
    let patch_status = patch.await.expect("patch task");
    // Whichever ran second saw the other's result; both are legal outcomes.
    assert!(delete_status == 204, "delete answered {delete_status}");
    assert!(
        patch_status == 200 || patch_status == 404,
        "patch answered {patch_status}"
    );
    assert!(runtime.keys.authenticate(&plaintext, 0).is_none());
}
