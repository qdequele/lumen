//! ADR 015 end to end on the real binary: a key in a Lab-linked lease spends,
//! usage events reach a mock Lab (after one rejected delivery), a grant from
//! the event's group snapshot keeps the key serving, SIGTERM delivers the
//! final flush, and the signing secret never reaches the logs. Boot refuses a
//! missing or blank secret and a non-UUID `account_ref` on a live group.
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::Read;
use std::net::TcpListener as StdTcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Bind an ephemeral port and immediately release it so `lumen` can bind it
/// instead. Small TOCTOU race in principle; in practice fine for a
/// single-host test suite (the same pattern `common::spawn_state` avoids
/// only because it can bind the listener itself and hand it to `serve()`
/// in-process - not possible here since `lumen` is a separate process that
/// takes a host:port from its config file).
fn free_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("read local addr").port()
}

/// Write `contents` to a fresh file under the test binary's scratch
/// directory (`CARGO_TARGET_TMPDIR`, cargo-provided - no extra crate needed)
/// and return its path. `unique` disambiguates concurrently-running tests in
/// this file.
fn write_temp_config(unique: &str, contents: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let path = dir.join(format!("usage-events-e2e-{unique}.toml"));
    std::fs::write(&path, contents).expect("write temp config");
    path
}

/// Poll `GET {base}/health` until it answers 200 or `timeout` elapses.
async fn wait_until_ready(base: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(resp) = reqwest::get(format!("{base}/health")).await {
            if resp.status().is_success() {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "lumen did not become ready within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Send `sig` to `child`'s pid. Safety: `kill(2)` on a pid we own (we just
/// spawned it) with a standard termination signal is the documented,
/// non-memory-unsafe use of this call; the only "unsafe" part is the FFI
/// boundary itself.
fn send_signal(child: &Child, sig: libc::c_int) {
    let pid = i32::try_from(child.id()).expect("child pid fits in pid_t");
    let rc = unsafe { libc::kill(pid, sig) };
    assert_eq!(rc, 0, "kill(2) failed: {}", std::io::Error::last_os_error());
}

/// Wait for `child` to exit within `timeout`, polling rather than blocking
/// the async runtime on a synchronous `wait()`. Returns the exit status.
async fn wait_for_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("poll child status") {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "lumen did not exit within {timeout:?} of the signal"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Read a child pipe to the end on its own thread, so a verbose child
/// (`RUST_LOG=debug`) never blocks on a full pipe while the test runs.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    })
}

/// Join both drain threads: the child's stdout and stderr, concatenated.
/// `main` reports boot errors through `tracing::error!`, whose subscriber
/// writes to stdout, so callers look things up on both streams.
fn collect(stdout: JoinHandle<String>, stderr: JoinHandle<String>) -> String {
    let out = stdout.join().expect("stdout drain thread");
    let err = stderr.join().expect("stderr drain thread");
    format!("--- stdout ---\n{out}\n--- stderr ---\n{err}")
}

const MASTER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ACCOUNT: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";
/// The events signing secret: distinctive, so a leak into any log is found.
const SECRET: &str = "e2e-signing-secret-7f3a9c";

/// The mock Lab accepts every event id it receives and records the events it
/// acknowledged, so a rejected batch is never counted as billed.
#[derive(Clone, Default)]
struct AcceptAll {
    accepted: Arc<Mutex<Vec<Value>>>,
}
impl Respond for AcceptAll {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).expect("events body is JSON");
        let events = body["events"].as_array().expect("events array").clone();
        let ids: Vec<Value> = events.iter().map(|e| e["id"].clone()).collect();
        self.accepted
            .lock()
            .expect("accepted events lock")
            .extend(events);
        ResponseTemplate::new(200).set_body_json(json!({ "accepted": ids }))
    }
}

impl AcceptAll {
    /// Every distinct event the Lab acknowledged, first sighting first. The
    /// Lab deduplicates on `id`, so a re-sent event is billed once.
    fn events(&self) -> Vec<Value> {
        let mut seen = std::collections::HashSet::new();
        self.accepted
            .lock()
            .expect("accepted events lock")
            .iter()
            .filter(|e| seen.insert(e["id"].as_str().expect("event id").to_owned()))
            .cloned()
            .collect()
    }
}

/// Remove a SQLite file and its WAL sidecars: a stale `-wal` next to a fresh
/// database is a "disk I/O error" at open.
fn remove_db_files(db: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = db.as_os_str().to_owned();
        name.push(suffix);
        let _ = std::fs::remove_file(std::path::PathBuf::from(name));
    }
}

