//! ADR 015 end to end on the real binary: a key in a Lab-linked lease spends,
//! usage events reach a mock Lab, a grant from the event's group snapshot
//! keeps the key serving, and SIGTERM delivers the final flush.
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::Read;
use std::net::TcpListener as StdTcpListener;
use std::process::{Child, Command, Stdio};
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

/// On failure, drain and print the child's stdout/stderr so a CI log shows
/// *why* startup or shutdown didn't behave - a bare "assertion failed" here
/// gives no signal about a config or port problem.
fn dump_output(mut child: Child, label: &str) {
    let mut out = String::new();
    let mut err = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut err);
    }
    eprintln!("--- {label} stdout ---\n{out}\n--- {label} stderr ---\n{err}");
}

const MASTER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ACCOUNT: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";

/// The mock Lab accepts every event id it receives.
struct AcceptAll;
impl Respond for AcceptAll {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).expect("events body is JSON");
        let ids: Vec<Value> = body["events"]
            .as_array()
            .expect("events array")
            .iter()
            .map(|e| e["id"].clone())
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({ "accepted": ids }))
    }
}

async fn lab_events(lab: &MockServer) -> Vec<Value> {
    lab.received_requests()
        .await
        .expect("request recording is on")
        .iter()
        .flat_map(|r| {
            serde_json::from_slice::<Value>(&r.body).expect("events body is JSON")["events"]
                .as_array()
                .expect("events array")
                .clone()
        })
        .collect()
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
    Mock::given(method("POST"))
        .and(path("/internal/events"))
        .respond_with(AcceptAll)
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
        .env("E2E_USAGE_EVENTS_SECRET", "e2e-secret")
        .env("E2E_UPSTREAM_KEY", "sk-test")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lumen binary");
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

    // Wait for the flush (200 ms) and the sender poll (2 s).
    let deadline = Instant::now() + Duration::from_secs(10);
    let events = loop {
        let events = lab_events(&lab).await;
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
    if !status.success() {
        dump_output(child, "usage-events-e2e");
        panic!("{status:?}");
    }
    assert_eq!(billed(&lab_events(&lab).await), 6_000_000);
}

#[tokio::test]
async fn a_missing_signing_secret_refuses_to_boot() {
    let port = free_port();
    let db = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("usage-events-nosecret.db");
    remove_db_files(&db);
    let config = write_temp_config(
        "nosecret",
        &format!(
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
        ),
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_lumen"))
        .arg("--config")
        .arg(&config)
        .env("LUMEN_MASTER_KEY", MASTER)
        .env_remove("E2E_UNSET_USAGE_EVENTS_SECRET")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lumen binary");
    // A binary that boots anyway would serve forever: bound the wait and kill
    // it so a regression fails the test instead of hanging it.
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child status") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("lumen booted without its signing secret");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(!status.success());
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    // `main` reports boot errors through `tracing::error!`, whose subscriber
    // writes to stdout, so the diagnostic is looked up on both streams.
    let text = format!("{stdout}{stderr}");
    assert!(text.contains("E2E_UNSET_USAGE_EVENTS_SECRET"), "{text}");
}
