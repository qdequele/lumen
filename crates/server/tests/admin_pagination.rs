//! Cursor pagination of `GET /admin/keys` and `GET /admin/groups` (ADR 023):
//! strictly additive. Without `limit` or `cursor` the response is the full
//! bare array with no extra header; with them, at most `limit` rows and an
//! `X-Next-Cursor` header while more remain.

mod common;

use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::KeyStore;
use lumen_server::auth::AuthRuntime;
use lumen_server::state::AppState;
use lumen_telemetry::{LatencyMetrics, Metrics, TokenMetrics};
use serde_json::{json, Value};
use std::sync::Arc;

const MASTER: &str = "admin-master-token";
const ACCOUNT_A: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";
const ACCOUNT_B: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b62";

struct Harness {
    base: String,
    client: reqwest::Client,
}

async fn spawn() -> Harness {
    let runtime = Arc::new(AuthRuntime {
        keys: AuthState::load(Vec::new(), Vec::new()),
        store: KeyStore::in_memory().await.unwrap(),
        admin_token_hash: hash_key(MASTER),
        master: None,
    });
    let metrics = Metrics::new();
    let tokens = TokenMetrics::register(&metrics, &[]).unwrap();
    let latency = LatencyMetrics::register(&metrics).unwrap();
    let state =
        AppState::new(metrics, common::empty_registry(), tokens, latency).with_auth(runtime);
    Harness {
        base: common::spawn_state(state, 1 << 20).await,
        client: reqwest::Client::new(),
    }
}

impl Harness {
    async fn post(&self, path: &str, body: &Value) -> Value {
        let resp = self
            .client
            .post(format!("{}{path}", self.base))
            .bearer_auth(MASTER)
            .json(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201, "{path}");
        resp.json().await.unwrap()
    }

    async fn get(&self, path_and_query: &str, account: Option<&str>) -> reqwest::Response {
        let mut req = self
            .client
            .get(format!("{}{path_and_query}", self.base))
            .bearer_auth(MASTER);
        if let Some(account) = account {
            req = req.header("X-Lumen-Account-Ref", account);
        }
        req.send().await.unwrap()
    }

    /// The ids of the unpaginated listing at `path_and_query`.
    async fn all(&self, path_and_query: &str, account: Option<&str>) -> Vec<String> {
        let resp = self.get(path_and_query, account).await;
        assert_eq!(resp.status(), 200, "{path_and_query}");
        ids(&resp.json().await.unwrap())
    }

    /// Walk `path` (which may already carry a query) `limit` rows at a time;
    /// returns the ids in the order served and the number of pages.
    async fn walk(&self, path: &str, limit: u32, account: Option<&str>) -> (Vec<String>, usize) {
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut walked = Vec::new();
        let mut pages = 0;
        let mut cursor: Option<String> = None;
        loop {
            let mut url = format!("{path}{sep}limit={limit}");
            if let Some(cursor) = &cursor {
                url.push_str("&cursor=");
                url.push_str(cursor);
            }
            let resp = self.get(&url, account).await;
            assert_eq!(resp.status(), 200, "{url}");
            cursor = next_cursor(&resp);
            let rows: Value = resp.json().await.unwrap();
            let page = ids(&rows);
            assert!(page.len() <= limit as usize, "{url}");
            pages += 1;
            walked.extend(page);
            if cursor.is_none() {
                return (walked, pages);
            }
        }
    }
}

fn next_cursor(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get("x-next-cursor")
        .map(|v| v.to_str().unwrap().to_owned())
}

fn ids(rows: &Value) -> Vec<String> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn without_limit_or_cursor_the_listings_are_unchanged() {
    let h = spawn().await;
    for i in 0..3 {
        h.post("/admin/keys", &json!({ "name": format!("k{i}") }))
            .await;
        h.post("/admin/groups", &json!({ "name": format!("g{i}") }))
            .await;
    }
    for path in ["/admin/keys", "/admin/groups"] {
        let resp = h.get(path, None).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(next_cursor(&resp), None, "{path}: no cursor header");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body.as_array().unwrap().len(), 3, "{path}: a bare array");
    }
}

