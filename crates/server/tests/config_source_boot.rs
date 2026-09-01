//! End-to-end boot tests for ADR 012's `config_source = "db"` mode: the real
//! `lumen` binary, spawned as a child process, so the actual `main.rs` boot
//! sequence runs (not just the library-level `ConfigContext` unit tests in
//! `crates/server/src/reload.rs`). Mirrors `tests/signal_shutdown.rs`'s
//! spawn-the-binary pattern rather than `tests/common`'s in-process harness,
//! since `main::run`'s DB-mode branch (open the store early, load the
//! dynamic document, merge it with the boot file) is not reachable from a
//! library call - only from the compiled binary's own `main`.

use std::io::Read;
use std::net::TcpListener as StdTcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use lumen_auth::store::KeyStore;
use lumen_server::config_source::{config_hash, ConfigSource, DbSource, EMPTY_DOC};

/// A valid master-key value (64 hex chars): the admin bearer token and the
/// at-rest encryption key for stored provider keys.
fn master_key() -> String {
    "a".repeat(64)
}

/// Bind an ephemeral port and immediately release it so `lumen` can bind it
/// instead (same small TOCTOU tradeoff `tests/signal_shutdown.rs` accepts,
/// for the same reason: `lumen` is a separate process taking host:port from
/// its config file, so the listener can't be created here and handed over).
fn free_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("read local addr").port()
}

/// Write `contents` to a fresh file under the test binary's scratch directory
/// (`CARGO_TARGET_TMPDIR`, cargo-provided) and return its path. `unique`
/// disambiguates concurrently-running tests in this file.
fn write_temp_config(unique: &str, contents: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let path = dir.join(format!("config-source-boot-{unique}.toml"));
    std::fs::write(&path, contents).expect("write temp config");
    path
}

/// A DB-mode boot-only document: no dynamic keys at all (see
/// `ensure_boot_only`), auth enabled with `db_path` under the scratch dir.
fn db_mode_boot_config(port: u16, db_path: &std::path::Path) -> String {
    // `config_source` MUST come before any `[table]` header: a bare key
    // after one belongs to that table in TOML, not to the document root.
    format!(
        r#"
config_source = "db"

[server]
host = "127.0.0.1"
port = {port}

[auth]
enabled = true
db_path = "{db}"
"#,
        db = db_path.display()
    )
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

/// Drain and return the child's stdout/stderr - for asserting on log lines,
/// and (on failure) for showing a CI log why boot didn't behave.
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

#[tokio::test]
async fn db_mode_boots_with_no_stored_config_and_warns() {
    let port = free_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("lumen.db");
    let config = write_temp_config("empty-doc", &db_mode_boot_config(port, &db_path));

    let mut child = lumen()
        .args(["--config"])
        .arg(&config)
        .env("LUMEN_MASTER_KEY", master_key())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lumen");

    let base = format!("http://127.0.0.1:{port}");
    let ready = wait_until_ready(&base, Duration::from_secs(10)).await;

    let _ = child.kill();
    let _ = child.wait();
    let (out, err) = drain_output(child);

    assert!(
        ready,
        "db mode with an empty config_versions table must still boot and \
         answer /health\n--- stdout ---\n{out}\n--- stderr ---\n{err}"
    );
    // The exact warn message from main.rs's DB-mode boot branch (ADR 012):
    // an empty stored document is a valid, if provider-less, boot state, and
    // the operator is told how to install a real one.
    assert!(
        out.contains("config_source = \"db\" and no config stored yet; PUT /admin/config to install one")
            || err.contains("config_source = \"db\" and no config stored yet; PUT /admin/config to install one"),
        "expected the empty-doc warning in the process output\n--- stdout ---\n{out}\n--- stderr ---\n{err}"
    );
}

#[tokio::test]
async fn db_mode_boots_a_preseeded_document_and_serves_its_model() {
    let port = free_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("lumen.db");
    let config = write_temp_config("preseeded", &db_mode_boot_config(port, &db_path));

    // Pre-seed the config_versions table BEFORE the server ever boots -
    // exactly the state a fleet-managed, file-less deployment starts from
    // (ADR 012 §4: the API is the only way to install a document, and here a
    // direct `ConfigSource::persist` stands in for a prior `PUT
    // /admin/config` against a previous instance of this same database).
    let store = KeyStore::connect(&format!("sqlite://{}", db_path.display()))
        .await
        .expect("open store to pre-seed");
    let source = DbSource::new(store.clone());
    let doc = r#"
[[providers]]
name = "openai"
kind = "openai"

[[providers.models]]
id = "gpt-preseeded"
capabilities = ["chat"]
"#;
    source
        .persist(doc, &config_hash(EMPTY_DOC.as_bytes()))
        .await
        .expect("pre-seed the config document");

    // `/v1/*` requires a virtual key once auth is enabled; mint one straight
    // against the DB (the same offline bootstrap `lumen keys create` uses)
    // so the request below can authenticate.
    let (plaintext, _record) = store
        .create_key(lumen_auth::store::NewKey {
            name: "boot-test".to_owned(),
            ..lumen_auth::store::NewKey::default()
        })
        .await
        .expect("pre-seed a virtual key");

    let mut child = lumen()
        .args(["--config"])
        .arg(&config)
        .env("LUMEN_MASTER_KEY", master_key())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lumen");

    let base = format!("http://127.0.0.1:{port}");
    let ready = wait_until_ready(&base, Duration::from_secs(10)).await;
    assert!(ready, "lumen did not become ready in db mode");

    let models: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/v1/models"))
        .bearer_auth(plaintext.reveal())
        .send()
        .await
        .expect("GET /v1/models")
        .json()
        .await
        .expect("parse models json");
    let ids: Vec<&str> = models["data"]
        .as_array()
        .expect("data array")
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        ids.contains(&"gpt-preseeded"),
        "the model from the pre-seeded DB document must be routable at boot, got {ids:?}"
    );
}

