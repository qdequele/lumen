//! The admin listings filter in SQL: `external_ref`, `account_ref`,
//! `parent_id` and the account scope are bound parameters of the query, not
//! an in-memory pass over every row of the table.

use lumen_auth::store::{GroupListFilter, KeyListFilter, KeyStore, NewGroup, NewKey};
use sqlx::Row;

const ACCOUNT_A: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";
const ACCOUNT_B: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b62";

async fn group(
    store: &KeyStore,
    name: &str,
    account: Option<&str>,
    parent: Option<&str>,
) -> String {
    store
        .create_group(NewGroup {
            name: name.to_owned(),
            budget_max: None,
            account_ref: account.map(str::to_owned),
            parent_id: parent.map(str::to_owned),
        })
        .await
        .expect("create group")
        .id
}

async fn key(store: &KeyStore, name: &str, group: Option<&str>, external: Option<&str>) -> String {
    store
        .create_key(NewKey {
            name: name.to_owned(),
            group_id: group.map(str::to_owned),
            external_ref: external.map(str::to_owned),
            ..NewKey::default()
        })
        .await
        .expect("create key")
        .1
        .id
}

async fn key_names(store: &KeyStore, filter: &KeyListFilter) -> Vec<String> {
    let mut names: Vec<String> = store
        .list_keys_filtered(filter)
        .await
        .expect("list keys")
        .into_iter()
        .map(|k| k.name)
        .collect();
    // Rows created in the same second order by their random id.
    names.sort();
    names
}

