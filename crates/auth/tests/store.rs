//! Integration tests for the SQLite key store (M5 §5.1).
//!
//! Everything runs against an in-memory SQLite database with the embedded
//! migrations applied, exactly as the server does at boot.

// Exact float literals stored and read back unchanged through SQLite REAL
// columns — strict equality is the correct assertion here.
#![allow(clippy::float_cmp)]

use ferrogate_auth::crypto::MasterKey;
use ferrogate_auth::key::hash_key;
use ferrogate_auth::store::{KeyPatch, KeyStore, NewKey, UsageRecord};

fn new_key(name: &str) -> NewKey {
    NewKey {
        name: name.to_owned(),
        budget_max: Some(10.0),
        rpm_limit: Some(60),
        tpm_limit: Some(100_000),
        expires_at: None,
    }
}

fn usage(key_id: &str, ts: i64) -> UsageRecord {
    UsageRecord {
        key_id: Some(key_id.to_owned()),
        model: "gpt-test".to_owned(),
        model_used: "gpt-test".to_owned(),
        capability: "chat".to_owned(),
        tokens_in: 12,
        tokens_out: 34,
        search_units: None,
        estimated: false,
        cost: 0.001,
        latency_ms: 42,
        status: 200,
        metadata: None,
        ts,
    }
}

#[tokio::test]
async fn create_key_returns_plaintext_once_and_stores_only_the_hash() {
    let store = KeyStore::in_memory().await.expect("open store");
    let (plaintext, record) = store.create_key(new_key("ci")).await.expect("create");

    // The plaintext has the documented shape: `fg-` + 64 hex chars (32 bytes).
    let revealed = plaintext.reveal();
    assert!(revealed.starts_with("fg-"), "prefix");
    assert_eq!(revealed.len(), 3 + 64, "fg- + 32 random bytes hex");

    // The record never carries the plaintext, and the stored hash matches.
    let fetched = store
        .find_by_hash(&hash_key(revealed))
        .await
        .expect("lookup")
        .expect("key exists");
    assert_eq!(fetched.id, record.id);
    assert_eq!(fetched.name, "ci");
    assert_eq!(fetched.budget_max, Some(10.0));
    assert_eq!(fetched.budget_spent, 0.0);
    assert_eq!(fetched.rpm_limit, Some(60));
    assert_eq!(fetched.tpm_limit, Some(100_000));
    assert!(!fetched.disabled);

    // Acceptance criterion 5 (DB half): the plaintext key appears nowhere in
    // the database — not in the keys table, not anywhere else.
    assert!(
        !store.debug_dump().await.expect("dump").contains(revealed),
        "plaintext virtual key must never be stored"
    );
}

#[tokio::test]
async fn plaintext_key_debug_output_is_redacted() {
    let store = KeyStore::in_memory().await.expect("open store");
    let (plaintext, _) = store.create_key(new_key("dbg")).await.expect("create");
    let debugged = format!("{plaintext:?}");
    assert!(!debugged.contains(plaintext.reveal()), "Debug must redact");
    assert!(debugged.contains("REDACTED"));
}

#[tokio::test]
async fn find_by_hash_misses_return_none() {
    let store = KeyStore::in_memory().await.expect("open store");
    let miss = store
        .find_by_hash(&hash_key("fg-does-not-exist"))
        .await
        .expect("lookup");
    assert!(miss.is_none());
}

#[tokio::test]
async fn list_keys_returns_all_records() {
    let store = KeyStore::in_memory().await.expect("open store");
    store.create_key(new_key("a")).await.expect("a");
    store.create_key(new_key("b")).await.expect("b");
    let all = store.list_keys().await.expect("list");
    assert_eq!(all.len(), 2);
}

