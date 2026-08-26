//! End-to-end test for M7 hot reload: rotating a DB-stored provider key
//! (`PUT /admin/provider-keys`) is picked up by a reload and the very next
//! upstream request authenticates with the new key - no restart. The upstream
//! is wiremock; LUMEN sits in front and we inspect the `Authorization` header
//! the provider actually sent.

mod common;

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use lumen_auth::crypto::MasterKey;
use lumen_auth::store::KeyStore;
use lumen_providers::{http, Registry};
use lumen_server::config::Config;
use lumen_server::pricing::CostTable;
use lumen_server::reload::{reload_once, spawn_config_reloader, ProviderKeySource, ReloadTargets};
use lumen_server::resilience::ResilienceRuntime;
use lumen_telemetry::{Metrics, ReloadMetrics};
use serde_json::json;
use tokio::sync::Notify;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;

/// The master-key value (64 hex chars) sealing provider keys in these tests.
fn master() -> MasterKey {
    MasterKey::from_env_value(&"a".repeat(64)).expect("master key")
}

/// A unique temp dir under the OS temp root (no external crate).
fn tempdir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("lumen-reload-it-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Write a cohere-only config pointing at `upstream`, and return its path.
fn write_config(dir: &Path, upstream: &str) -> PathBuf {
    let body = format!(
        r#"
        [[providers]]
        name = "cohere"
        kind = "cohere"
        base_url = "{upstream}"
        [[providers.models]]
        id = "rr"
        upstream_id = "rerank-v3.5"
        capabilities = ["rerank"]
        "#
    );
    let path = dir.join("config.toml");
    let mut file = std::fs::File::create(&path).expect("create config");
    file.write_all(body.as_bytes()).expect("write config");
    path
}

/// Build the registry from `path`'s specs, merging a single provider key in
/// (mirroring the boot DB back-fill for an env-keyless provider).
fn registry_with_key(path: &Path, key: &str) -> Arc<Registry> {
    let config = Config::load(path).expect("config loads");
    let mut specs = config.provider_specs();
    for spec in &mut specs {
        if spec.name == "cohere" {
            spec.api_key = Some(key.to_owned());
        }
    }
    Arc::new(
        Registry::build(
            specs,
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
    )
}

async fn mount_rerank(upstream: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v2/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{ "index": 0, "relevance_score": 0.9 }],
            "meta": { "billed_units": { "search_units": 1 } }
        })))
        .mount(upstream)
        .await;
}

/// The `Authorization` header of the most recent upstream request.
async fn last_upstream_auth(upstream: &MockServer) -> String {
    let reqs = upstream
        .received_requests()
        .await
        .expect("received requests");
    let last = reqs.last().expect("at least one upstream request");
    last.headers
        .get("authorization")
        .expect("authorization header present")
        .to_str()
        .expect("header is ascii")
        .to_owned()
}

async fn send_rerank(base: &str, client: &reqwest::Client) {
    let resp = client
        .post(format!("{base}/v1/rerank"))
        .json(&json!({ "model": "rr", "query": "q", "documents": ["a"] }))
        .send()
        .await
        .expect("send rerank");
    assert_eq!(resp.status(), 200, "rerank succeeds");
}

