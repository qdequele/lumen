//! Integration tests for budget groups in the SQLite store (ADR 009):
//! CRUD round-trips, tombstone semantics, member-guarded deletion,
//! group-membership validation on key create/patch, and the group budget
//! flush - all against an in-memory SQLite database with the embedded
//! migrations applied, exactly as the server does at boot.

// Exact float literals stored and read back unchanged through SQLite REAL
// columns - strict equality is the correct assertion here.
#![allow(clippy::float_cmp)]

use lumen_auth::key::hash_key;
use lumen_auth::store::{DeleteGroupOutcome, GroupPatch, KeyPatch, KeyStore, NewGroup, NewKey};
use lumen_auth::AuthError;

fn new_group(name: &str) -> NewGroup {
    NewGroup {
        name: name.to_owned(),
        budget_max: Some(25.0),
    }
}

fn member_key(name: &str, group_id: &str) -> NewKey {
    NewKey {
        name: name.to_owned(),
        group_id: Some(group_id.to_owned()),
        ..NewKey::default()
    }
}

#[tokio::test]
async fn create_group_returns_the_record_with_zero_spend() {
    let store = KeyStore::in_memory().await.expect("open store");
    let record = store
        .create_group(new_group("acme"))
        .await
        .expect("create group");

    assert!(!record.id.is_empty(), "a group gets an opaque id");
    assert_eq!(record.name, "acme");
    assert_eq!(record.budget_max, Some(25.0));
    assert_eq!(record.budget_spent, 0.0);
    assert!(record.deleted_at.is_none());

    let listed = store.list_groups(false).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, record.id);
}

#[tokio::test]
async fn a_group_without_a_budget_is_a_pure_attribution_container() {
    // ADR 009 §1: budget_max = NULL means unlimited.
    let store = KeyStore::in_memory().await.expect("open store");
    let record = store
        .create_group(NewGroup {
            name: "tracking-only".to_owned(),
            budget_max: None,
        })
        .await
        .expect("create group");
    assert_eq!(record.budget_max, None);
}

#[tokio::test]
async fn update_group_patches_only_provided_fields() {
    let store = KeyStore::in_memory().await.expect("open store");
    let record = store
        .create_group(new_group("patch-me"))
        .await
        .expect("create");

    let renamed = store
        .update_group(
            &record.id,
            GroupPatch {
                name: Some("renamed".to_owned()),
                ..GroupPatch::default()
            },
        )
        .await
        .expect("patch")
        .expect("group exists");
    assert_eq!(renamed.name, "renamed");
    assert_eq!(renamed.budget_max, Some(25.0), "untouched field survives");

    let raised = store
        .update_group(
            &record.id,
            GroupPatch {
                budget_max: Some(99.0),
                ..GroupPatch::default()
            },
        )
        .await
        .expect("patch")
        .expect("group exists");
    assert_eq!(raised.budget_max, Some(99.0));
    assert_eq!(raised.name, "renamed", "untouched field survives");
}

#[tokio::test]
async fn update_group_of_unknown_or_deleted_id_returns_none() {
    let store = KeyStore::in_memory().await.expect("open store");
    assert!(store
        .update_group("nope", GroupPatch::default())
        .await
        .expect("patch")
        .is_none());

    let record = store.create_group(new_group("dead")).await.expect("create");
    assert!(matches!(
        store.delete_group(&record.id).await.expect("delete"),
        DeleteGroupOutcome::Deleted(_)
    ));
    // A tombstone behaves like an unknown id: it cannot be resurrected.
    assert!(store
        .update_group(&record.id, GroupPatch::default())
        .await
        .expect("patch")
        .is_none());
}

#[tokio::test]
async fn delete_group_tombstones_and_stays_visible_on_request() {
    let store = KeyStore::in_memory().await.expect("open store");
    let keep = store.create_group(new_group("keep")).await.expect("keep");
    let drop = store.create_group(new_group("drop")).await.expect("drop");

    let outcome = store.delete_group(&drop.id).await.expect("delete");
    let DeleteGroupOutcome::Deleted(tombstone) = outcome else {
        panic!("expected the empty group to be deleted");
    };
    assert!(
        tombstone.deleted_at.is_some(),
        "tombstone carries a timestamp"
    );

    let visible = store.list_groups(false).await.expect("list");
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].id, keep.id);

    let all = store.list_groups(true).await.expect("list all");
    assert_eq!(all.len(), 2);
    let listed_tombstone = all
        .iter()
        .find(|g| g.id == drop.id)
        .expect("tombstone listed");
    assert!(listed_tombstone.deleted_at.is_some());
}

