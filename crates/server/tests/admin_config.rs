//! Integration tests for `GET /admin/config` (ADR 010): master-key gating,
//! and the central design constraint that the response is the config file's
//! raw bytes, never a re-serialisation of the in-memory `Config` (which would
//! render environment overrides as if they were file content).

mod common;

use std::sync::Arc;
use std::time::Duration;

use lumen_auth::crypto::MasterKey;
use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::KeyStore;
use lumen_auth::usage::{spawn_usage_writer, UsageWriterConfig};
use lumen_providers::Registry;
use lumen_server::auth::AuthRuntime;
use lumen_server::AppState;
use lumen_telemetry::{LatencyMetrics, Metrics, TokenMetrics};
use serde_json::Value;
use tempfile::TempDir;

const LIMIT: usize = 10 * 1024 * 1024;

/// The master key value (64 hex chars) used as the admin bearer token.
fn master() -> String {
    "a".repeat(64)
}

/// Name of the env var the harness config references via `api_key_env`.
/// Distinctive so it cannot race any other test in this workspace, and
/// deliberately NOT `LUMEN_`-prefixed: `Config::load` merges every
/// `LUMEN_`-prefixed env var as a config override via figment, and `Config`
/// denies unknown fields, so a `LUMEN_`-prefixed provider-key var here would
/// make every `Config::load` call in this file fail with "unknown field"
/// (only surfaces once a test exercises a real load, i.e. `PUT`'s
/// `validate_candidate` - `GET` never calls `Config::load`).
const PROVIDER_KEY_ENV: &str = "TEST_PROVIDER_API_KEY";
/// The sentinel value exported under `PROVIDER_KEY_ENV`. Must never appear in
/// the rendered config: only the env var *name* is file content.
const PROVIDER_KEY_VALUE: &str = "sentinel-provider-secret";

/// `LUMEN_`-prefixed env var that `Config::load`'s figment merge genuinely
/// folds into a `Config` field: `Env::prefixed("LUMEN_").split("__")` maps
/// `LUMEN_SERVER__PORT` onto `server.port` (config.rs, `Config::load`), and
/// `config.rs`'s own `env_var_overrides_file_value` test proves this - it
/// sets exactly this var and asserts `cfg.server.port` changes to match.
/// Used here, set to a value the config file never mentions, to prove the
/// handler returns the raw file rather than a `Config` merged with env
/// overrides: a handler that round-tripped through `Config::load` and
/// re-serialized the result would leak this value into the response.
const ENV_OVERRIDE_PORT: &str = "LUMEN_SERVER__PORT";
/// The env override value for `ENV_OVERRIDE_PORT`. Distinct from `FILE_PORT`
/// and from `Config`'s own default port (8080), so its presence in the
/// response can only be explained by a merge that must not happen.
const ENV_OVERRIDE_PORT_VALUE: &str = "40404";
/// The port the harness config file specifies directly, distinct from
/// `ENV_OVERRIDE_PORT_VALUE` and from the default, so the response containing
/// it can only be explained by reading the file.
const FILE_PORT: &str = "7777";

/// Config file content the harness boots with: an explicit `server.port`
/// (see `FILE_PORT`) distinct from what the env override would set, and a
/// provider that resolves its key from `PROVIDER_KEY_ENV`, so the
/// "never expose a resolved value" test has something to resolve.
const CONFIG_TOML: &str = r#"
[server]
port = 7777

[[providers]]
name = "test-provider"
kind = "openai"
api_key_env = "TEST_PROVIDER_API_KEY"

[[providers.models]]
id = "gpt-4o"
capabilities = ["chat"]
"#;

struct Harness {
    base: String,
    client: reqwest::Client,
    /// Path of the on-disk config file `AppState::config_path` points at.
    config_path: std::path::PathBuf,
    /// Kept alive for the harness's lifetime: dropping it would delete
    /// `config_path` out from under the running server.
    _dir: TempDir,
}

impl Harness {
    /// GET an admin path with the master key.
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .bearer_auth(master())
            .send()
            .await
            .expect("send")
    }

    /// The current config hash, via GET.
    async fn current_hash(&self) -> String {
        let body: Value = self.get("/admin/config").await.json().await.expect("json");
        body["hash"].as_str().expect("hash").to_owned()
    }

    /// PUT a config document with an `If-Match` header.
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

    /// The config document the harness booted from.
    fn valid_config(&self) -> String {
        std::fs::read_to_string(&self.config_path).expect("read config")
    }
}

/// The same minimal registry the other admin test suites use: no providers
/// configured, since these tests only exercise the admin config route.
fn registry() -> Arc<Registry> {
    common::empty_registry()
}