#[tokio::test]
async fn rotating_a_db_provider_key_takes_effect_on_reload_without_restart() {
    let upstream = MockServer::start().await;
    mount_rerank(&upstream).await;
    let dir = tempdir();
    let path = write_config(&dir, &upstream.uri());

    // Seed the encrypted store with the ORIGINAL provider key.
    let store = KeyStore::in_memory().await.expect("store");
    store
        .store_provider_key("cohere", "old-key", &master())
        .await
        .expect("store old key");

    // Boot the gateway with the original key back-filled into the registry.
    let registry = registry_with_key(&path, "old-key");
    let state = common::base_state(Arc::clone(&registry));
    let base = common::spawn_state(state, LIMIT).await;
    let client = reqwest::Client::new();

    // First request goes out under the original key.
    send_rerank(&base, &client).await;
    assert_eq!(
        last_upstream_auth(&upstream).await,
        "Bearer old-key",
        "the boot key authenticates the first request"
    );

    // Rotate the key in the DB (as `PUT /admin/provider-keys` would), then run
    // exactly one reload through the real reloader entry point.
    store
        .store_provider_key("cohere", "new-key", &master())
        .await
        .expect("rotate key");
    let targets = Arc::new(ReloadTargets {
        registry: Arc::clone(&registry),
        pricing: Arc::new(ArcSwap::from_pointee(CostTable::default())),
        resilience: Arc::new(ResilienceRuntime::defaults()),
        metrics: ReloadMetrics::register(&Metrics::new()).expect("reload metrics"),
        key_backfill: Arc::new(ArcSwap::from_pointee(single("cohere", "old-key"))),
        key_source: Some(Arc::new(ProviderKeySource::new(
            store.clone(),
            master(),
            vec!["cohere".to_owned()],
        ))),
        auth_knobs: None,
        webhooks: None,
        auth_runtime: None,
    });
    reload_once(&path, &targets).await;

    // The next request - through the SAME running gateway - uses the new key.
    send_rerank(&base, &client).await;
    assert_eq!(
        last_upstream_auth(&upstream).await,
        "Bearer new-key",
        "the rotated DB key authenticates requests after a reload, no restart"
    );
}

fn single(k: &str, v: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert(k.to_owned(), v.to_owned());
    m
}

#[tokio::test]
async fn reload_makes_an_offline_group_and_member_key_live_and_group_enforced() {
    // ADR 009 §4: a hot reload re-reads groups from the DB BEFORE keys, so a
    // group and its member key provisioned straight against the DB (offline,
    // as `lumen keys create` does) become live - and group-ENFORCED - after
    // one reload, no restart. Mirrors the offline-key reload test.
    use lumen_auth::key::hash_key;
    use lumen_auth::state::{usd_to_micro, AuthState};
    use lumen_auth::store::{NewGroup, NewKey};
    use lumen_core::{BudgetScope, GatewayError};
    use lumen_server::auth::AuthRuntime;

    let upstream = MockServer::start().await;
    let dir = tempdir();
    let path = write_config(&dir, &upstream.uri());

    // A live auth runtime whose in-memory tables were loaded at "boot",
    // before the offline group and key existed.
    let store = KeyStore::in_memory().await.expect("store");
    let runtime = Arc::new(AuthRuntime {
        keys: AuthState::load(Vec::new(), Vec::new()),
        store: store.clone(),
        admin_token_hash: hash_key("admin"),
        master: None,
    });

    // Offline provisioning straight against the DB: a $5 pool and one
    // member key, neither of which the live tables have ever seen.
    let group = store
        .create_group(NewGroup {
            name: "offline-pool".to_owned(),
            budget_max: Some(5.0),
        })
        .await
        .expect("create group offline");
    let (plaintext, _record) = store
        .create_key(NewKey {
            name: "offline-member".to_owned(),
            group_id: Some(group.id.clone()),
            ..NewKey::default()
        })
        .await
        .expect("create key offline");
    assert!(
        runtime.keys.authenticate(plaintext.reveal(), 0).is_none(),
        "the offline-created key must not be live before the reload"
    );

    let targets = Arc::new(ReloadTargets {
        registry: registry_with_key(&path, "irrelevant-key"),
        pricing: Arc::new(ArcSwap::from_pointee(CostTable::default())),
        resilience: Arc::new(ResilienceRuntime::defaults()),
        metrics: ReloadMetrics::register(&Metrics::new()).expect("reload metrics"),
        key_backfill: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        key_source: None,
        auth_knobs: None,
        webhooks: None,
        auth_runtime: Some(Arc::clone(&runtime)),
    });
    reload_once(&path, &targets).await;

    // Live now...
    let entry = runtime
        .keys
        .authenticate(plaintext.reveal(), 0)
        .expect("the offline-created key is live after the reload");
    // ...and enforced against its (also freshly loaded) group: a $10
    // estimate blows the $5 pool - group-scoped, since the key itself has
    // no budget of its own.
    assert!(matches!(
        entry.admit(0, 1, usd_to_micro(10.0)),
        Err(GatewayError::BudgetExceeded {
            scope: BudgetScope::Group
        })
    ));
    // A $3 estimate under the pool is admitted: the key is usable.
    assert!(entry.admit(0, 1, usd_to_micro(3.0)).is_ok());
}