fn billed(events: &[Value]) -> i64 {
    events
        .iter()
        .map(|e| {
            e["data"]["cost_micro_usd"]
                .as_i64()
                .expect("cost_micro_usd")
        })
        .sum()
}

// One linear scenario against a real process: splitting it would hide the order.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn a_lease_is_billed_topped_up_and_flushed_on_shutdown() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "object": "chat.completion", "created": 1, "model": "gpt",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1000, "completion_tokens": 1000, "total_tokens": 2000}
        })))
        .mount(&upstream)
        .await;
    let lab = MockServer::start().await;
    // The first delivery is rejected with 401, so the sender's error-log
    // branch runs under `RUST_LOG=debug` before the happy path.
    Mock::given(method("POST"))
        .and(path("/internal/events"))
        .respond_with(ResponseTemplate::new(401))
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&lab)
        .await;
    let accept = AcceptAll::default();
    Mock::given(method("POST"))
        .and(path("/internal/events"))
        .respond_with(accept.clone())
        .mount(&lab)
        .await;

    let port = free_port();
    let db = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("usage-events-e2e.db");
    remove_db_files(&db);
    let config = write_temp_config(
        "billed",
        &format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[auth]
enabled = true
db_path = "{db}"
flush_interval_ms = 200

[usage_events]
url = "{lab}"
signing_key_env = "E2E_USAGE_EVENTS_SECRET"
source = "e2e"

[[providers]]
name = "mock"
kind = "openai"
api_key_env = "E2E_UPSTREAM_KEY"
base_url = "{upstream}"

[[providers.models]]
id = "gpt"
upstream_id = "gpt"
capabilities = ["chat"]
cost_per_1m_input = 1000.0
cost_per_1m_output = 1000.0
"#,
            db = db.display(),
            lab = lab.uri(),
            upstream = upstream.uri(),
        ),
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_lumen"))
        .arg("--config")
        .arg(&config)
        .env("LUMEN_MASTER_KEY", MASTER)
        .env("E2E_USAGE_EVENTS_SECRET", SECRET)
        .env("E2E_UPSTREAM_KEY", "sk-test")
        .env("RUST_LOG", "debug")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lumen binary");
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let base = format!("http://127.0.0.1:{port}");
    wait_until_ready(&base, Duration::from_secs(10)).await;
    let http = reqwest::Client::new();

    // Lease of $5; each call costs $2 (1000 in + 1000 out tokens at $1000/1M).
    let group: Value = http
        .post(format!("{base}/admin/groups"))
        .bearer_auth(MASTER)
        .json(&json!({"name": "acme", "budget_max": 5.0, "account_ref": ACCOUNT}))
        .send()
        .await
        .expect("create group")
        .json()
        .await
        .expect("group json");
    let gid = group["id"].as_str().expect("group id").to_owned();
    let key: Value = http
        .post(format!("{base}/admin/keys"))
        .bearer_auth(MASTER)
        .json(&json!({"name": "k", "group_id": gid, "external_ref": "lab-key-1"}))
        .send()
        .await
        .expect("create key")
        .json()
        .await
        .expect("key json");
    let plain = key["key"].as_str().expect("plaintext key").to_owned();
    let chat = || {
        http.post(format!("{base}/v1/chat/completions"))
            .bearer_auth(&plain)
            .json(&json!({"model": "gpt", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
    };

    assert_eq!(chat().await.expect("first call").status(), 200);
    assert_eq!(chat().await.expect("second call").status(), 200);

    // Wait for the flush (200 ms), the rejected first delivery, its 2 s
    // backoff and the sender poll (2 s).
    let deadline = Instant::now() + Duration::from_secs(20);
    let events = loop {
        let events = accept.events();
        if billed(&events) >= 4_000_000 {
            break events;
        }
        assert!(
            Instant::now() < deadline,
            "events not delivered: {events:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let last = events.last().expect("at least one event");
    assert_eq!(last["account_id"], ACCOUNT);
    assert_eq!(last["api_key_id"], "lab-key-1");
    assert_eq!(last["data"]["source"], "e2e");
    assert!(lab.received_requests().await.expect("recorded")[0]
        .headers
        .contains_key("x-lab-signature"));

    // The lease has $1 left: the Lab tops up from the event's snapshot.
    let remaining = last["data"]["group"]["budget_max_micro"]
        .as_i64()
        .expect("budget_max_micro")
        - last["data"]["group"]["spent_micro"]
            .as_i64()
            .expect("spent_micro");
    assert!(remaining < 2_000_000);
    let grant = http
        .post(format!("{base}/admin/groups/{gid}/grant"))
        .bearer_auth(MASTER)
        .json(&json!({"amount": 10.0}))
        .send()
        .await
        .expect("grant");
    assert_eq!(grant.status(), 200);
    assert_eq!(
        chat().await.expect("third call").status(),
        200,
        "topped-up lease keeps serving"
    );

    // SIGTERM: the final flush bills the third call and the sender delivers it.
    send_signal(&child, libc::SIGTERM);
    let status = wait_for_exit(&mut child, Duration::from_secs(15)).await;
    // Always drain both streams: they are checked for the secret below.
    let output = collect(stdout, stderr);
    assert!(status.success(), "{status:?}\n{output}");
    assert_eq!(billed(&accept.events()), 6_000_000);

    // The 401 branch ran (it is the one that names the signing secret) and
    // no log line at debug level carries the secret's value.
    assert!(
        output.contains("usage events rejected with 401"),
        "the 401 path did not run:\n{output}"
    );
    assert!(
        !output.contains(SECRET),
        "the signing secret leaked into the logs"
    );
}

/// Spawn the binary on `toml` with `env`, expect it to exit non-zero within
/// 10 s (a binary that boots anyway is killed so a regression fails instead
/// of hanging), and return its combined output.
async fn boot_failure_output(unique: &str, toml: &str, env: &[(&str, &str)]) -> String {
    let config = write_temp_config(unique, toml);
    let mut command = Command::new(env!("CARGO_BIN_EXE_lumen"));
    command
        .arg("--config")
        .arg(&config)
        .env("LUMEN_MASTER_KEY", MASTER)
        .env_remove("E2E_UNSET_USAGE_EVENTS_SECRET")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in env {
        command.env(name, value);
    }
    let mut child = command.spawn().expect("spawn lumen binary");
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child status") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "lumen booted when it must refuse:\n{}",
                collect(stdout, stderr)
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let output = collect(stdout, stderr);
    assert!(!status.success(), "{output}");
    output
}

#[tokio::test]
async fn a_missing_or_blank_signing_secret_refuses_to_boot() {
    for (unique, env) in [
        ("nosecret", &[][..]),
        (
            "blanksecret",
            &[("E2E_UNSET_USAGE_EVENTS_SECRET", " \t ")][..],
        ),
    ] {
        let port = free_port();
        let db = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("usage-events-{unique}.db"));
        remove_db_files(&db);
        let toml = format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[auth]
enabled = true
db_path = "{db}"

[usage_events]
url = "https://lab.example"
signing_key_env = "E2E_UNSET_USAGE_EVENTS_SECRET"
source = "e2e"
"#,
            db = db.display()
        );
        let output = boot_failure_output(unique, &toml, env).await;
        assert!(
            output.contains("E2E_UNSET_USAGE_EVENTS_SECRET"),
            "{unique}: {output}"
        );
    }
}

#[tokio::test]
async fn a_non_uuid_account_ref_refuses_to_boot_with_usage_events() {
    let db = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("usage-events-badref.db");
    remove_db_files(&db);
    // A group written while billing was off, when any opaque ref is valid.
    let store = lumen_auth::store::KeyStore::connect(&format!("sqlite://{}", db.display()))
        .await
        .expect("open auth db");
    let group = store
        .create_group(lumen_auth::store::NewGroup {
            name: "legacy".to_owned(),
            budget_max: None,
            account_ref: Some("acme".to_owned()),
        })
        .await
        .expect("create group");
    store.pool().close().await;

    let port = free_port();
    let toml = format!(
        r#"
[server]
host = "127.0.0.1"
port = {port}

[auth]
enabled = true
db_path = "{db}"

[usage_events]
url = "https://lab.example"
signing_key_env = "E2E_BADREF_USAGE_EVENTS_SECRET"
source = "e2e"
"#,
        db = db.display()
    );
    let output = boot_failure_output(
        "badref",
        &toml,
        &[("E2E_BADREF_USAGE_EVENTS_SECRET", SECRET)],
    )
    .await;
    assert!(output.contains(&group.id), "names the group: {output}");
    assert!(
        !output.contains("acme"),
        "names only group ids, never the ref: {output}"
    );
}