/// Spawn an auth-enabled gateway booted against a real config file on disk.
async fn spawn_admin(registry: Arc<Registry>) -> Harness {
    // Distinctive var name (see `PROVIDER_KEY_ENV` doc comment) so setting it
    // process-wide cannot race any other test in the workspace.
    std::env::set_var(PROVIDER_KEY_ENV, PROVIDER_KEY_VALUE);
    // Genuinely merges into `Config.server.port` via figment (see
    // `ENV_OVERRIDE_PORT` doc comment); every test in this file sets it to
    // the same value, so no race with itself either.
    std::env::set_var(ENV_OVERRIDE_PORT, ENV_OVERRIDE_PORT_VALUE);

    let dir = TempDir::new().expect("create temp dir");
    let config_path = dir.path().join("lumen.toml");
    std::fs::write(&config_path, CONFIG_TOML).expect("write config file");

    let store = KeyStore::in_memory().await.expect("open store");
    let groups = store.load_groups().await.expect("load groups");
    let entries = store.load_auth_entries().await.expect("load entries");
    let keys = AuthState::load(groups, entries);
    let runtime = Arc::new(AuthRuntime {
        keys,
        store: store.clone(),
        admin_token_hash: hash_key(&master()),
        master: Some(MasterKey::from_env_value(&master()).expect("master key")),
    });
    let (logger, _writer) = spawn_usage_writer(
        store.clone(),
        UsageWriterConfig {
            capacity: 64,
            batch_max: 500,
            flush_interval: Duration::from_millis(25),
        },
    );

    let metrics = Metrics::new();
    let tokens = TokenMetrics::register(&metrics, &[]).expect("register token metrics");
    let latency = LatencyMetrics::register(&metrics).expect("register latency metrics");
    let state = AppState::new(metrics, registry, tokens, latency)
        .with_auth(Arc::clone(&runtime))
        .with_usage(logger)
        .with_config_path(config_path.clone());
    let base = common::spawn_state(state, LIMIT).await;

    Harness {
        base,
        client: reqwest::Client::new(),
        config_path,
        _dir: dir,
    }
}

