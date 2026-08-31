//! End-to-end coverage for the `ConfigSource` abstraction (ADR 012), both
//! modes, tying together what Tasks 1-8 built in isolation:
//!
//! 1. file mode: a granular provider reroute lands live, no restart.
//! 2. db mode: a cold start with no stored document, then the first
//!    `PUT /admin/config` makes chat requests reach a real upstream.
//! 3. db mode: a stale `If-Match` after a successful apply is refused.
//! 4. file mode: an external (human/GitOps) edit to the file makes an
//!    `If-Match` taken before that edit stale.
//! 5. db mode: neither a whole-document nor a granular `PUT` ever writes a
//!    RESOLVED provider secret into `config_versions` - only the env var
//!    name.
//!
//! Also covers a Task 8 review follow-up deferred here: a db-mode regression
//! test for `PUT /admin/config/auth` (see
//! `db_mode_auth_knobs_put_does_not_trip_the_boot_layer_guard`).
//!
//! Tests 1 and 2 spawn the real `lumen` binary (mirroring
//! `tests/config_source_boot.rs` and `tests/signal_shutdown.rs`): they need
//! the full `main.rs` boot sequence - `arm_config_reload` wiring the admin
//! trigger, the file watcher and `ReloadTargets` together - which is not
//! reachable from a library call. Tests 3-5 and the Task 8 regression only
//! exercise the admin config pipeline itself (no routing, no reload), so
//! they use the same in-process `AppState` harness style as
//! `tests/admin_config.rs` instead - faster and no process-spawn flakiness,
//! at no loss of coverage for what they're actually checking.

mod common;

use std::io::Read;
use std::net::TcpListener as StdTcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lumen_auth::crypto::MasterKey;
use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::{KeyStore, NewKey};
use lumen_providers::Registry;
use lumen_server::auth::AuthRuntime;
use lumen_server::config_source::{empty_doc_hash, ConfigContext, DbSource};
use lumen_server::AppState;
use lumen_telemetry::{LatencyMetrics, Metrics, TokenMetrics};
use serde_json::{json, Value};
use sqlx::Row;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;

/// A valid master-key value (64 hex chars): the admin bearer token in every
/// harness below, and the boot process's `LUMEN_MASTER_KEY` for the
/// real-binary tests.
fn master() -> String {
    "a".repeat(64)
}

// ============================================================================
// Shared real-binary plumbing (tests 1 and 2). Copied from
// `tests/config_source_boot.rs` rather than shared: test files under
// `tests/` cannot import from each other, only from `tests/common`, and
// `tests/common` is the in-process-harness module the real-binary tests
// deliberately don't use (see the module doc comment).
// ============================================================================

/// Bind an ephemeral port and immediately release it so `lumen` can bind it
/// instead (small TOCTOU tradeoff, same one `tests/signal_shutdown.rs` and
/// `tests/config_source_boot.rs` accept, for the same reason: `lumen` is a
/// separate process taking host:port from its config file).
fn free_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("read local addr").port()
}

/// Write `contents` to a fresh file under the test binary's scratch
/// directory (`CARGO_TARGET_TMPDIR`, cargo-provided) and return its path.
/// `unique` disambiguates concurrently-running tests in this file.
fn write_temp_config(unique: &str, contents: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let path = dir.join(format!("config-source-e2e-{unique}.toml"));
    std::fs::write(&path, contents).expect("write temp config");
    path
}

fn lumen() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lumen"))
}

/// Poll `GET {base}/health` until it answers 200 or `timeout` elapses.
async fn wait_until_ready(base: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(resp) = reqwest::get(format!("{base}/health")).await {
            if resp.status().is_success() {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Drain and return the child's stdout/stderr - required so a killed child's
/// pipes don't fill and hang the test, and so a failure has a log to show.
fn drain_output(mut child: Child) -> (String, String) {
    let mut out = String::new();
    let mut err = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut err);
    }
    (out, err)
}

/// Mount a `POST /chat/completions` OpenAI-shaped response whose message
/// content is `marker` - lets a test tell which of two upstreams actually
/// answered a request.
async fn mount_chat(upstream: &MockServer, marker: &str) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-e2e",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-4o-2024-08-06",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": marker },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })))
        .mount(upstream)
        .await;
}