const ONE_MODEL_CHAT: &str = r#"
    [[providers]]
    name = "openai"
    kind = "openai"
    [[providers.models]]
    id = "gpt"
    capabilities = ["chat"]
"#;

const TWO_MODELS_CHAT_EMBED: &str = r#"
    [[providers]]
    name = "openai"
    kind = "openai"
    [[providers.models]]
    id = "gpt"
    capabilities = ["chat"]
    [[providers.models]]
    id = "embed"
    capabilities = ["embed"]
"#;

fn registry_from_chat_config(path: &Path) -> Arc<Registry> {
    let config = Config::load(path).expect("initial config valid");
    Arc::new(
        Registry::build(
            config.provider_specs(),
            http::build_client(),
            Duration::from_secs(300),
        )
        .expect("registry"),
    )
}

/// Reload targets sharing `registry`/`metrics`, with default pricing and
/// resilience, no key backfill and no auth knobs.
fn reload_targets(registry: Arc<Registry>, metrics: ReloadMetrics) -> ReloadTargets {
    ReloadTargets {
        registry,
        pricing: Arc::new(ArcSwap::from_pointee(CostTable::default())),
        resilience: Arc::new(ResilienceRuntime::defaults()),
        metrics,
        key_backfill: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        key_source: None,
        auth_knobs: None,
        webhooks: None,
        auth_runtime: None,
    }
}

