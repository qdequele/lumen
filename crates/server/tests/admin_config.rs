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
use lumen_server::config_source::{ConfigContext, DbSource};
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
    // Written exactly once per process, before any server in this file can
    // start. `set_var` is not merely "racy with other tests that also write":
    // `PUT /admin/config` calls `Config::load` on a blocking worker, which
    // READS the environment, so a concurrent test entering `spawn_admin` and
    // writing it is the classic setenv/getenv data race. Setting the same
    // value twice does not make that safe, because the race is write-vs-read,
    // not write-vs-write. `Once` removes the writes after the first.
    static ENV_ONCE: std::sync::Once = std::sync::Once::new();
    ENV_ONCE.call_once(|| {
        // Distinctive var name (see `PROVIDER_KEY_ENV` doc comment) so setting
        // it process-wide cannot collide with any other test in the workspace.
        std::env::set_var(PROVIDER_KEY_ENV, PROVIDER_KEY_VALUE);
        // Genuinely merges into `Config.server.port` via figment (see
        // `ENV_OVERRIDE_PORT` doc comment).
        std::env::set_var(ENV_OVERRIDE_PORT, ENV_OVERRIDE_PORT_VALUE);
    });

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
        .with_config_context(Arc::new(ConfigContext::file(config_path.clone())));
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
    // `[server] port = 7777` is repeated verbatim (matching the harness's
    // own file) so this candidate exercises the registry-build rejection
    // specifically, not the boot-layer guard - an omitted `[server]` block
    // would resolve to the DEFAULT port (8080), which differs from the
    // harness's explicit 7777 and would be refused earlier, for the wrong
    // reason, by `boot_layer_diff`.
    let bad = r#"
[server]
port = 7777

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