#[tokio::test]
async fn key_pages_walk_the_whole_listing_in_its_own_order() {
    let h = spawn().await;
    for i in 0..5 {
        h.post("/admin/keys", &json!({ "name": format!("k{i}") }))
            .await;
    }
    let all = h.all("/admin/keys", None).await;
    assert_eq!(all.len(), 5);

    let (walked, pages) = h.walk("/admin/keys", 2, None).await;
    assert_eq!(walked, all, "pages concatenate to the unpaginated listing");
    assert_eq!(pages, 3);

    // The body of a page is still a bare array of key records.
    let resp = h.get("/admin/keys?limit=2", None).await;
    assert!(next_cursor(&resp).is_some());
    let page: Value = resp.json().await.unwrap();
    assert_eq!(page.as_array().unwrap().len(), 2);
    assert!(page[0]["name"].is_string());

    // A page that ends the listing carries no cursor.
    let resp = h.get("/admin/keys?limit=5", None).await;
    assert_eq!(next_cursor(&resp), None);
    let page: Value = resp.json().await.unwrap();
    assert_eq!(page.as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn group_pages_walk_the_whole_listing_and_keep_the_filters() {
    let h = spawn().await;
    for i in 0..5 {
        let account = if i % 2 == 0 { ACCOUNT_A } else { ACCOUNT_B };
        h.post(
            "/admin/groups",
            &json!({ "name": format!("g{i}"), "account_ref": account }),
        )
        .await;
    }
    let all = h.all("/admin/groups", None).await;
    let (walked, pages) = h.walk("/admin/groups", 2, None).await;
    assert_eq!(walked, all);
    assert_eq!(pages, 3);

    let query = format!("/admin/groups?account_ref={ACCOUNT_A}");
    let filtered = h.all(&query, None).await;
    assert_eq!(filtered.len(), 3);
    assert_eq!(h.walk(&query, 2, None).await.0, filtered);

    // The account scope holds on every page.
    let scoped = h.all("/admin/groups", Some(ACCOUNT_B)).await;
    assert_eq!(scoped.len(), 2);
    assert_eq!(h.walk("/admin/groups", 1, Some(ACCOUNT_B)).await.0, scoped);
}

#[tokio::test]
async fn scoped_key_pages_stay_inside_the_account() {
    let h = spawn().await;
    let group = h
        .post(
            "/admin/groups",
            &json!({ "name": "ga", "account_ref": ACCOUNT_A }),
        )
        .await;
    let ga = group["id"].as_str().unwrap();
    for i in 0..3 {
        h.post(
            "/admin/keys",
            &json!({ "name": format!("a{i}"), "group_id": ga }),
        )
        .await;
        h.post("/admin/keys", &json!({ "name": format!("loose{i}") }))
            .await;
    }
    let scoped = h.all("/admin/keys", Some(ACCOUNT_A)).await;
    assert_eq!(scoped.len(), 3);
    let (walked, pages) = h.walk("/admin/keys", 2, Some(ACCOUNT_A)).await;
    assert_eq!(walked, scoped);
    assert_eq!(pages, 2);
}

#[tokio::test]
async fn a_cursor_alone_uses_the_default_page_size() {
    let h = spawn().await;
    for i in 0..3 {
        h.post("/admin/keys", &json!({ "name": format!("k{i}") }))
            .await;
    }
    let first = h.get("/admin/keys?limit=1", None).await;
    let cursor = next_cursor(&first).unwrap();
    let rest = h.get(&format!("/admin/keys?cursor={cursor}"), None).await;
    assert_eq!(rest.status(), 200);
    assert_eq!(next_cursor(&rest), None);
    let rows: Value = rest.json().await.unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn invalid_limits_and_cursors_are_lm_1001() {
    let h = spawn().await;
    h.post("/admin/keys", &json!({ "name": "k" })).await;
    for path in ["/admin/keys", "/admin/groups"] {
        for query in [
            "limit=0",
            "limit=1001",
            "limit=-1",
            "limit=abc",
            "limit=2&cursor=zz",
            // Valid hex, but not a cursor ("nope").
            "limit=2&cursor=6e6f7065",
            "cursor=",
        ] {
            let resp = h.get(&format!("{path}?{query}"), None).await;
            assert_eq!(resp.status(), 400, "{path}?{query}");
            let body: Value = resp.json().await.unwrap();
            assert_eq!(body["error"]["code"], "LM-1001", "{path}?{query}");
        }
        // The cap itself is accepted.
        let resp = h.get(&format!("{path}?limit=1000"), None).await;
        assert_eq!(resp.status(), 200, "{path}");
    }
}