/// Regression test: a config path with NO directory component used to make
/// `spawn_config_reloader` watch the FILE's own inode rather than its
/// containing directory. Replacing the file via rename - exactly what
/// `PUT /admin/config` and any GitOps sync do - unlinks that inode, silently
/// ending the watch with no error anywhere; only a directory watch (filtered
/// to the config's own file name) survives a rename.
///
/// This lives in its own process (an integration test under
/// `crates/server/tests/`), not in the `lumen_server` lib's unit test
/// binary, specifically BECAUSE it calls `std::env::set_current_dir` and
/// holds a foreign working directory for the better part of a second: two
/// unit tests in `crates/server/src/config.rs`
/// (`env_var_overrides_file_value`,
/// `master_key_env_var_is_never_folded_into_the_config`) call
/// `Config::load(Path::new("config.toml"))` inside a `figment::Jail`, and
/// `Jail` chdirs into its own temp directory internally while serialising
/// only against OTHER jails via its own private static lock - it has no way
/// to know about a chdir happening outside of it. Sharing a test binary (and
/// therefore a process and a CWD) with those tests would make this test's
/// chdir liable to land in the middle of a jail test's relative-path load,
/// and would make this test's CWD-restoring guard liable to capture a
/// jail's temp directory as "the original CWD" and later restore the
/// process into a directory that has since been deleted. Running as a
/// separate binary (Cargo gives every file under `tests/` its own process)
/// makes that interference structurally impossible instead of relying on a
/// lock everyone remembers to take.
///
/// Note: this cannot discriminate old from new code on macOS, because
/// FSEvents does not reproduce the inode-unlink-on-rename behaviour that
/// inotify does; it is a real regression guard only under inotify (Linux).
/// That platform gap is known and accepted, not something to fix here.
#[tokio::test]
async fn spawn_config_reloader_survives_a_rename_replace_of_a_bare_filename_config() {
    // Restores the original CWD on drop (including on panic/early return),
    // so this test cannot leave the process's working directory changed for
    // whatever runs after it in this binary.
    struct RestoreCwd(PathBuf);
    impl Drop for RestoreCwd {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    let dir = tempdir();
    let config_path = dir.join("lumen.toml");
    std::fs::write(&config_path, ONE_MODEL_CHAT).expect("write config");
    let registry = registry_from_chat_config(&config_path);

    // A BARE filename (no directory component) is passed to
    // `spawn_config_reloader` below - the exact shape that triggered the bug
    // - and it, like the reload path in general, re-resolves that relative
    // path against the process CWD on every single reload, not just once at
    // startup. The CWD must therefore stay pointed at `dir` for this whole
    // test, not just while arming the watcher; a real gateway process never
    // changes its CWD after boot, so this is a property of the test rig,
    // not of the code under test.
    let _restore = RestoreCwd(std::env::current_dir().expect("read cwd"));
    std::env::set_current_dir(&dir).expect("chdir into tempdir");

    let metrics = ReloadMetrics::register(&Metrics::new()).expect("reload metrics");
    let t = reload_targets(Arc::clone(&registry), metrics);
    let trigger = Arc::new(Notify::new());
    let handle =
        spawn_config_reloader(PathBuf::from("lumen.toml"), t, trigger).expect("spawn reloader");

    // Give the watcher a moment to be fully armed before the replace.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Replace the file via RENAME, not an in-place write: the operation
    // that unlinks a file-level watch.
    let staged = dir.join("lumen.toml.staged");
    std::fs::write(&staged, TWO_MODELS_CHAT_EMBED).expect("write staged");
    std::fs::rename(&staged, &config_path).expect("rename into place");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if registry.embedding_route("embed").is_some() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "hot reload did not fire after a rename-replace of a bare-filename config"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    handle.abort();
}

/// A cohere config with auth on, and an optional `[webhooks]` block appended.
/// Used by the webhook reload test to rewrite the very file the reloader reads.
fn webhook_config_body(upstream: &str, db: &Path, webhooks: Option<&str>) -> String {
    format!(
        r#"
        [auth]
        enabled = true
        db_path = "{db}"

        [[providers]]
        name = "cohere"
        kind = "cohere"
        base_url = "{upstream}"
        [[providers.models]]
        id = "rr"
        upstream_id = "rerank-v3.5"
        capabilities = ["rerank"]
        {block}
        "#,
        db = db.display(),
        block = webhooks.unwrap_or_default()
    )
}

/// Open `db`, create one $100 key, and build the live auth runtime over it.
/// Returns the runtime and the key's one-time plaintext.
async fn webhook_auth_runtime(
    db: &Path,
) -> (
    Arc<lumen_server::auth::AuthRuntime>,
    lumen_auth::key::PlaintextKey,
) {
    use lumen_auth::key::hash_key;
    use lumen_auth::state::AuthState;
    use lumen_auth::store::NewKey;
    use lumen_server::auth::AuthRuntime;

    let store = KeyStore::connect(&format!("sqlite://{}?mode=rwc", db.display()))
        .await
        .expect("open store");
    let (plaintext, _record) = store
        .create_key(NewKey {
            name: "prepaid".to_owned(),
            budget_max: Some(100.0),
            ..NewKey::default()
        })
        .await
        .expect("create key");
    let entries = store.load_auth_entries().await.expect("load entries");
    let runtime = Arc::new(AuthRuntime {
        keys: AuthState::load(Vec::new(), entries),
        store,
        admin_token_hash: hash_key(&"a".repeat(64)),
        master: Some(master()),
    });
    (runtime, plaintext)
}

#[tokio::test]
async fn reload_retargets_and_retunes_webhooks_without_dropping_the_queue() {
    // ADR 011 §4: a reload swaps the receiver URL and the event/threshold set
    // through the SAME bounded queue and the same sender task, so a retarget
    // never loses what is already queued. Removing the block stops detection.
    use lumen_auth::events::EventKind;
    use lumen_auth::state::usd_to_micro;
    use lumen_server::webhooks::WebhookController;
    use tokio_util::sync::CancellationToken;

    let upstream = MockServer::start().await;
    let dir = tempdir();
    let db = dir.join("auth.db");
    let path = dir.join("config.toml");

    let first = "https://first.example/events";
    let second = "https://second.example/events";
    let write = |body: &str| {
        let mut file = std::fs::File::create(&path).expect("write config");
        file.write_all(body.as_bytes()).expect("write config body");
    };
    write(&webhook_config_body(
        &upstream.uri(),
        &db,
        Some(&format!(
            r#"
            [webhooks]
            url = "{first}"
            events = ["budget.threshold"]
            thresholds = [50]
            "#
        )),
    ));

    // A live auth runtime with one $100 key, and the webhook stack over it.
    let (runtime, plaintext) = webhook_auth_runtime(&db).await;

    let boot_config = Config::load(&path).expect("boot config loads");
    let webhooks = Arc::new(WebhookController::new(
        Metrics::new(),
        lumen_providers::http::build_client(),
        CancellationToken::new(),
    ));
    webhooks
        .refresh_from_store(&runtime.store, runtime.master.as_ref())
        .await;
    let source = webhooks
        .resolve_and_apply(boot_config.webhooks.as_ref(), &runtime.keys)
        .expect("the boot block applies");
    assert_eq!(source, lumen_auth::events::SettingsSource::Config);
    assert_eq!(
        webhooks.live_settings().expect("enabled").url,
        first,
        "the file block is in force with no stored row"
    );

    let targets = Arc::new(ReloadTargets {
        registry: registry_with_key(&path, "irrelevant-key"),
        pricing: Arc::new(ArcSwap::from_pointee(CostTable::default())),
        resilience: Arc::new(ResilienceRuntime::defaults()),
        metrics: ReloadMetrics::register(&Metrics::new()).expect("reload metrics"),
        key_backfill: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        key_source: None,
        auth_knobs: None,
        webhooks: Some(Arc::clone(&webhooks)),
        auth_runtime: Some(Arc::clone(&runtime)),
    });

    // Reload with a new URL and a new event/threshold set.
    write(&webhook_config_body(
        &upstream.uri(),
        &db,
        Some(&format!(
            r#"
            [webhooks]
            url = "{second}"
            events = ["budget.exhausted"]
            thresholds = [10]
            "#
        )),
    ));
    reload_once(&path, &targets).await;

    assert_eq!(
        webhooks.live_settings().expect("still enabled").url,
        second,
        "the reload retargets delivery"
    );
    let live = runtime.keys.signals().expect("signals still installed");
    assert!(live.wants(EventKind::BudgetExhausted));
    assert!(
        !live.wants(EventKind::BudgetThreshold),
        "the reloaded event set replaces the old one"
    );
    assert_eq!(live.thresholds(), [10]);

    // The key is still live and enforced; nothing about the reload disturbed
    // the in-memory budget state.
    let entry = runtime
        .keys
        .authenticate(plaintext.reveal(), 0)
        .expect("key still live after the reload");
    assert!(entry.admit(0, 0, usd_to_micro(1.0)).is_ok());

    // Removing the block stops detection entirely.
    write(&webhook_config_body(&upstream.uri(), &db, None));
    reload_once(&path, &targets).await;
    assert!(
        runtime.keys.signals().is_none(),
        "removing [webhooks] must stop emitting budget events"
    );
    assert!(webhooks.live_settings().is_none());
}

#[tokio::test]
async fn a_stored_webhook_row_wins_over_the_config_block_across_reloads() {
    // ADR 011 amendment §2: a PUT /admin/webhooks must not be undone by the
    // next reload, and a DELETE must not be re-enabled by it either.
    use lumen_auth::events::{SettingsSource, WebhookSettings};
    use lumen_server::webhooks::WebhookController;
    use tokio_util::sync::CancellationToken;

    let upstream = MockServer::start().await;
    let dir = tempdir();
    let db = dir.join("auth.db");
    let path = dir.join("config.toml");
    let from_file = "https://from-file.example/events";
    let from_api = "https://from-api.example/events";
    let write = |body: &str| {
        let mut file = std::fs::File::create(&path).expect("write config");
        file.write_all(body.as_bytes()).expect("write config body");
    };
    write(&webhook_config_body(
        &upstream.uri(),
        &db,
        Some(&format!(
            r#"
            [webhooks]
            url = "{from_file}"
            events = ["budget.threshold"]
            thresholds = [50]
            "#
        )),
    ));

    let (runtime, _plaintext) = webhook_auth_runtime(&db).await;
    let webhooks = Arc::new(WebhookController::new(
        Metrics::new(),
        lumen_providers::http::build_client(),
        CancellationToken::new(),
    ));
    let targets = Arc::new(ReloadTargets {
        registry: registry_with_key(&path, "irrelevant-key"),
        pricing: Arc::new(ArcSwap::from_pointee(CostTable::default())),
        resilience: Arc::new(ResilienceRuntime::defaults()),
        metrics: ReloadMetrics::register(&Metrics::new()).expect("reload metrics"),
        key_backfill: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        key_source: None,
        auth_knobs: None,
        webhooks: Some(Arc::clone(&webhooks)),
        auth_runtime: Some(Arc::clone(&runtime)),
    });

    // With no stored row, the file wins.
    reload_once(&path, &targets).await;
    assert_eq!(webhooks.source(), SettingsSource::Config);
    assert_eq!(webhooks.live_settings().expect("enabled").url, from_file);

    // Store a row (as PUT /admin/webhooks does) and reload: the row wins.
    runtime
        .store
        .save_webhook_config(&WebhookSettings {
            url: from_api.to_owned(),
            ..WebhookSettings::default()
        })
        .await
        .expect("store the webhook config");
    reload_once(&path, &targets).await;
    assert_eq!(webhooks.source(), SettingsSource::Database);
    assert_eq!(
        webhooks.live_settings().expect("enabled").url,
        from_api,
        "a stored row must survive a reload that would otherwise re-apply the file"
    );

    // Disable it (as DELETE /admin/webhooks does) and reload: still off, even
    // though the file block is right there.
    runtime
        .store
        .disable_webhook_config()
        .await
        .expect("disable the stored config");
    reload_once(&path, &targets).await;
    assert_eq!(webhooks.source(), SettingsSource::None);
    assert!(
        webhooks.live_settings().is_none(),
        "a disabled row must shadow the config block"
    );
}

/// The current value of `lumen_config_reloads_total`, scraped from the
/// Prometheus text output (the counter has no public getter).
fn reloads_total(metrics: &Metrics) -> u64 {
    metrics
        .encode_text()
        .lines()
        .find_map(|line| line.strip_prefix("lumen_config_reloads_total "))
        .and_then(|value| value.trim().parse().ok())
        .expect("lumen_config_reloads_total is registered and exported")
}

/// Poll `reloads_total` until it stops moving for `quiet`, or `deadline`
/// passes. Returns the last observed count. A genuine one-shot reload settles
/// almost immediately; a feedback loop never does, so this returns whatever it
/// had climbed to by the deadline and the caller's assertion fails on the
/// magnitude.
async fn settled_reload_count(metrics: &Metrics, quiet: Duration, deadline: Duration) -> u64 {
    let start = tokio::time::Instant::now();
    let mut last = reloads_total(metrics);
    let mut stable_since = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = reloads_total(metrics);
        if now == last {
            if stable_since.elapsed() >= quiet {
                return now;
            }
        } else {
            last = now;
            stable_since = tokio::time::Instant::now();
        }
        if start.elapsed() >= deadline {
            return reloads_total(metrics);
        }
    }
}