#[tokio::test]
async fn delete_group_with_active_member_keys_reports_the_member_count() {
    let store = KeyStore::in_memory().await.expect("open store");
    let group = store.create_group(new_group("busy")).await.expect("create");
    store
        .create_key(member_key("m1", &group.id))
        .await
        .expect("m1");
    let (_, m2) = store
        .create_key(member_key("m2", &group.id))
        .await
        .expect("m2");

    assert!(matches!(
        store.delete_group(&group.id).await.expect("delete"),
        DeleteGroupOutcome::HasMembers(2)
    ));
    // The refusal wrote nothing: the group is still active.
    assert_eq!(store.list_groups(false).await.expect("list").len(), 1);

    // Tombstoned member keys stop counting: with one key soft-deleted, one
    // active member remains.
    store
        .delete_key(&m2.id)
        .await
        .expect("delete key")
        .expect("key exists");
    assert!(matches!(
        store.delete_group(&group.id).await.expect("delete"),
        DeleteGroupOutcome::HasMembers(1)
    ));
}

#[tokio::test]
async fn delete_group_succeeds_once_every_member_key_is_deleted() {
    let store = KeyStore::in_memory().await.expect("open store");
    let group = store
        .create_group(new_group("emptying"))
        .await
        .expect("create");
    let (_, member) = store
        .create_key(member_key("last", &group.id))
        .await
        .expect("member");

    assert!(matches!(
        store.delete_group(&group.id).await.expect("delete"),
        DeleteGroupOutcome::HasMembers(1)
    ));

    store
        .delete_key(&member.id)
        .await
        .expect("delete key")
        .expect("key exists");
    let DeleteGroupOutcome::Deleted(tombstone) =
        store.delete_group(&group.id).await.expect("delete")
    else {
        panic!("expected deletion once the last member key is tombstoned");
    };
    assert!(tombstone.deleted_at.is_some());
}

#[tokio::test]
async fn delete_group_of_unknown_or_already_deleted_id_is_not_found() {
    let store = KeyStore::in_memory().await.expect("open store");
    assert!(matches!(
        store.delete_group("nope").await.expect("delete"),
        DeleteGroupOutcome::NotFound
    ));

    let record = store
        .create_group(new_group("twice"))
        .await
        .expect("create");
    assert!(matches!(
        store.delete_group(&record.id).await.expect("delete"),
        DeleteGroupOutcome::Deleted(_)
    ));
    // A tombstone cannot be deleted twice.
    assert!(matches!(
        store.delete_group(&record.id).await.expect("delete again"),
        DeleteGroupOutcome::NotFound
    ));
}

#[tokio::test]
async fn create_key_with_unknown_or_deleted_group_is_refused_before_any_write() {
    let store = KeyStore::in_memory().await.expect("open store");

    let err = store
        .create_key(member_key("orphan", "nope"))
        .await
        .expect_err("an unknown group must refuse the create");
    assert!(matches!(err, AuthError::UnknownGroup(id) if id == "nope"));

    let group = store.create_group(new_group("gone")).await.expect("create");
    assert!(matches!(
        store.delete_group(&group.id).await.expect("delete"),
        DeleteGroupOutcome::Deleted(_)
    ));
    let err = store
        .create_key(member_key("orphan", &group.id))
        .await
        .expect_err("a deleted group is as unknown as a made-up one");
    assert!(matches!(err, AuthError::UnknownGroup(id) if id == group.id));

    // Refused BEFORE any write: no key row landed.
    assert!(store.list_keys(false).await.expect("list").is_empty());
}

