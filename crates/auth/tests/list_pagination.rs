//! Keyset pagination of the admin listings (ADR 023): pages follow the
//! `(created_at, id)` order of the unpaginated listing, a cursor never skips
//! or repeats a row, and the walk rides an index instead of a sort.

use lumen_auth::store::{
    GroupListFilter, KeyListFilter, KeyStore, ListCursor, NewGroup, NewKey, PageRequest,
};
use sqlx::Row;

const ACCOUNT_A: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";

async fn group(store: &KeyStore, name: &str, account: Option<&str>) -> String {
    store
        .create_group(NewGroup {
            name: name.to_owned(),
            budget_max: None,
            account_ref: account.map(str::to_owned),
            parent_id: None,
        })
        .await
        .expect("create group")
        .id
}

async fn key(store: &KeyStore, name: &str, group: Option<&str>) -> String {
    store
        .create_key(NewKey {
            name: name.to_owned(),
            group_id: group.map(str::to_owned),
            ..NewKey::default()
        })
        .await
        .expect("create key")
        .1
        .id
}

/// Every key id of `filter`, walked `limit` rows at a time.
async fn walk_keys(store: &KeyStore, filter: &KeyListFilter, limit: u32) -> Vec<String> {
    let mut ids = Vec::new();
    let mut after = None;
    loop {
        let page = store
            .list_keys_page(filter, &PageRequest { limit, after })
            .await
            .expect("page");
        assert!(page.rows.len() <= limit as usize, "a page never overflows");
        ids.extend(page.rows.into_iter().map(|k| k.id));
        match page.next {
            Some(next) => after = Some(next),
            None => return ids,
        }
    }
}

#[tokio::test]
async fn key_pages_concatenate_to_the_unpaginated_listing() {
    let store = KeyStore::in_memory().await.expect("store");
    for i in 0..5 {
        key(&store, &format!("k{i}"), None).await;
    }
    // Spread two rows over other seconds so both keyset columns matter.
    sqlx::query("UPDATE virtual_keys SET created_at = created_at - 10 WHERE name = 'k3'")
        .execute(store.pool())
        .await
        .expect("backdate");
    sqlx::query("UPDATE virtual_keys SET created_at = created_at + 10 WHERE name = 'k0'")
        .execute(store.pool())
        .await
        .expect("postdate");

    let filter = KeyListFilter::default();
    let all: Vec<String> = store
        .list_keys_filtered(&filter)
        .await
        .expect("list")
        .into_iter()
        .map(|k| k.id)
        .collect();
    assert_eq!(all.len(), 5);

    for limit in [1, 2, 3, 5, 6] {
        assert_eq!(
            walk_keys(&store, &filter, limit).await,
            all,
            "limit {limit}"
        );
    }

    // A page that holds exactly the last rows carries no cursor: no empty
    // trailing page.
    let exact = store
        .list_keys_page(
            &filter,
            &PageRequest {
                limit: 5,
                after: None,
            },
        )
        .await
        .expect("page");
    assert_eq!(exact.rows.len(), 5);
    assert_eq!(exact.next, None);

    // A full page with more behind it names its own last row.
    let first = store
        .list_keys_page(
            &filter,
            &PageRequest {
                limit: 2,
                after: None,
            },
        )
        .await
        .expect("page");
    let last = first.rows.last().expect("two rows");
    assert_eq!(
        first.next,
        Some(ListCursor {
            created_at: last.created_at,
            id: last.id.clone(),
        })
    );
}

#[tokio::test]
async fn key_pages_keep_the_filter_and_the_scope() {
    let store = KeyStore::in_memory().await.expect("store");
    let ga = group(&store, "ga", Some(ACCOUNT_A)).await;
    let mut mine = Vec::new();
    for i in 0..3 {
        mine.push(key(&store, &format!("a{i}"), Some(&ga)).await);
        key(&store, &format!("loose{i}"), None).await;
    }
    let gone = key(&store, "gone", Some(&ga)).await;
    store.delete_key(&gone).await.expect("delete");
    mine.sort();

    let scoped = KeyListFilter {
        scope_account_ref: Some(ACCOUNT_A.to_owned()),
        ..KeyListFilter::default()
    };
    let mut walked = walk_keys(&store, &scoped, 2).await;
    walked.sort();
    assert_eq!(walked, mine, "only the account's live keys, across pages");

    let scoped_deleted = KeyListFilter {
        include_deleted: true,
        ..scoped
    };
    assert_eq!(walk_keys(&store, &scoped_deleted, 2).await.len(), 4);
}

