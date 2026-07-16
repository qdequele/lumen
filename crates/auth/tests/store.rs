//! Integration tests for the SQLite key store (M5 §5.1).
//!
//! Everything runs against an in-memory SQLite database with the embedded
//! migrations applied, exactly as the server does at boot.

// Exact float literals stored and read back unchanged through SQLite REAL
// columns - strict equality is the correct assertion here.
#![allow(clippy::float_cmp)]

use lumen_auth::crypto::MasterKey;
use lumen_auth::key::hash_key;
use lumen_auth::store::{KeyPatch, KeyStore, NewKey, UsageFilter, UsageGroupBy, UsageRecord};

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
        provider: "openai".to_owned(),
        capability: "chat".to_owned(),
        tokens_in: 12,
        tokens_out: 34,
        search_units: None,
        media_count: 0,
        media_bytes: 0,
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
    // the database - not in the keys table, not anywhere else.
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
    let all = store.list_keys(false).await.expect("list");
    assert_eq!(all.len(), 2);
}

#[tokio::test]
async fn delete_key_tombstones_without_touching_usage_history() {
    let store = KeyStore::in_memory().await.expect("open store");
    let (plaintext, record) = store.create_key(new_key("gone")).await.expect("create");
    store
        .insert_usage(&[usage(&record.id, 1)])
        .await
        .expect("usage");

    let deleted = store
        .delete_key(&record.id)
        .await
        .expect("delete")
        .expect("key exists");
    assert!(
        deleted.deleted_at.is_some(),
        "tombstone carries a timestamp"
    );

    // Soft delete: the usage history keeps its key_id attribution.
    assert_eq!(store.count_usage().await.expect("count"), 1);
    // The hash no longer authenticates...
    assert!(store
        .find_by_hash(&hash_key(plaintext.reveal()))
        .await
        .expect("lookup")
        .is_none());
    // ...and a restarted gateway would not load the key either.
    assert!(store.load_auth_entries().await.expect("entries").is_empty());
}

#[tokio::test]
async fn deleted_keys_are_hidden_from_the_default_list_but_visible_on_request() {
    let store = KeyStore::in_memory().await.expect("open store");
    let (_, keep) = store.create_key(new_key("keep")).await.expect("keep");
    let (_, drop) = store.create_key(new_key("drop")).await.expect("drop");
    store
        .delete_key(&drop.id)
        .await
        .expect("delete")
        .expect("key exists");

    let visible = store.list_keys(false).await.expect("list");
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].id, keep.id);

    let all = store.list_keys(true).await.expect("list all");
    assert_eq!(all.len(), 2);
    let tombstone = all.iter().find(|k| k.id == drop.id).expect("tombstone");
    assert!(tombstone.deleted_at.is_some());
}

#[tokio::test]
async fn a_deleted_key_rejects_further_patch_delete_and_rotate() {
    let store = KeyStore::in_memory().await.expect("open store");
    let (_, record) = store.create_key(new_key("dead")).await.expect("create");
    store
        .delete_key(&record.id)
        .await
        .expect("delete")
        .expect("key exists");

    // A tombstone behaves like an unknown id: it cannot be resurrected.
    assert!(store
        .update_key(&record.id, KeyPatch::default())
        .await
        .expect("patch")
        .is_none());
    assert!(store
        .delete_key(&record.id)
        .await
        .expect("delete")
        .is_none());
    assert!(store
        .rotate_key(&record.id)
        .await
        .expect("rotate")
        .is_none());
}

#[tokio::test]
async fn rotate_key_swaps_the_hash_and_preserves_the_record() {
    let store = KeyStore::in_memory().await.expect("open store");
    let (old_plaintext, record) = store.create_key(new_key("spin")).await.expect("create");
    store
        .persist_budgets(&[(record.id.clone(), 4.5)])
        .await
        .expect("flush");

    let (new_plaintext, rotated) = store
        .rotate_key(&record.id)
        .await
        .expect("rotate")
        .expect("key exists");

    // Same generation contract as creation, and a genuinely new secret.
    assert!(new_plaintext.reveal().starts_with("fg-"));
    assert_eq!(new_plaintext.reveal().len(), 3 + 64);
    assert_ne!(new_plaintext.reveal(), old_plaintext.reveal());

    // Identity, budgets, spend and limits are all preserved.
    assert_eq!(rotated.id, record.id);
    assert_eq!(rotated.name, "spin");
    assert_eq!(rotated.budget_max, Some(10.0));
    assert_eq!(rotated.budget_spent, 4.5);
    assert_eq!(rotated.rpm_limit, Some(60));

    // The old hash stops resolving; the new one resolves to the same record.
    assert!(store
        .find_by_hash(&hash_key(old_plaintext.reveal()))
        .await
        .expect("old lookup")
        .is_none());
    let via_new = store
        .find_by_hash(&hash_key(new_plaintext.reveal()))
        .await
        .expect("new lookup")
        .expect("key exists");
    assert_eq!(via_new.id, record.id);

    // Neither plaintext ever lands at rest.
    let dump = store.debug_dump().await.expect("dump");
    assert!(!dump.contains(old_plaintext.reveal()));
    assert!(!dump.contains(new_plaintext.reveal()));
}