#[tokio::test]
async fn get_config_requires_the_master_key() {
    let h = spawn_admin(registry()).await;
    let response = h
        .client
        .get(format!("{}/admin/config", h.base))
        .send()
        .await
        .expect("request sent");
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn get_config_returns_the_file_bytes_verbatim() {
    let h = spawn_admin(registry()).await;
    let on_disk = std::fs::read_to_string(&h.config_path).expect("read config");

    let body: Value = h.get("/admin/config").await.json().await.expect("json");
    assert_eq!(
        body["config"].as_str().expect("config string"),
        on_disk,
        "the operator sees the file, not a re-serialised merge of file and env"
    );
    assert_eq!(body["hash"].as_str().expect("hash").len(), 64);
}

#[tokio::test]
async fn get_config_never_exposes_a_resolved_provider_key() {
    // The harness config sets api_key_env = "TEST_PROVIDER_API_KEY" and the
    // harness exports that variable with a sentinel value.
    let h = spawn_admin(registry()).await;
    let body: Value = h.get("/admin/config").await.json().await.expect("json");
    let rendered = body["config"].as_str().expect("config string");
    assert!(
        !rendered.contains(PROVIDER_KEY_VALUE),
        "the config exposes the env var NAME, never its resolved value"
    );
    assert!(rendered.contains(PROVIDER_KEY_ENV));
}

#[tokio::test]
async fn get_config_never_exposes_a_merged_env_override() {
    // `ENV_OVERRIDE_PORT` (`LUMEN_SERVER__PORT`) genuinely merges into
    // `Config.server.port` via figment (see its doc comment and
    // `config.rs`'s `env_var_overrides_file_value` test) - unlike the
    // provider-key case above, there is a real code path (`Config::load`
    // followed by re-serializing the merged struct) that WOULD leak this
    // value into the response. The file itself says `port = 7777`
    // (`FILE_PORT`), never `ENV_OVERRIDE_PORT_VALUE`, so this test can only
    // pass if the handler returns the file's own bytes.
    let h = spawn_admin(registry()).await;
    let body: Value = h.get("/admin/config").await.json().await.expect("json");
    let rendered = body["config"].as_str().expect("config string");
    assert!(
        !rendered.contains(ENV_OVERRIDE_PORT_VALUE),
        "an env override merged into `Config` must never leak into the file \
         contents returned to the operator"
    );
    assert!(
        rendered.contains(FILE_PORT),
        "the file's own value must still be present"
    );
}

#[tokio::test]
async fn put_config_rejects_invalid_toml_and_leaves_the_file_untouched() {
    let h = spawn_admin(registry()).await;
    let before = std::fs::read(&h.config_path).expect("read config");
    let hash = h.current_hash().await;

    let response = h.put_config("this is not valid toml {{{", &hash).await;
    assert_eq!(response.status(), 400);

    let after = std::fs::read(&h.config_path).expect("read config");
    assert_eq!(before, after, "a rejected apply must not touch the file");
}

/// Regression coverage for a leak `put_config_rejection_names_the_field_but_not_the_staging_path`
/// (below) does not exercise: that test's payload fails `Config::validate`
/// (a hand-written, path-free message), never `figment`'s own TOML parser.
/// A genuine TOML syntax error goes through `ConfigError::Parse`, whose
/// `message` used to be `figment::Error::to_string()` verbatim - and
/// figment's own `Display` appends `" in {source} {name}"`, where `source`
/// is the path it actually read, i.e. the staged `.tmp` file here, never
/// anything the operator wrote. The offending line/column must still come
/// through: the fix is to stop leaking the path, not to blank the message.
#[tokio::test]
async fn put_config_rejects_invalid_toml_without_leaking_the_staging_path() {
    let h = spawn_admin(registry()).await;
    let hash = h.current_hash().await;

    let response = h.put_config("this is not valid toml {{{", &hash).await;
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.expect("json");
    let message = body["error"]["message"].as_str().expect("message");

    let config_dir = h
        .config_path
        .parent()
        .expect("a parent directory")
        .to_string_lossy()
        .into_owned();
    assert!(
        !message.contains(&config_dir) && !message.contains(".tmp"),
        "the response must not leak the staging file's filesystem path: {message}"
    );
    assert!(
        message.to_lowercase().contains("line"),
        "the operator must still learn where the document is malformed: {message}"
    );
}

/// Finding 3: `PUT /admin/config` must not widen the live file's Unix
/// permissions. `std::fs::File::create` (used to stage the submitted bytes)
/// always creates with mode `0o666 & !umask`, and the rename makes the live
/// file inherit whatever mode the STAGED file has, not the original's - so
/// an operator-hardened `0600` config would otherwise become `0644` (or
/// looser) on the very first successful apply through this route.
#[cfg(unix)]
#[tokio::test]
async fn put_config_preserves_unix_permissions_across_a_successful_apply() {
    use std::os::unix::fs::PermissionsExt;

    let h = spawn_admin(registry()).await;
    std::fs::set_permissions(&h.config_path, std::fs::Permissions::from_mode(0o600))
        .expect("chmod 0600");
    let hash = h.current_hash().await;

    let updated = format!("{}\n# applied by the console\n", h.valid_config());
    let response = h.put_config(&updated, &hash).await;
    assert_eq!(response.status(), 204);

    let mode = std::fs::metadata(&h.config_path)
        .expect("stat config")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "an apply must not widen the config file's permissions"
    );
}

#[tokio::test]
async fn put_config_rejects_a_config_the_registry_cannot_build() {
    let h = spawn_admin(registry()).await;
    let before = std::fs::read(&h.config_path).expect("read config");
    let hash = h.current_hash().await;

    // A keyless provider with no base_url passes Config::validate and is only
    // caught when the registry is built. `kind = "vllm"` has no built-in
    // default base URL (unlike `"openai"`, which falls back to
    // api.openai.com and would pass registry construction too), so this is
    // the shape that actually exercises `RegistryError::MissingBaseUrl`.
    let bad = r#"
[[providers]]
name = "nowhere"
kind = "vllm"
[[providers.models]]
id = "m"
capabilities = ["chat"]
"#;
    let response = h.put_config(bad, &hash).await;
    assert_eq!(response.status(), 400);
    assert_eq!(
        std::fs::read(&h.config_path).expect("read config"),
        before,
        "registry rejection must also leave the file untouched"
    );
}

#[tokio::test]
async fn put_config_rejects_a_stale_if_match() {
    let h = spawn_admin(registry()).await;
    let before = std::fs::read(&h.config_path).expect("read config");

    let response = h
        .put_config(&h.valid_config(), "0".repeat(64).as_str())
        .await;
    assert_eq!(response.status(), 412);
    let body: Value = response.json().await.expect("json");
    assert_eq!(
        body["error"]["code"].as_str().expect("code"),
        "LM-1004",
        "a stale If-Match must be distinguishable from a plain LM-1001 400"
    );
    assert_eq!(
        std::fs::read(&h.config_path).expect("read config"),
        before,
        "a lost-update guard must not apply the write it refused"
    );
}