#[tokio::test]
async fn create_key_with_a_valid_group_persists_the_membership() {
    let store = KeyStore::in_memory().await.expect("open store");
    let group = store
        .create_group(new_group("home"))
        .await
        .expect("create group");
    let (plaintext, record) = store
        .create_key(member_key("member", &group.id))
        .await
        .expect("create key");
    assert_eq!(record.group_id, Some(group.id.clone()));

    // The membership rides every read path the gateway uses.
    let fetched = store
        .find_by_hash(&hash_key(plaintext.reveal()))
        .await
        .expect("lookup")
        .expect("key exists");
    assert_eq!(fetched.group_id, Some(group.id.clone()));

    let entries = store.load_auth_entries().await.expect("entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1.group_id, Some(group.id.clone()));
}

#[tokio::test]
async fn key_patch_group_id_is_tri_state_absent_join_and_leave() {
    let store = KeyStore::in_memory().await.expect("open store");
    let g1 = store.create_group(new_group("g1")).await.expect("g1");
    let g2 = store.create_group(new_group("g2")).await.expect("g2");
    let (_, key) = store
        .create_key(member_key("mover", &g1.id))
        .await
        .expect("key");

    // Absent = unchanged: a patch that says nothing about group_id keeps it.
    let unchanged = store
        .update_key(&key.id, KeyPatch::default())
        .await
        .expect("patch")
        .expect("key exists");
    assert_eq!(unchanged.group_id, Some(g1.id.clone()));

    // A string = join that group.
    let moved = store
        .update_key(
            &key.id,
            KeyPatch {
                group_id: Some(Some(g2.id.clone())),
                ..KeyPatch::default()
            },
        )
        .await
        .expect("patch")
        .expect("key exists");
    assert_eq!(moved.group_id, Some(g2.id.clone()));

    // Explicit null = leave the group - the one deliberate exception to the
    // "patches cannot clear to NULL" rule (ADR 009 §6).
    let left = store
        .update_key(
            &key.id,
            KeyPatch {
                group_id: Some(None),
                ..KeyPatch::default()
            },
        )
        .await
        .expect("patch")
        .expect("key exists");
    assert_eq!(left.group_id, None);
}

#[tokio::test]
async fn key_patch_joining_an_unknown_or_deleted_group_is_refused() {
    let store = KeyStore::in_memory().await.expect("open store");
    let home = store.create_group(new_group("home")).await.expect("home");
    let (_, key) = store
        .create_key(member_key("loyal", &home.id))
        .await
        .expect("key");

    let err = store
        .update_key(
            &key.id,
            KeyPatch {
                group_id: Some(Some("nope".to_owned())),
                ..KeyPatch::default()
            },
        )
        .await
        .expect_err("an unknown group must refuse the patch");
    assert!(matches!(err, AuthError::UnknownGroup(id) if id == "nope"));

    let dead = store.create_group(new_group("dead")).await.expect("dead");
    assert!(matches!(
        store.delete_group(&dead.id).await.expect("delete"),
        DeleteGroupOutcome::Deleted(_)
    ));
    let err = store
        .update_key(
            &key.id,
            KeyPatch {
                group_id: Some(Some(dead.id.clone())),
                ..KeyPatch::default()
            },
        )
        .await
        .expect_err("a deleted group is refused too");
    assert!(matches!(err, AuthError::UnknownGroup(id) if id == dead.id));

    // Refused before any write: the membership is untouched.
    let entries = store.load_auth_entries().await.expect("entries");
    assert_eq!(entries[0].1.group_id, Some(home.id.clone()));
}

#[tokio::test]
async fn load_groups_returns_only_active_groups() {
    let store = KeyStore::in_memory().await.expect("open store");
    let live = store.create_group(new_group("live")).await.expect("live");
    let doomed = store
        .create_group(new_group("doomed"))
        .await
        .expect("doomed");
    assert!(matches!(
        store.delete_group(&doomed.id).await.expect("delete"),
        DeleteGroupOutcome::Deleted(_)
    ));

    // What a restarted gateway (or a reload) loads: tombstones excluded.
    let loaded = store.load_groups().await.expect("load");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].id, live.id);
}

#[tokio::test]
async fn persist_group_budgets_survives_reload() {
    // Mirror of `persist_budgets_survives_reload` for keys: the flushed
    // group spend is what a restarted gateway reloads, so an exhausted pool
    // stays exhausted across a restart.
    let store = KeyStore::in_memory().await.expect("open store");
    let record = store
        .create_group(new_group("spender"))
        .await
        .expect("create");

    store
        .persist_group_budgets(&[(record.id.clone(), 12.5)])
        .await
        .expect("flush");

    let reloaded = store.load_groups().await.expect("load");
    assert_eq!(reloaded[0].budget_spent, 12.5);
    let listed = store.list_groups(false).await.expect("list");
    assert_eq!(listed[0].budget_spent, 12.5);
}