#[tokio::test]
async fn db_mode_without_auth_enabled_refuses_to_boot() {
    // ADR 012 §1: `config_source = "db"` requires a database, i.e.
    // `auth.enabled = true`. Without it there is nowhere for the dynamic
    // document to live, so boot must fail fast rather than silently falling
    // back to an empty config.
    let port = free_port();
    let config = write_temp_config(
        "no-auth",
        &format!(
            r#"
config_source = "db"

[server]
host = "127.0.0.1"
port = {port}
"#
        ),
    );

    let mut child = lumen()
        .args(["--config"])
        .arg(&config)
        .env("LUMEN_MASTER_KEY", master_key())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lumen");

    // It must not become ready: give it a bounded window, then check it
    // already exited (rather than racing a slow failure against the ready
    // check).
    let base = format!("http://127.0.0.1:{port}");
    let ready = wait_until_ready(&base, Duration::from_secs(3)).await;

    // Taken before the blocking `wait()` below moves `child`: needed to read
    // the process's output afterward, and `Child::wait()` itself only
    // reaps the exit status, not the piped output.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join wait")
        .expect("wait for exit");
    let mut out = String::new();
    let mut err = String::new();
    if let Some(mut pipe) = stdout_pipe.take() {
        let _ = pipe.read_to_string(&mut out);
    }
    if let Some(mut pipe) = stderr_pipe.take() {
        let _ = pipe.read_to_string(&mut err);
    }

    assert!(
        !ready,
        "db mode without auth.enabled must never come up and answer /health"
    );
    assert!(
        !status.success(),
        "boot must fail (non-zero exit), not hang or succeed"
    );
    // The actual `anyhow::ensure!` message from `boot_config_context`, not
    // just a bare non-zero exit: pins the FAILURE REASON, not merely that
    // *something* went wrong (a config parse error, a bind failure, etc.
    // would also exit non-zero, but only this message names the real cause).
    // `tracing`'s default writer is stdout (confirmed empirically: this
    // message lands there, never on stderr, since logging is already
    // initialised by the time `run` fails and reports through
    // `tracing::error!`, not a bare `eprintln!`), so that is what this
    // asserts against - `err` is captured anyway as a diagnostic in the
    // failure message, in case that ever changes.
    assert!(
        out.contains("config_source = \"db\" requires [auth] enabled = true with a database"),
        "expected the auth-required message in stdout\n--- stdout ---\n{out}\n--- stderr ---\n{err}"
    );
}