/// Parse the `lumen_config_reloads_total` counter out of `GET /metrics`
/// text. Only increments on a SUCCESSFUL registry swap
/// (`ReloadMetrics::inc_success`, see `crates/telemetry/src/reload.rs`), so
/// it is a precise, side-effect-free signal that a hot reload landed.
fn reloads_total_from_metrics(text: &str) -> u64 {
    text.lines()
        .find_map(|line| line.strip_prefix("lumen_config_reloads_total "))
        .and_then(|value| value.trim().parse().ok())
        .expect("lumen_config_reloads_total is registered and exported")
}

async fn reloads_total(base: &str) -> u64 {
    let text = reqwest::get(format!("{base}/metrics"))
        .await
        .expect("get metrics")
        .text()
        .await
        .expect("metrics body");
    reloads_total_from_metrics(&text)
}

/// Poll `GET /metrics` until `lumen_config_reloads_total` rises above
/// `baseline`, or panic after `timeout`. Bounded, deterministic wait for an
/// admin-triggered hot reload to land, without retrying the actual chat
/// endpoint (see `file_mode_provider_reroute_takes_effect_without_restart`'s
/// doc comment for why that would be the wrong signal to poll here).
async fn wait_for_reload_past(base: &str, baseline: u64, timeout: Duration) -> u64 {
    let deadline = Instant::now() + timeout;
    loop {
        let n = reloads_total(base).await;
        if n > baseline {
            return n;
        }
        assert!(
            Instant::now() < deadline,
            "no successful config reload landed within {timeout:?} (still at {n}, baseline {baseline})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Pre-seed a virtual key directly against the sqlite file at `db_path`,
/// before `lumen` ever boots (mirrors `tests/config_source_boot.rs`'s
/// preseeded test): auth is enabled from the very first request either
/// real-binary test below sends, so a key must already exist in the store
/// `lumen` is about to open.
async fn preseed_virtual_key(
    db_path: &std::path::Path,
    name: &str,
) -> lumen_auth::key::PlaintextKey {
    let store = KeyStore::connect(&format!("sqlite://{}", db_path.display()))
        .await
        .expect("open store to pre-seed");
    let (plaintext, _record) = store
        .create_key(NewKey {
            name: name.to_owned(),
            ..NewKey::default()
        })
        .await
        .expect("pre-seed a virtual key");
    plaintext
}

/// Spawn `lumen` against `config_path` on `port` and wait for `/health` to
/// answer. Returns the running child plus its base URL.
async fn spawn_lumen_ready(port: u16, config_path: &std::path::Path) -> (Child, String) {
    let child = lumen()
        .args(["--config"])
        .arg(config_path)
        .env("LUMEN_MASTER_KEY", master())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lumen");
    let base = format!("http://127.0.0.1:{port}");
    let ready = wait_until_ready(&base, Duration::from_secs(10)).await;
    assert!(ready, "lumen did not become ready");
    (child, base)
}

/// `GET /admin/config`'s hash, with the master key.
async fn admin_config_hash(client: &reqwest::Client, base: &str) -> String {
    let doc: Value = client
        .get(format!("{base}/admin/config"))
        .bearer_auth(master())
        .send()
        .await
        .expect("get config")
        .json()
        .await
        .expect("config json");
    doc["hash"].as_str().expect("hash").to_owned()
}

/// `POST /v1/chat/completions` for model `"gpt"`, with `key` as the bearer
/// virtual key.
async fn send_gpt_chat(client: &reqwest::Client, base: &str, key: &str) -> reqwest::Response {
    client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(key)
        .json(&json!({
            "model": "gpt",
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send()
        .await
        .expect("chat request")
}

// ============================================================================
// Test 1: file mode, a granular provider reroute lands live, no restart.
// ============================================================================

#[tokio::test]
async fn file_mode_provider_reroute_takes_effect_without_restart() {
    let upstream_a = MockServer::start().await;
    mount_chat(&upstream_a, "reached-A").await;
    let upstream_b = MockServer::start().await;
    mount_chat(&upstream_b, "reached-B").await;

    let port = free_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("lumen.db");
    let config = write_temp_config(
        "file-reroute",
        &format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[auth]
enabled = true
db_path = "{db}"

[[providers]]
name = "test-provider"
kind = "openai"
base_url = "{a}"

[[providers.models]]
id = "gpt"
capabilities = ["chat"]
"#,
            db = db_path.display(),
            a = upstream_a.uri(),
        ),
    );

    let plaintext = preseed_virtual_key(&db_path, "reroute-test").await;
    let (mut child, base) = spawn_lumen_ready(port, &config).await;
    let client = reqwest::Client::new();

    let hash = admin_config_hash(&client, &base).await;
    let baseline_reloads = reloads_total(&base).await;

    // Reroute the provider from wiremock A to wiremock B through the
    // granular provider endpoint (Task 8) - the exact shape an operator's
    // console would send.
    let provider = json!({
        "name": "test-provider",
        "kind": "openai",
        "base_url": upstream_b.uri(),
        "models": [{ "id": "gpt", "capabilities": ["chat"] }]
    });
    let response = client
        .put(format!("{base}/admin/config/providers/test-provider"))
        .bearer_auth(master())
        .header("If-Match", &hash)
        .json(&provider)
        .send()
        .await
        .expect("put provider");
    assert_eq!(response.status(), 204);

    // The reload runs asynchronously off the admin trigger (a
    // `tokio::sync::Notify` pinged at the end of `admin::apply_document`,
    // consumed by the background task `spawn_config_reloader` armed at
    // boot): the PUT above only guarantees the write landed, not that the
    // registry has been rebuilt yet. Polling `lumen_config_reloads_total` on
    // `/metrics` (a counter that only increments on a SUCCESSFUL registry
    // swap) is a bounded, deterministic way to wait for that - polling the
    // actual chat endpoint instead would send an unknown number of requests
    // to wiremock A while the reload is still in flight, which would make
    // "A received nothing after the reroute" unprovable.
    let landed = wait_for_reload_past(&base, baseline_reloads, Duration::from_secs(5)).await;
    assert!(landed > baseline_reloads);

    // Exactly one chat request, sent only after the reload is known to have
    // landed.
    let resp = send_gpt_chat(&client, &base, plaintext.reveal()).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("chat json");
    assert_eq!(body["choices"][0]["message"]["content"], "reached-B");

    let _ = child.kill();
    let _ = child.wait();
    let (out, err) = drain_output(child);

    let b_requests = upstream_b
        .received_requests()
        .await
        .expect("b requests")
        .len();
    let a_requests = upstream_a
        .received_requests()
        .await
        .expect("a requests")
        .len();
    assert_eq!(
        b_requests, 1,
        "wiremock B must receive exactly the one post-reroute chat request\n\
         --- stdout ---\n{out}\n--- stderr ---\n{err}"
    );
    assert_eq!(
        a_requests, 0,
        "wiremock A must never receive a request after the reroute landed\n\
         --- stdout ---\n{out}\n--- stderr ---\n{err}"
    );
}

// ============================================================================
// Test 2: db mode, cold start with an empty stored document, then the first
// `PUT /admin/config` makes chat reach a real upstream - no restart.
// ============================================================================

#[tokio::test]
async fn db_mode_cold_start_installs_config_without_restart() {
    let upstream = MockServer::start().await;
    mount_chat(&upstream, "db-mode-target").await;

    let port = free_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("lumen.db");
    // `config_source` must come before any `[table]` header: a bare key
    // after one belongs to that table in TOML, not the document root (see
    // `tests/config_source_boot.rs`'s `db_mode_boot_config`).
    let config = write_temp_config(
        "db-cold-start",
        &format!(
            r#"
config_source = "db"

[server]
host = "127.0.0.1"
port = {port}

[auth]
enabled = true
db_path = "{db}"
"#,
            db = db_path.display(),
        ),
    );

    // The boot-only document above declares no providers at all, so there
    // is nothing for a chat request to route to yet regardless - the key
    // just needs to already authenticate from the first request.
    let plaintext = preseed_virtual_key(&db_path, "cold-start-test").await;
    let (mut child, base) = spawn_lumen_ready(port, &config).await;
    let client = reqwest::Client::new();

    let health = client
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("get health");
    assert_eq!(health.status(), 200);

    // No providers exist yet (the stored document is empty): a chat request
    // for any model is a 404 naming an unknown model, not any kind of
    // provider/upstream failure.
    let cold_chat = send_gpt_chat(&client, &base, plaintext.reveal()).await;
    assert_eq!(cold_chat.status(), 404);
    let cold_body: Value = cold_chat.json().await.expect("cold chat json");
    assert_eq!(cold_body["error"]["code"], "LM-2001");

    let hash = admin_config_hash(&client, &base).await;
    assert_eq!(
        hash,
        empty_doc_hash(),
        "before any PUT, db mode's current hash is the empty document's hash"
    );

    let baseline_reloads = reloads_total(&base).await;

    let full_doc = format!(
        r#"
[[providers]]
name = "db-provider"
kind = "openai"
base_url = "{upstream}"

[[providers.models]]
id = "gpt"
capabilities = ["chat"]
"#,
        upstream = upstream.uri(),
    );
    let response = client
        .put(format!("{base}/admin/config"))
        .bearer_auth(master())
        .header("If-Match", &hash)
        .body(full_doc)
        .send()
        .await
        .expect("put config");
    assert_eq!(response.status(), 204);

    // Same deterministic, side-effect-free wait as test 1: poll the reload
    // counter, not the chat endpoint, before sending the one request whose
    // outcome the test actually asserts on.
    let landed = wait_for_reload_past(&base, baseline_reloads, Duration::from_secs(5)).await;
    assert!(landed > baseline_reloads);

    let warm_chat = send_gpt_chat(&client, &base, plaintext.reveal()).await;
    assert_eq!(
        warm_chat.status(),
        200,
        "the installed config must route without a restart"
    );
    let warm_body: Value = warm_chat.json().await.expect("warm chat json");
    assert_eq!(
        warm_body["choices"][0]["message"]["content"],
        "db-mode-target"
    );

    let _ = child.kill();
    let _ = child.wait();
    let (out, err) = drain_output(child);

    let upstream_requests = upstream
        .received_requests()
        .await
        .expect("upstream requests")
        .len();
    assert_eq!(
        upstream_requests, 1,
        "the one post-install chat request must have reached the real upstream\n\
         --- stdout ---\n{out}\n--- stderr ---\n{err}"
    );
}

// ============================================================================
// In-process harnesses for tests 3-5 and the Task 8 regression: no routing,
// no reload, no wiremock needed - only the admin config pipeline itself, so
// the lighter `tests/admin_config.rs`-style in-process `AppState` harness
// applies here instead of a spawned binary.
// ============================================================================

/// A file-mode config document with no providers at all: valid (an empty
/// provider list is legal), and enough for the hash/If-Match tests below,
/// which never route a request.
const MINIMAL_FILE_CONFIG: &str = "# minimal file-mode config\n[server]\nport = 6001\n";

struct FileHarness {
    base: String,
    client: reqwest::Client,
    config_path: std::path::PathBuf,
    /// Kept alive for the harness's lifetime: dropping it would delete
    /// `config_path` out from under the running server.
    _dir: TempDir,
}

impl FileHarness {
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .bearer_auth(master())
            .send()
            .await
            .expect("send")
    }

    async fn current_hash(&self) -> String {
        let body: Value = self.get("/admin/config").await.json().await.expect("json");
        body["hash"].as_str().expect("hash").to_owned()
    }

    async fn put_config(&self, body: &str, if_match: &str) -> reqwest::Response {
        self.client
            .put(format!("{}/admin/config", self.base))
            .bearer_auth(master())
            .header("If-Match", if_match)
            .body(body.to_owned())
            .send()
            .await
            .expect("request sent")
    }
}

async fn spawn_file_harness(registry: Arc<Registry>) -> FileHarness {
    let dir = TempDir::new().expect("create temp dir");
    let config_path = dir.path().join("lumen.toml");
    std::fs::write(&config_path, MINIMAL_FILE_CONFIG).expect("write config file");

    let store = KeyStore::in_memory().await.expect("open store");
    let keys = AuthState::load(Vec::new(), Vec::new());
    let runtime = Arc::new(AuthRuntime {
        keys,
        store,
        admin_token_hash: hash_key(&master()),
        master: Some(MasterKey::from_env_value(&master()).expect("master key")),
    });

    let metrics = Metrics::new();
    let tokens = TokenMetrics::register(&metrics, &[]).expect("register token metrics");
    let latency = LatencyMetrics::register(&metrics).expect("register latency metrics");
    let state = AppState::new(metrics, registry, tokens, latency)
        .with_auth(runtime)
        .with_config_context(Arc::new(ConfigContext::file(config_path.clone())));
    let base = common::spawn_state(state, LIMIT).await;

    FileHarness {
        base,
        client: reqwest::Client::new(),
        config_path,
        _dir: dir,
    }
}

struct DbHarness {
    base: String,
    client: reqwest::Client,
    /// Kept alive: db-mode `validate_document`/`load_config` read
    /// `boot_path` off disk on every apply.
    _dir: TempDir,
    /// The same store the harness's `ConfigContext::db` reads and writes -
    /// exposed so `config_versions` can be queried directly (test 5).
    store: KeyStore,
}

impl DbHarness {
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .bearer_auth(master())
            .send()
            .await
            .expect("send")
    }

    async fn current_hash(&self) -> String {
        let body: Value = self.get("/admin/config").await.json().await.expect("json");
        body["hash"].as_str().expect("hash").to_owned()
    }

    async fn put_config(&self, body: &str, if_match: &str) -> reqwest::Response {
        self.client
            .put(format!("{}/admin/config", self.base))
            .bearer_auth(master())
            .header("If-Match", if_match)
            .body(body.to_owned())
            .send()
            .await
            .expect("request sent")
    }

    async fn put_json(&self, path: &str, body: &Value, if_match: &str) -> reqwest::Response {
        self.client
            .put(format!("{}{path}", self.base))
            .bearer_auth(master())
            .header("If-Match", if_match)
            .json(body)
            .send()
            .await
            .expect("request sent")
    }
}

/// Build an auth-enabled `AppState` over a `ConfigContext::db`, mirroring
/// `tests/admin_config.rs`'s `spawn_admin_db_mode`.
async fn spawn_db_harness(registry: Arc<Registry>) -> DbHarness {
    let store = KeyStore::in_memory().await.expect("open store");
    let keys = AuthState::load(Vec::new(), Vec::new());
    let runtime = Arc::new(AuthRuntime {
        keys,
        store: store.clone(),
        admin_token_hash: hash_key(&master()),
        master: Some(MasterKey::from_env_value(&master()).expect("master key")),
    });

    let dir = TempDir::new().expect("create temp dir");
    let boot_path = dir.path().join("boot.toml");
    std::fs::write(&boot_path, "[auth]\nenabled = true\n").expect("write boot file");
    let ctx = Arc::new(ConfigContext::db(boot_path, DbSource::new(store.clone())));

    let metrics = Metrics::new();
    let tokens = TokenMetrics::register(&metrics, &[]).expect("register token metrics");
    let latency = LatencyMetrics::register(&metrics).expect("register latency metrics");
    let state = AppState::new(metrics, registry, tokens, latency)
        .with_auth(runtime)
        .with_config_context(ctx);
    let base = common::spawn_state(state, LIMIT).await;

    DbHarness {
        base,
        client: reqwest::Client::new(),
        _dir: dir,
        store,
    }
}

// ============================================================================
// Test 3: db mode, a stale `If-Match` after a successful apply is refused.
// ============================================================================

#[tokio::test]
async fn db_mode_stale_write_after_apply_is_refused_with_412() {
    let h = spawn_db_harness(common::empty_registry()).await;

    // Two GETs before any write see the same (empty-document) hash.
    let hash_1 = h.current_hash().await;
    let hash_1_again = h.current_hash().await;
    assert_eq!(hash_1, hash_1_again);

    let doc = r#"
[[providers]]
name = "db-provider"
kind = "openai"
[[providers.models]]
id = "gpt"
capabilities = ["chat"]
"#;
    let applied = h.put_config(doc, &hash_1).await;
    assert_eq!(
        applied.status(),
        204,
        "the first apply, against the fresh hash, must succeed"
    );

    // Re-applying with the now-stale `hash_1` - the document has moved on -
    // must be refused, never silently accepted or re-applied.
    let reapplied = h.put_config(doc, &hash_1).await;
    assert_eq!(reapplied.status(), 412);
    let body: Value = reapplied.json().await.expect("json");
    assert_eq!(body["error"]["code"].as_str().expect("code"), "LM-1004");

    // Nothing from the rejected re-apply changed the stored document.
    let after: Value = h.get("/admin/config").await.json().await.expect("json");
    assert_eq!(after["config"].as_str().expect("config string"), doc);
}

// ============================================================================
// Test 4: file mode, an external (human/GitOps) edit makes an `If-Match`
// taken before that edit stale.
// ============================================================================

#[tokio::test]
async fn file_mode_external_edit_makes_a_stale_if_match_refused_with_412() {
    let h = spawn_file_harness(common::empty_registry()).await;
    let hash_before_edit = h.current_hash().await;

    // The human/GitOps writer: edits the file directly on disk, entirely
    // outside the admin API. `apply_document`'s very first step re-reads
    // the source (`ConfigSource::load`) and compares against `If-Match`
    // BEFORE staging or validating anything, so this external edit is
    // caught there - the same lost-update guard `FileSource::persist`'s own
    // CAS re-check exists for, just tripped one step earlier.
    let edited = format!("{MINIMAL_FILE_CONFIG}\n# edited directly on disk by a human\n");
    std::fs::write(&h.config_path, &edited).expect("simulate an external edit");

    let response = h.put_config(&edited, &hash_before_edit).await;
    assert_eq!(response.status(), 412);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"].as_str().expect("code"), "LM-1004");

    // The external edit survives untouched: a rejected apply must never
    // overwrite it.
    assert_eq!(
        std::fs::read_to_string(&h.config_path).expect("read config"),
        edited
    );
}

// ============================================================================
// Task 8 review follow-up (deferred to this task): a db-mode regression test
// for `PUT /admin/config/auth` - the 5-knob merge must not trip the
// boot-layer guard in db mode. Correct by inspection per the Task 8 review;
// exercised here for real.
// ============================================================================

#[tokio::test]
async fn db_mode_auth_knobs_put_does_not_trip_the_boot_layer_guard() {
    // `config_edit::replace_auth_knobs` grafts only the 5 hot-reloadable
    // `[auth]` keys into the DYNAMIC document; `enabled`/`db_path` live in
    // the boot file only (`spawn_db_harness`'s `boot.toml`) and are never
    // part of what this route reads or writes. `boot_layer_diff` compares
    // the CURRENT and CANDIDATE dynamic-document texts - both silent on
    // `auth.enabled`/`auth.db_path` in db mode - so this merge must not be
    // mistaken for a boot-layer change.
    let h = spawn_db_harness(common::empty_registry()).await;
    let hash = h.current_hash().await;

    let knobs = json!({
        "flush_interval_ms": 5000,
        "usage_channel_capacity": 100,
        "usage_batch_max": 50,
        "usage_flush_ms": 500,
        "retention_days": 30
    });
    let response = h.put_json("/admin/config/auth", &knobs, &hash).await;
    assert_eq!(
        response.status(),
        204,
        "the auth-knobs merge must apply in db mode, not be mistaken for a boot-layer change"
    );

    let doc: Value = h.get("/admin/config").await.json().await.expect("json");
    let text = doc["config"].as_str().expect("config string");
    assert!(text.contains("flush_interval_ms = 5000"), "{text}");
    assert!(
        !text.contains("enabled") && !text.contains("db_path"),
        "the stored dynamic document must never gain the boot-layer auth keys: {text}"
    );
}

// ============================================================================
// Test 5: db mode, neither a whole-document nor a granular `PUT` ever writes
// a RESOLVED provider secret into `config_versions` - only the env var name.
// ============================================================================

/// Env var name the harness config's provider resolves its key from.
/// Distinctive, and deliberately NOT `LUMEN_`-prefixed (see
/// `tests/admin_config.rs`'s identical precaution: a `LUMEN_`-prefixed var
/// would fold into every `Config::load` call in this whole test binary via
/// figment's env merge, breaking unrelated tests with "unknown field").
const SECRET_ENV: &str = "CONFIG_SOURCE_E2E_API_KEY_ENV";
/// The sentinel value exported under `SECRET_ENV`. Must never appear in
/// `config_versions`: only the env var NAME is ever config content.
const SECRET_VALUE: &str = "sentinel-config-source-e2e-secret";

#[tokio::test]
async fn db_mode_config_versions_never_stores_a_resolved_provider_secret() {
    // Written at most once per process (see `tests/admin_config.rs`'s
    // identical guard): `PUT /admin/config` reads the environment on a
    // blocking worker while validating a candidate, so a concurrent test
    // entering this one and writing the var is a genuine setenv/getenv race,
    // not merely a redundant write.
    static ENV_ONCE: std::sync::Once = std::sync::Once::new();
    ENV_ONCE.call_once(|| {
        std::env::set_var(SECRET_ENV, SECRET_VALUE);
    });

    let h = spawn_db_harness(common::empty_registry()).await;
    let hash = h.current_hash().await;

    let whole_doc = format!(
        r#"
[[providers]]
name = "leaky-provider"
kind = "openai"
api_key_env = "{SECRET_ENV}"

[[providers.models]]
id = "gpt"
capabilities = ["chat"]
"#
    );
    let applied = h.put_config(&whole_doc, &hash).await;
    assert_eq!(applied.status(), 204);

    // Also exercise the granular path (Task 8): a second provider with the
    // same secret-bearing env var, added through `PUT
    // /admin/config/providers/{name}` rather than the whole-document route.
    let hash_2 = h.current_hash().await;
    let granular_provider = json!({
        "name": "leaky-provider-2",
        "kind": "openai",
        "api_key_env": SECRET_ENV,
        "models": [{ "id": "gpt-2", "capabilities": ["chat"] }]
    });
    let granular = h
        .put_json(
            "/admin/config/providers/leaky-provider-2",
            &granular_provider,
            &hash_2,
        )
        .await;
    assert_eq!(granular.status(), 204);

    // Open the store directly and inspect every stored version's raw TOML -
    // never through the admin API, which already masks the resolved secret;
    // this is the ground truth of what actually landed on disk.
    let rows = sqlx::query("SELECT toml FROM config_versions")
        .fetch_all(h.store.pool())
        .await
        .expect("query config_versions");
    assert!(
        !rows.is_empty(),
        "expected at least one stored config version"
    );

    let mut saw_env_name = false;
    for row in &rows {
        let toml: String = row.try_get("toml").expect("toml column");
        assert!(
            !toml.contains(SECRET_VALUE),
            "the resolved secret must never be written to config_versions: {toml}"
        );
        if toml.contains(SECRET_ENV) {
            saw_env_name = true;
        }
    }
    assert!(
        saw_env_name,
        "sanity check: the env var NAME must still appear in at least one stored version, \
         proving this test actually exercised the secret-bearing path"
    );
}