/// Arm the real watcher on a temp directory holding a valid config, and
/// return (dir, config path, registry, metrics, reloader handle).
async fn armed_reloader() -> (
    PathBuf,
    PathBuf,
    Arc<Registry>,
    Metrics,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempdir();
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, ONE_MODEL_CHAT).expect("write config");
    let registry = registry_from_chat_config(&config_path);
    let metrics = Metrics::new();
    let reload_metrics = ReloadMetrics::register(&metrics).expect("reload metrics");
    let targets = reload_targets(Arc::clone(&registry), reload_metrics);
    let handle = spawn_config_reloader(config_path.clone(), targets, Arc::new(Notify::new()))
        .expect("spawn reloader");
    // Let the watcher finish arming before anything touches the file.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        reloads_total(&metrics),
        0,
        "arming the watcher must not reload anything by itself"
    );
    (dir, config_path, registry, metrics, handle)
}

/// Regression test for the self-sustaining hot-reload loop reproduced on
/// production at v0.3.1: `spawn_config_reloader` watches the config's parent
/// DIRECTORY, and `notify`'s inotify backend arms that watch with a 0xfee
/// mask that includes `IN_OPEN` (0x20). Because a reload OPENS the config
/// file to re-read it, every reload used to schedule the next one: one
/// `cat /etc/lumen/config.toml` from any process put the gateway into a
/// ~4 reloads/second loop that only a restart cleared. The project's own
/// `lumen --check-config` and `lumen keys list` both open the file, so
/// running the documented pre-reload safety check was enough to trigger it.
///
/// Note: like the rename-replace test above, this can only discriminate old
/// from new code where the backend reports opens at all, i.e. inotify
/// (Linux). FSEvents does not report file opens, so on macOS the count stays
/// at 0 either way and this test is a smoke test rather than a regression
/// guard. That platform gap is known and accepted.
#[tokio::test]
async fn reading_the_config_file_does_not_start_a_reload_loop() {
    let (_dir, config_path, _registry, metrics, handle) = armed_reloader().await;

    // Exactly what `cat`, `lumen --check-config` and `lumen keys list` do:
    // open the config file and read it. Nothing is written.
    let bytes = std::fs::read(&config_path).expect("read config");
    assert!(
        !bytes.is_empty(),
        "the config file was read, not just stat'd"
    );

    // Well past several debounce windows (DEBOUNCE is 250ms): pre-fix this
    // had already climbed into double digits and was still climbing.
    let count =
        settled_reload_count(&metrics, Duration::from_millis(600), Duration::from_secs(3)).await;
    assert!(
        count <= 1,
        "reading the config file must schedule at most one reload, got {count}"
    );

    // And it is not merely slow: the count is stable, not still climbing.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        reloads_total(&metrics),
        count,
        "the reload count must be stable after the debounce window, not climbing"
    );
    handle.abort();
}