/// Regression coverage for ADR 012 task 1's `ConfigSource` extraction: the
/// hash check must still run BEFORE the submitted body is ever staged or
/// validated, exactly like the pre-extraction inline sequence did. A request
/// that is both stale AND semantically invalid must fail on staleness alone
/// (412 `LM-1004`) - not 400, and not after writing anything to disk - the
/// same way a request that is only stale (never even glanced at the body) has
/// always behaved.
#[tokio::test]
async fn put_config_stale_if_match_wins_over_an_invalid_body() {
    let h = spawn_admin(registry()).await;
    let before = std::fs::read(&h.config_path).expect("read config");

    let response = h
        .put_config("this is not valid toml {{{", "0".repeat(64).as_str())
        .await;
    assert_eq!(
        response.status(),
        412,
        "staleness must be checked before the body is validated"
    );
    let body: Value = response.json().await.expect("json");
    assert_eq!(
        body["error"]["code"].as_str().expect("code"),
        "LM-1004",
        "a stale If-Match must win over an invalid body, not be masked by it"
    );
    assert_eq!(
        std::fs::read(&h.config_path).expect("read config"),
        before,
        "a request rejected as stale must never touch the live file"
    );

    let dir = h.config_path.parent().expect("a parent directory");
    let strays: Vec<_> = std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(
        strays.is_empty(),
        "a stale rejection must fail before staging anything, invalid body or not"
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
    // `server.port = 0` would be). `[server] port = 7777` is repeated
    // verbatim so this candidate exercises the semantic-validation rejection
    // specifically, not the boot-layer guard - an omitted `[server]` block
    // would resolve to the default port (8080) and be refused earlier, for
    // the wrong reason, by `boot_layer_diff`.
    let bad = r#"
[server]
port = 7777

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

// ---- File mode: boot-layer guard (Task 6) ----------------------------------

/// (a) A PUT that changes `server.port` (a boot-layer field, ADR 012 §1) is
/// refused with `LM-1001` naming the changed key: only a restart picks up a
/// new bind port, and this route promises everything it accepts applies
/// immediately.
#[tokio::test]
async fn put_config_rejects_a_changed_restart_only_key() {
    let h = spawn_admin(registry()).await;
    let before = std::fs::read(&h.config_path).expect("read config");
    let hash = h.current_hash().await;

    let changed_port = h.valid_config().replace("port = 7777", "port = 9999");
    assert_ne!(
        changed_port,
        h.valid_config(),
        "the replacement must actually apply"
    );

    let response = h.put_config(&changed_port, &hash).await;
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"].as_str().expect("code"), "LM-1001");
    let message = body["error"]["message"].as_str().expect("message");
    assert!(
        message.contains("server.port"),
        "the rejection must name the changed boot-layer key: {message}"
    );
    assert_eq!(
        std::fs::read(&h.config_path).expect("read config"),
        before,
        "a boot-layer rejection must leave the file untouched"
    );
}

/// (b) A PUT whose `[server]` block is byte-for-byte UNCHANGED, but which
/// adds a new `[[providers]]` entry (a dynamic-layer change), is accepted:
/// the whole file is the candidate in file mode, so an unchanged boot block
/// must not itself trigger the restart-only guard.
#[tokio::test]
async fn put_config_accepts_an_unchanged_boot_layer_with_a_new_provider() {
    let h = spawn_admin(registry()).await;
    let hash = h.current_hash().await;

    let updated = format!(
        "{}\n[[providers]]\nname = \"second\"\nkind = \"openai\"\n[[providers.models]]\nid = \"m2\"\ncapabilities = [\"chat\"]\n",
        h.valid_config()
    );
    let response = h.put_config(&updated, &hash).await;
    assert_eq!(response.status(), 204);
    assert_eq!(
        std::fs::read_to_string(&h.config_path).expect("read config"),
        updated
    );
}

// ---- DB mode: GET/PUT /admin/config over ConfigContext::db (Task 6) -------

/// A thin harness mirroring [`Harness`] but over a `ConfigContext::db`, so
/// the db-mode config pipeline can be exercised the same way the file-mode
/// tests above exercise `Harness`.
struct DbHarness {
    base: String,
    client: reqwest::Client,
    /// Kept alive for the harness's lifetime: `boot_path` lives inside this
    /// directory, and db-mode `validate_document`/`load_config` read it off
    /// disk (`Config::load_with_dynamic`) on every PUT - dropping the
    /// directory early would make every apply fail with a missing boot file.
    _dir: TempDir,
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
}

/// Build an auth-enabled `AppState` over a `ConfigContext::db`, so the
/// db-mode config pipeline can be exercised without ever going through
/// `main.rs`'s boot sequence. The boot file sets `auth.enabled = true` only
/// (no `[server]` block, so it never contributes a boot-layer key to a
/// `boot_layer_diff` - see the module doc comment on that function: the diff
/// only ever compares the two DOCUMENT TEXTS being loaded/applied through
/// `ConfigSource`, never `ctx.boot_path` on disk).
async fn spawn_admin_db_mode(registry: Arc<Registry>) -> DbHarness {
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

    let dir = TempDir::new().expect("create temp dir");
    let boot_path = dir.path().join("boot.toml");
    std::fs::write(&boot_path, "[auth]\nenabled = true\n").expect("write boot file");
    let ctx = Arc::new(ConfigContext::db(boot_path, DbSource::new(store)));

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
    }
}

/// A valid dynamic-only document: no boot-layer keys at all, matching what a
/// well-behaved db-mode candidate always looks like.
const DB_MODE_VALID_DOC: &str = r#"
[[providers]]
name = "db-provider"
kind = "openai"
[[providers.models]]
id = "gpt-4o"
capabilities = ["chat"]
"#;

#[tokio::test]
async fn get_config_returns_empty_document_before_first_put_in_db_mode() {
    let h = spawn_admin_db_mode(registry()).await;
    let body: Value = h.get("/admin/config").await.json().await.expect("json");
    assert_eq!(body["config"].as_str().expect("config string"), "");
    assert_eq!(
        body["hash"].as_str().expect("hash"),
        lumen_server::config_source::empty_doc_hash(),
        "an empty document must hash the same way an empty file would"
    );
}

/// (c) The full db-mode round trip: GET before any PUT returns the empty
/// document and its hash; a PUT with that hash as `If-Match` applies and
/// returns 204; GET afterward returns exactly what was applied; and a second
/// PUT reusing the now-stale original hash is refused as `LM-1004`.
#[tokio::test]
async fn put_config_round_trips_in_db_mode() {
    let h = spawn_admin_db_mode(registry()).await;
    let empty_hash = h.current_hash().await;
    assert_eq!(empty_hash, lumen_server::config_source::empty_doc_hash());

    let response = h.put_config(DB_MODE_VALID_DOC, &empty_hash).await;
    assert_eq!(response.status(), 204);

    let body: Value = h.get("/admin/config").await.json().await.expect("json");
    assert_eq!(
        body["config"].as_str().expect("config string"),
        DB_MODE_VALID_DOC
    );
    let applied_hash = body["hash"].as_str().expect("hash").to_owned();
    assert_ne!(applied_hash, empty_hash);

    // Reusing the stale (pre-apply) hash must be refused, never silently
    // reapplied or accepted as a no-op.
    let stale_response = h.put_config(DB_MODE_VALID_DOC, &empty_hash).await;
    assert_eq!(stale_response.status(), 412);
    let stale_body: Value = stale_response.json().await.expect("json");
    assert_eq!(
        stale_body["error"]["code"].as_str().expect("code"),
        "LM-1004"
    );
}

/// (d) A db-mode candidate containing a boot-layer key (`[server] port`)
/// that differs from the built-in default is refused with the same
/// restart-only `LM-1001`, even though the current (empty) dynamic document
/// has no boot keys of its own to compare against - the comparison is
/// against the DEFAULT a boot-key-free document resolves to.
#[tokio::test]
async fn put_config_rejects_restart_only_boot_layer_keys_in_db_mode() {
    let h = spawn_admin_db_mode(registry()).await;
    let empty_hash = h.current_hash().await;

    let response = h.put_config("[server]\nport = 1\n", &empty_hash).await;
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"].as_str().expect("code"), "LM-1001");
    let message = body["error"]["message"].as_str().expect("message");
    assert!(
        message.contains("server.port"),
        "the rejection must name the changed boot-layer key: {message}"
    );

    // Confirm nothing was persisted: GET still reports the empty document.
    let after: Value = h.get("/admin/config").await.json().await.expect("json");
    assert_eq!(after["config"].as_str().expect("config string"), "");
}