#[tokio::test]
async fn a_row_created_during_a_walk_is_neither_skipped_nor_repeated() {
    let store = KeyStore::in_memory().await.expect("store");
    for i in 0..4 {
        key(&store, &format!("k{i}"), None).await;
    }
    let filter = KeyListFilter::default();
    let first = store
        .list_keys_page(
            &filter,
            &PageRequest {
                limit: 2,
                after: None,
            },
        )
        .await
        .expect("page");
    // A later key (a later second, so it sorts after the cursor) and the
    // deletion of an already-returned row, both between two pages.
    let late = key(&store, "late", None).await;
    sqlx::query("UPDATE virtual_keys SET created_at = created_at + 60 WHERE id = ?")
        .bind(&late)
        .execute(store.pool())
        .await
        .expect("postdate");
    store
        .delete_key(&first.rows[0].id)
        .await
        .expect("delete")
        .expect("existed");

    let rest = store
        .list_keys_page(
            &filter,
            &PageRequest {
                limit: 10,
                after: first.next.clone(),
            },
        )
        .await
        .expect("page");
    let ids: Vec<&str> = rest.rows.iter().map(|k| k.id.as_str()).collect();
    assert_eq!(ids.len(), 3, "the two remaining keys and the new one");
    assert_eq!(ids.last().copied(), Some(late.as_str()));
    assert!(first.rows.iter().all(|k| !ids.contains(&k.id.as_str())));
    assert_eq!(rest.next, None);
}

#[tokio::test]
async fn group_pages_concatenate_to_the_unpaginated_listing() {
    let store = KeyStore::in_memory().await.expect("store");
    for i in 0..5 {
        let account = (i % 2 == 0).then_some(ACCOUNT_A);
        group(&store, &format!("g{i}"), account).await;
    }
    for filter in [
        GroupListFilter::default(),
        GroupListFilter {
            account_ref: Some(ACCOUNT_A.to_owned()),
            ..GroupListFilter::default()
        },
    ] {
        let all: Vec<String> = store
            .list_groups_filtered(&filter)
            .await
            .expect("list")
            .into_iter()
            .map(|g| g.id)
            .collect();
        let mut walked = Vec::new();
        let mut after = None;
        loop {
            let page = store
                .list_groups_page(&filter, &PageRequest { limit: 2, after })
                .await
                .expect("page");
            walked.extend(page.rows.into_iter().map(|g| g.id));
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        assert_eq!(walked, all, "{filter:?}");
    }
}

/// The query plan of `sql` with `texts` text binds, then the keyset pair and
/// the limit.
async fn plan(store: &KeyStore, sql: &str, texts: usize) -> String {
    let mut query = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")));
    for _ in 0..texts {
        query = query.bind("x");
    }
    query
        .bind(0_i64)
        .bind("x")
        .bind(3_i64)
        .fetch_all(store.pool())
        .await
        .expect("explain")
        .iter()
        .map(|row| row.get::<String, _>("detail"))
        .collect::<Vec<_>>()
        .join(" | ")
}

#[tokio::test]
async fn an_unfiltered_page_walks_the_created_index_without_sorting() {
    let store = KeyStore::in_memory().await.expect("store");

    let detail = plan(&store, &KeyListFilter::default().page_sql(true), 0).await;
    assert!(
        detail.contains("idx_virtual_keys_created"),
        "key page: {detail}"
    );
    assert!(!detail.contains("TEMP B-TREE"), "key page: {detail}");

    let detail = plan(&store, &GroupListFilter::default().page_sql(true), 0).await;
    assert!(
        detail.contains("idx_budget_groups_created"),
        "group page: {detail}"
    );
    assert!(!detail.contains("TEMP B-TREE"), "group page: {detail}");
}