/// A stored document that itself sets `[auth]` must not silently win over
/// the boot file's own value: `Config::load_with_dynamic` merges the stored
/// document OVER the boot layer, so without `boot_config_context`'s explicit
/// re-assertion, a document like this would leave the process with auth
/// OFF - `boot_auth_stack` never runs, `/admin` never mounts, `/v1/*` runs as
/// an open proxy - even though the boot file (and the `anyhow::ensure!` at
/// the top of DB-mode boot) required `auth.enabled = true` a moment earlier.
/// Boot must refuse instead, naming the stored document as the cause.
#[tokio::test]
async fn db_mode_refuses_to_boot_when_the_stored_document_disables_auth() {
    let port = free_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("lumen.db");
    let config = write_temp_config("auth-override", &db_mode_boot_config(port, &db_path));

    let store = KeyStore::connect(&format!("sqlite://{}", db_path.display()))
        .await
        .expect("open store to pre-seed");
    let source = DbSource::new(store);
    source
        .persist(
            "[auth]\nenabled = false\n",
            &config_hash(EMPTY_DOC.as_bytes()),
        )
        .await
        .expect("pre-seed a document that disables auth");

    let mut child = lumen()
        .args(["--config"])
        .arg(&config)
        .env("LUMEN_MASTER_KEY", master_key())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lumen");

    let base = format!("http://127.0.0.1:{port}");
    let ready = wait_until_ready(&base, Duration::from_secs(3)).await;

    let mut stdout_pipe = child.stdout.take();
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join wait")
        .expect("wait for exit");
    let mut out = String::new();
    if let Some(mut pipe) = stdout_pipe.take() {
        let _ = pipe.read_to_string(&mut out);
    }

    assert!(
        !ready,
        "a stored document that disables auth must never let db mode come up as an open proxy"
    );
    assert!(!status.success(), "boot must fail, not silently continue");
    assert!(
        out.contains("the stored config document sets [auth] enabled = false"),
        "expected the actionable stored-document message in stdout, got: {out}"
    );
}

/// The `db_path` sibling of the test above: a stored document repointing
/// `auth.db_path` away from the boot file's own value must also be refused,
/// not silently followed (it is just as much a restart-only boot-layer value
/// as `auth.enabled`).
#[tokio::test]
async fn db_mode_refuses_to_boot_when_the_stored_document_repoints_db_path() {
    let port = free_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("lumen.db");
    let config = write_temp_config("db-path-override", &db_mode_boot_config(port, &db_path));

    let store = KeyStore::connect(&format!("sqlite://{}", db_path.display()))
        .await
        .expect("open store to pre-seed");
    let source = DbSource::new(store);
    let elsewhere = dir.path().join("elsewhere.db");
    source
        .persist(
            &format!("[auth]\ndb_path = \"{}\"\n", elsewhere.display()),
            &config_hash(EMPTY_DOC.as_bytes()),
        )
        .await
        .expect("pre-seed a document that repoints db_path");

    let mut child = lumen()
        .args(["--config"])
        .arg(&config)
        .env("LUMEN_MASTER_KEY", master_key())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lumen");

    let base = format!("http://127.0.0.1:{port}");
    let ready = wait_until_ready(&base, Duration::from_secs(3)).await;

    let mut stdout_pipe = child.stdout.take();
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join wait")
        .expect("wait for exit");
    let mut out = String::new();
    if let Some(mut pipe) = stdout_pipe.take() {
        let _ = pipe.read_to_string(&mut out);
    }

    assert!(
        !ready,
        "a stored document that repoints auth.db_path must never let db mode boot"
    );
    assert!(!status.success(), "boot must fail, not silently continue");
    assert!(
        out.contains("sets auth.db_path to"),
        "expected the actionable stored-document message in stdout, got: {out}"
    );
}