#[tokio::test]
async fn patch_updates_only_provided_fields() {
    let store = KeyStore::in_memory().await.expect("open store");
    let (_, record) = store.create_key(new_key("patch-me")).await.expect("create");

    let updated = store
        .update_key(
            &record.id,
            KeyPatch {
                disabled: Some(true),
                budget_max: Some(99.0),
                ..KeyPatch::default()
            },
        )
        .await
        .expect("patch")
        .expect("key exists");

    assert!(updated.disabled);
    assert_eq!(updated.budget_max, Some(99.0));
    // Untouched fields survive.
    assert_eq!(updated.name, "patch-me");
    assert_eq!(updated.rpm_limit, Some(60));
}

#[tokio::test]
async fn patch_of_unknown_key_returns_none() {
    let store = KeyStore::in_memory().await.expect("open store");
    let missing = store
        .update_key("nope", KeyPatch::default())
        .await
        .expect("patch");
    assert!(missing.is_none());
}

#[tokio::test]
async fn persist_budgets_survives_reload() {
    // Acceptance criterion 6: spent budgets flushed to the DB are what a
    // restarted gateway reloads — an exhausted key stays exhausted.
    let store = KeyStore::in_memory().await.expect("open store");
    let (_, record) = store.create_key(new_key("spender")).await.expect("create");

    store
        .persist_budgets(&[(record.id.clone(), 10.0)])
        .await
        .expect("flush");

    let reloaded = store.list_keys().await.expect("list");
    assert_eq!(reloaded[0].budget_spent, 10.0);
}

#[tokio::test]
async fn usage_batch_insert_and_retention_purge() {
    let store = KeyStore::in_memory().await.expect("open store");
    let (_, record) = store.create_key(new_key("logger")).await.expect("create");

    let old_ts = 1_000;
    let new_ts = 2_000_000;
    store
        .insert_usage(&[usage(&record.id, old_ts), usage(&record.id, new_ts)])
        .await
        .expect("insert");
    assert_eq!(store.count_usage().await.expect("count"), 2);

    // Purge everything strictly older than the cutoff.
    let purged = store
        .purge_usage_older_than(1_500_000)
        .await
        .expect("purge");
    assert_eq!(purged, 1);
    assert_eq!(store.count_usage().await.expect("count"), 1);
}

#[tokio::test]
async fn usage_metadata_column_roundtrips_json() {
    let store = KeyStore::in_memory().await.expect("open store");
    let mut rec = usage("k", 1);
    rec.key_id = None;
    rec.metadata = Some(r#"{"team":"search"}"#.to_owned());
    store.insert_usage(&[rec]).await.expect("insert");
    let dump = store.debug_dump().await.expect("dump");
    assert!(dump.contains(r#"{"team":"search"}"#));
}

#[tokio::test]
async fn provider_keys_are_encrypted_at_rest_and_roundtrip() {
    let store = KeyStore::in_memory().await.expect("open store");
    let master = MasterKey::from_env_value(&"a".repeat(64)).expect("master key");

    store
        .store_provider_key("openai", "sk-super-secret", &master)
        .await
        .expect("store");

    // Ciphertext at rest: the plaintext never appears in the DB.
    let dump = store.debug_dump().await.expect("dump");
    assert!(!dump.contains("sk-super-secret"));

    let loaded = store
        .load_provider_key("openai", &master)
        .await
        .expect("load")
        .expect("present");
    assert_eq!(loaded, "sk-super-secret");

    // A different master key cannot decrypt.
    let wrong = MasterKey::from_env_value(&"b".repeat(64)).expect("master key");
    assert!(store.load_provider_key("openai", &wrong).await.is_err());
}

#[tokio::test]
async fn expired_and_disabled_flags_load_correctly() {
    let store = KeyStore::in_memory().await.expect("open store");
    let (_, record) = store
        .create_key(NewKey {
            expires_at: Some(123),
            ..new_key("expiring")
        })
        .await
        .expect("create");
    store
        .update_key(
            &record.id,
            KeyPatch {
                disabled: Some(true),
                ..KeyPatch::default()
            },
        )
        .await
        .expect("patch");

    let all = store.list_keys().await.expect("list");
    assert_eq!(all[0].expires_at, Some(123));
    assert!(all[0].disabled);
}