#[tokio::test]
async fn put_config_requires_an_if_match_header() {
    // 400, not 428: a missing header is a malformed request like any other,
    // and the console always sends one. 412 is reserved for a STALE hash,
    // which the console must distinguish because it retries after re-reading.
    let h = spawn_admin(registry()).await;
    let response = h
        .client
        .put(format!("{}/admin/config", h.base))
        .bearer_auth(master())
        .body(h.valid_config())
        .send()
        .await
        .expect("request sent");
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn put_config_applies_a_valid_document_and_keeps_a_backup() {
    let h = spawn_admin(registry()).await;
    let before = std::fs::read_to_string(&h.config_path).expect("read config");
    let hash = h.current_hash().await;

    let updated = format!("{before}\n# applied by the console\n");
    let response = h.put_config(&updated, &hash).await;
    assert_eq!(response.status(), 204);

    assert_eq!(
        std::fs::read_to_string(&h.config_path).expect("read config"),
        updated
    );

    let backup = h.config_path.with_extension("toml.bak");
    assert_eq!(
        std::fs::read_to_string(&backup).expect("read backup"),
        before,
        "the previous document is kept so the console can offer a revert"
    );
}

#[tokio::test]
async fn put_config_leaves_no_temporary_file_behind() {
    let h = spawn_admin(registry()).await;
    let hash = h.current_hash().await;
    let _ = h.put_config("not toml at all {{{", &hash).await;

    let dir = h.config_path.parent().expect("a parent directory");
    let strays: Vec<_> = std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(
        strays.is_empty(),
        "a rejected apply must clean up its staging file"
    );
}

/// Regression coverage for a check-then-rename race: an unsynchronised
/// handler could let two concurrent `PUT`s both pass the `If-Match` check
/// against the same pre-apply hash and then clobber each other's staged
/// bytes before either validated, so a caller could receive `204` while a
/// DIFFERENT document is what actually went live. `If-Match`'s whole reason
/// to exist is that two operators editing at once cannot silently lose one
/// edit, so this must hold under real concurrency, not just sequentially.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn put_config_serializes_concurrent_applies_no_lost_update() {
    let h = spawn_admin(registry()).await;
    let before = h.valid_config();
    let hash = h.current_hash().await;

    let doc_a = format!("{before}\n# applied by operator A\n");
    let doc_b = format!("{before}\n# applied by operator B\n");

    let (resp_a, resp_b) = tokio::join!(h.put_config(&doc_a, &hash), h.put_config(&doc_b, &hash));
    let status_a = resp_a.status();
    let status_b = resp_b.status();

    // Exactly one racing apply wins (204); the other must observe that the
    // hash already moved and be refused as stale (412) - never both
    // succeeding (a lost update) and never both failing (a live apply
    // starved out by its own race).
    let successes = [status_a, status_b]
        .into_iter()
        .filter(|s| *s == 204)
        .count();
    let stale = [status_a, status_b]
        .into_iter()
        .filter(|s| *s == 412)
        .count();
    assert_eq!(
        (successes, stale),
        (1, 1),
        "expected exactly one winner and one stale rejection, got {status_a} and {status_b}"
    );

    let landed = std::fs::read_to_string(&h.config_path).expect("read config");
    let winner = if status_a == 204 { &doc_a } else { &doc_b };
    assert_eq!(
        &landed, winner,
        "the live file must exactly equal the document reported as applied, \
         never a mix of the two racing writes"
    );
}

#[tokio::test]
async fn put_config_rejection_names_the_field_but_not_the_staging_path() {
    // A semantic validation failure (never a TOML parse error, so the
    // message comes from `Config::validate`, which is exactly where a
    // `ConfigError::Validation { path, message }` used to leak `path` -
    // the staged `.tmp` file's full filesystem path - into the client
    // response.
    let h = spawn_admin(registry()).await;
    let hash = h.current_hash().await;

    // A duplicate provider name (not under `[server]`, so it cannot be
    // masked by the harness's `LUMEN_SERVER__PORT` env override the way
    // `server.port = 0` would be).
    let bad = r#"
[[providers]]
name = "dup"
kind = "openai"
[[providers.models]]
id = "a"
capabilities = ["chat"]

[[providers]]
name = "dup"
kind = "openai"
[[providers.models]]
id = "b"
capabilities = ["chat"]
"#;
    let response = h.put_config(bad, &hash).await;
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.expect("json");
    let message = body["error"]["message"].as_str().expect("message");

    assert!(
        message.contains("dup"),
        "the operator must still learn which field was rejected: {message}"
    );
    let config_dir = h
        .config_path
        .parent()
        .expect("a parent directory")
        .to_string_lossy()
        .into_owned();
    assert!(
        !message.contains(&config_dir) && !message.contains(".tmp"),
        "the response must not leak the staging file's filesystem path: {message}"
    );
}