#[tokio::test]
async fn rotate_of_unknown_key_returns_none() {
    let store = KeyStore::in_memory().await.expect("open store");
    assert!(store.rotate_key("nope").await.expect("rotate").is_none());
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
    // restarted gateway reloads - an exhausted key stays exhausted.
    let store = KeyStore::in_memory().await.expect("open store");
    let (_, record) = store.create_key(new_key("spender")).await.expect("create");

    store
        .persist_budgets(&[(record.id.clone(), 10.0)])
        .await
        .expect("flush");

    let reloaded = store.list_keys(false).await.expect("list");
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
async fn usage_summary_aggregates_filters_and_groups() {
    let store = KeyStore::in_memory().await.expect("open store");
    let mut chat = usage("key-a", 100);
    chat.estimated = true; // locally estimated counts
    let mut embed = usage("key-b", 200);
    embed.capability = "embed".to_owned();
    embed.model = "embed-small".to_owned();
    embed.model_used = "embed-small".to_owned();
    embed.provider = "tei".to_owned();
    embed.tokens_in = 3;
    embed.tokens_out = 0;
    embed.cost = 0.5;
    let outside = usage("key-a", 900); // outside the window below
    store
        .insert_usage(&[chat, embed, outside])
        .await
        .expect("seed");

    let filter = UsageFilter {
        since: 0,
        until: 500,
        limit: 10,
        ..UsageFilter::default()
    };

    // Grouped by provider: two groups, each with its own aggregates.
    let groups = store
        .usage_summary(&filter, UsageGroupBy::Provider)
        .await
        .expect("summary");
    assert_eq!(groups.len(), 2);
    let openai = groups
        .iter()
        .find(|g| g.group == "openai")
        .expect("openai group");
    assert_eq!(openai.requests, 1);
    assert_eq!(openai.tokens_in, 12);
    assert_eq!(openai.tokens_out, 34);
    assert_eq!(openai.estimated_requests, 1);
    let tei = groups.iter().find(|g| g.group == "tei").expect("tei group");
    assert_eq!(tei.requests, 1);
    assert_eq!(tei.estimated_requests, 0);
    assert_eq!(tei.cost, 0.5);

    // Filtered by capability: only the embed row remains.
    let embed_only = store
        .usage_summary(
            &UsageFilter {
                capability: Some("embed".to_owned()),
                ..filter.clone()
            },
            UsageGroupBy::Model,
        )
        .await
        .expect("summary");
    assert_eq!(embed_only.len(), 1);
    assert_eq!(embed_only[0].group, "embed-small");

    // Filtered by key id.
    let key_a = store
        .usage_summary(
            &UsageFilter {
                key_id: Some("key-a".to_owned()),
                ..filter.clone()
            },
            UsageGroupBy::KeyId,
        )
        .await
        .expect("summary");
    assert_eq!(key_a.len(), 1);
    assert_eq!(key_a[0].group, "key-a");
    assert_eq!(key_a[0].requests, 1);
}

#[tokio::test]
async fn usage_summary_groups_by_model_used_separately_from_model() {
    // A fallback-served request: requested "gpt-test", served by "gpt-mini".
    let store = KeyStore::in_memory().await.expect("open store");
    let mut fell_back = usage("k", 100);
    fell_back.model_used = "gpt-mini".to_owned();
    store
        .insert_usage(&[usage("k", 100), fell_back])
        .await
        .expect("seed");

    let filter = UsageFilter {
        since: 0,
        until: 200,
        limit: 10,
        ..UsageFilter::default()
    };
    let by_model = store
        .usage_summary(&filter, UsageGroupBy::Model)
        .await
        .expect("summary");
    assert_eq!(by_model.len(), 1, "both requested the same model");
    let by_model_used = store
        .usage_summary(&filter, UsageGroupBy::ModelUsed)
        .await
        .expect("summary");
    assert_eq!(by_model_used.len(), 2, "but two different models served");
    assert!(by_model_used.iter().any(|g| g.group == "gpt-mini"));
}

#[tokio::test]
async fn usage_summary_orders_by_cost_and_honors_the_limit() {
    let store = KeyStore::in_memory().await.expect("open store");
    let mut rows = Vec::new();
    for (model, cost) in [("a", 1.0), ("b", 3.0), ("c", 2.0)] {
        let mut r = usage("k", 100);
        r.model = model.to_owned();
        r.cost = cost;
        rows.push(r);
    }
    store.insert_usage(&rows).await.expect("seed");

    let groups = store
        .usage_summary(
            &UsageFilter {
                since: 0,
                until: 1_000,
                limit: 2,
                ..UsageFilter::default()
            },
            UsageGroupBy::Model,
        )
        .await
        .expect("summary");
    assert_eq!(groups.len(), 2, "limit bounds the result size");
    assert_eq!(groups[0].group, "b", "most expensive first");
    assert_eq!(groups[1].group, "c");
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

    let all = store.list_keys(false).await.expect("list");
    assert_eq!(all[0].expires_at, Some(123));
    assert!(all[0].disabled);
}
