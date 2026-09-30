//! ADR 015: opaque control-plane refs on keys and groups, and the billing
//! watermark column.

use lumen_auth::store::{GroupPatch, KeyPatch, KeyStore, NewGroup, NewKey};

fn key(name: &str) -> NewKey {
    NewKey {
        name: name.to_owned(),
        ..NewKey::default()
    }
}

#[tokio::test]
async fn key_external_ref_round_trips_and_clears() {
    let store = KeyStore::in_memory().await.unwrap();
    let (_, created) = store
        .create_key(NewKey {
            external_ref: Some("lab-key-1".to_owned()),
            ..key("k")
        })
        .await
        .unwrap();
    assert_eq!(created.external_ref.as_deref(), Some("lab-key-1"));
    assert_eq!(created.billed_micro, 0);

    // Absent in the patch: unchanged.
    let kept = store
        .update_key(
            &created.id,
            KeyPatch {
                name: Some("k2".to_owned()),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.external_ref.as_deref(), Some("lab-key-1"));

    // Explicit null: cleared.
    let patch: KeyPatch = serde_json::from_str(r#"{"external_ref": null}"#).unwrap();
    let cleared = store.update_key(&created.id, patch).await.unwrap().unwrap();
    assert_eq!(cleared.external_ref, None);

    let listed = store.list_keys(false).await.unwrap();
    assert_eq!(listed[0].external_ref, None);
}

#[tokio::test]
async fn group_account_ref_round_trips_and_clears() {
    let store = KeyStore::in_memory().await.unwrap();
    let group = store
        .create_group(NewGroup {
            name: "acme".to_owned(),
            budget_max: Some(10.0),
            account_ref: Some("0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61".to_owned()),
        })
        .await
        .unwrap();
    assert_eq!(
        group.account_ref.as_deref(),
        Some("0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61")
    );
    let patch: GroupPatch = serde_json::from_str(r#"{"account_ref": null}"#).unwrap();
    let cleared = store.update_group(&group.id, patch).await.unwrap().unwrap();
    assert_eq!(cleared.account_ref, None);
    let loaded = store.load_groups().await.unwrap();
    assert_eq!(loaded[0].account_ref, None);
}

#[tokio::test]
async fn load_auth_entries_carries_refs_and_watermark() {
    let store = KeyStore::in_memory().await.unwrap();
    let (_, created) = store
        .create_key(NewKey {
            external_ref: Some("x".to_owned()),
            ..key("k")
        })
        .await
        .unwrap();
    sqlx::query("UPDATE virtual_keys SET billed_micro = 4200 WHERE id = ?")
        .bind(&created.id)
        .execute(store.pool())
        .await
        .unwrap();
    let entries = store.load_auth_entries().await.unwrap();
    assert_eq!(entries[0].1.external_ref.as_deref(), Some("x"));
    assert_eq!(entries[0].1.billed_micro, 4200);
}

#[tokio::test]
async fn billed_micro_is_never_serialized() {
    let store = KeyStore::in_memory().await.unwrap();
    let (_, created) = store.create_key(key("k")).await.unwrap();
    let json = serde_json::to_value(&created).unwrap();
    assert!(json.get("billed_micro").is_none());
    assert!(json.get("external_ref").is_some());
}

#[tokio::test]
async fn migration_backfills_the_watermark_from_spend() {
    // Re-run the 0011 back-fill statement on a row with recorded spend: the
    // watermark must equal the spend in micro-USD, so enabling billing on an
    // upgraded gateway never bills spend from before the upgrade.
    let store = KeyStore::in_memory().await.unwrap();
    let (_, created) = store.create_key(key("k")).await.unwrap();
    sqlx::query("UPDATE virtual_keys SET budget_spent = 1.234567, billed_micro = 0 WHERE id = ?")
        .bind(&created.id)
        .execute(store.pool())
        .await
        .unwrap();
    let backfill = include_str!("../migrations/0011_lab_refs.sql")
        .lines()
        .find(|l| l.starts_with("UPDATE virtual_keys SET billed_micro"))
        .unwrap();
    sqlx::query(backfill).execute(store.pool()).await.unwrap();
    let entries = store.load_auth_entries().await.unwrap();
    assert_eq!(entries[0].1.billed_micro, 1_234_567);
}