async fn group_names(store: &KeyStore, filter: &GroupListFilter) -> Vec<String> {
    let mut names: Vec<String> = store
        .list_groups_filtered(filter)
        .await
        .expect("list groups")
        .into_iter()
        .map(|g| g.name)
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn keys_are_filtered_by_external_ref_scope_and_tombstone() {
    let store = KeyStore::in_memory().await.expect("store");
    let ga = group(&store, "ga", Some(ACCOUNT_A), None).await;
    let gb = group(&store, "gb", Some(ACCOUNT_B), None).await;
    key(&store, "a1", Some(&ga), Some("ref-1")).await;
    let a2 = key(&store, "a2", Some(&ga), Some("ref-2")).await;
    key(&store, "b1", Some(&gb), Some("ref-1")).await;
    key(&store, "loose", None, Some("ref-1")).await;
    store
        .delete_key(&a2)
        .await
        .expect("delete")
        .expect("existed");

    // No filter: the old `list_keys` behaviour.
    assert_eq!(
        key_names(&store, &KeyListFilter::default()).await,
        ["a1", "b1", "loose"]
    );
    assert_eq!(
        store.list_keys(false).await.expect("list").len(),
        3,
        "list_keys(false) is the unfiltered listing"
    );
    let with_deleted = KeyListFilter {
        include_deleted: true,
        ..KeyListFilter::default()
    };
    assert_eq!(
        key_names(&store, &with_deleted).await,
        ["a1", "a2", "b1", "loose"]
    );

    let by_ref = KeyListFilter {
        external_ref: Some("ref-1".to_owned()),
        ..KeyListFilter::default()
    };
    assert_eq!(key_names(&store, &by_ref).await, ["a1", "b1", "loose"]);

    // Scoped: only keys whose group belongs to the account; a key with no
    // group is in nobody's scope.
    let scoped = KeyListFilter {
        scope_account_ref: Some(ACCOUNT_A.to_owned()),
        ..KeyListFilter::default()
    };
    assert_eq!(key_names(&store, &scoped).await, ["a1"]);
    let scoped_deleted = KeyListFilter {
        include_deleted: true,
        scope_account_ref: Some(ACCOUNT_A.to_owned()),
        ..KeyListFilter::default()
    };
    assert_eq!(key_names(&store, &scoped_deleted).await, ["a1", "a2"]);
    let scoped_ref = KeyListFilter {
        external_ref: Some("ref-1".to_owned()),
        scope_account_ref: Some(ACCOUNT_B.to_owned()),
        ..KeyListFilter::default()
    };
    assert_eq!(key_names(&store, &scoped_ref).await, ["b1"]);

    // Matching is exact, and a value is data, never SQL.
    let no_match = KeyListFilter {
        external_ref: Some("REF-1".to_owned()),
        ..KeyListFilter::default()
    };
    assert_eq!(key_names(&store, &no_match).await, [] as [String; 0]);
    let hostile = KeyListFilter {
        external_ref: Some("x' OR '1'='1".to_owned()),
        ..KeyListFilter::default()
    };
    assert_eq!(key_names(&store, &hostile).await, [] as [String; 0]);
}

#[tokio::test]
async fn a_scoped_key_listing_keeps_the_keys_of_a_tombstoned_group() {
    // `?include_deleted=true` must stay inside the account: the tombstones of
    // a deleted group's keys still belong to it.
    let store = KeyStore::in_memory().await.expect("store");
    let ga = group(&store, "ga", Some(ACCOUNT_A), None).await;
    let k = key(&store, "a1", Some(&ga), None).await;
    store.delete_key(&k).await.expect("delete key");
    store.delete_group(&ga).await.expect("delete group");

    let scoped_deleted = KeyListFilter {
        include_deleted: true,
        scope_account_ref: Some(ACCOUNT_A.to_owned()),
        ..KeyListFilter::default()
    };
    assert_eq!(key_names(&store, &scoped_deleted).await, ["a1"]);
}

#[tokio::test]
async fn groups_are_filtered_by_account_parent_scope_and_tombstone() {
    let store = KeyStore::in_memory().await.expect("store");
    let ga = group(&store, "ga", Some(ACCOUNT_A), None).await;
    group(&store, "ga-sub", None, Some(&ga)).await;
    group(&store, "gb", Some(ACCOUNT_B), None).await;
    let gone = group(&store, "gone", Some(ACCOUNT_A), None).await;
    group(&store, "plain", None, None).await;
    store.delete_group(&gone).await.expect("delete group");

    assert_eq!(
        group_names(&store, &GroupListFilter::default()).await,
        ["ga", "ga-sub", "gb", "plain"]
    );
    assert_eq!(store.list_groups(false).await.expect("list").len(), 4);
    assert_eq!(store.list_groups(true).await.expect("list").len(), 5);

    let by_account = GroupListFilter {
        account_ref: Some(ACCOUNT_A.to_owned()),
        ..GroupListFilter::default()
    };
    // The sub-group carries its parent's account.
    assert_eq!(group_names(&store, &by_account).await, ["ga", "ga-sub"]);
    let by_account_deleted = GroupListFilter {
        include_deleted: true,
        account_ref: Some(ACCOUNT_A.to_owned()),
        ..GroupListFilter::default()
    };
    assert_eq!(
        group_names(&store, &by_account_deleted).await,
        ["ga", "ga-sub", "gone"]
    );

    let by_parent = GroupListFilter {
        parent_id: Some(ga.clone()),
        ..GroupListFilter::default()
    };
    assert_eq!(group_names(&store, &by_parent).await, ["ga-sub"]);

    // The scope applies on top of the query filter: asking for another
    // account narrows to nothing, it never widens the scope.
    let scoped = GroupListFilter {
        scope_account_ref: Some(ACCOUNT_B.to_owned()),
        ..GroupListFilter::default()
    };
    assert_eq!(group_names(&store, &scoped).await, ["gb"]);
    let scoped_other = GroupListFilter {
        account_ref: Some(ACCOUNT_A.to_owned()),
        scope_account_ref: Some(ACCOUNT_B.to_owned()),
        ..GroupListFilter::default()
    };
    assert_eq!(group_names(&store, &scoped_other).await, [] as [String; 0]);
}

/// The query plan of `sql`, with `binds` text parameters bound.
async fn plan(store: &KeyStore, sql: &str, binds: usize) -> String {
    let mut query = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")));
    for _ in 0..binds {
        query = query.bind("x");
    }
    query
        .fetch_all(store.pool())
        .await
        .expect("explain")
        .iter()
        .map(|row| row.get::<String, _>("detail"))
        .collect::<Vec<_>>()
        .join(" | ")
}

#[tokio::test]
async fn the_filtered_listings_search_their_indexes() {
    // The SQL the store builds for each filter (`KeyListFilter::sql`,
    // `GroupListFilter::sql`), so the plan checked is the plan that runs.
    let store = KeyStore::in_memory().await.expect("store");

    let by_ref = KeyListFilter {
        external_ref: Some("x".to_owned()),
        ..KeyListFilter::default()
    };
    let detail = plan(&store, &by_ref.sql(), 1).await;
    assert!(
        detail.contains("USING INDEX idx_virtual_keys_external_ref (external_ref=?)"),
        "keys by external_ref: {detail}"
    );

    let by_account = GroupListFilter {
        account_ref: Some("x".to_owned()),
        ..GroupListFilter::default()
    };
    let detail = plan(&store, &by_account.sql(), 1).await;
    assert!(
        detail.contains("USING INDEX idx_budget_groups_account_ref (account_ref=?)"),
        "groups by account_ref: {detail}"
    );

    let by_parent = GroupListFilter {
        parent_id: Some("x".to_owned()),
        ..GroupListFilter::default()
    };
    let detail = plan(&store, &by_parent.sql(), 1).await;
    assert!(
        detail.contains("USING INDEX idx_budget_groups_parent (parent_id=?)"),
        "groups by parent_id: {detail}"
    );

    // The scope resolves the account's groups through the account index.
    let scoped = KeyListFilter {
        scope_account_ref: Some("x".to_owned()),
        ..KeyListFilter::default()
    };
    let detail = plan(&store, &scoped.sql(), 1).await;
    assert!(
        detail.contains("idx_budget_groups_account_ref (account_ref=?)"),
        "scoped keys: {detail}"
    );
}