/// The flip side of the loop fix: a genuine content change must still be
/// picked up, exactly once. The change goes in via write-to-staging plus
/// rename, which is what `PUT /admin/config` and a GitOps sync both do, and
/// which lands as a single `IN_MOVED_TO` rather than a burst.
#[tokio::test]
async fn a_real_config_change_still_triggers_exactly_one_reload() {
    let (dir, config_path, registry, metrics, handle) = armed_reloader().await;
    assert!(
        registry.embedding_route("embed").is_none(),
        "the embed model is not routable before the change"
    );

    let staged = dir.join("config.toml.staged");
    std::fs::write(&staged, TWO_MODELS_CHAT_EMBED).expect("write staged config");
    std::fs::rename(&staged, &config_path).expect("rename into place");

    // The swap must actually happen: hot reload still works.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if registry.embedding_route("embed").is_some() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "hot reload did not fire for a genuine config change"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Once, and only once - the reload's own read of the file must not
    // schedule another one.
    let count =
        settled_reload_count(&metrics, Duration::from_millis(600), Duration::from_secs(3)).await;
    assert_eq!(
        count, 1,
        "a single config change must produce exactly one reload, got {count}"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        reloads_total(&metrics),
        1,
        "the reload count must stay at one, not climb after the change"
    );
    handle.abort();
}
