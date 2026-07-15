//! Real-signal graceful shutdown (issue #27): `tests/shutdown.rs` proves
//! `serve()`'s draining behaviour by injecting a oneshot future as the
//! "shutdown" signal, but never exercises the actual OS signal path in
//! `lifecycle::shutdown_signal` (`tokio::signal::ctrl_c` /
//! `tokio::signal::unix::signal(SignalKind::terminate())`). This file spawns
//! the real `lumen` binary as a child process and sends it a genuine
//! SIGTERM/SIGINT, asserting the same "in-flight request completes, then the
//! process exits 0" behaviour end to end.
//!
//! Unix-only: signal delivery by `kill(2)` has no portable equivalent, and
//! `lifecycle::shutdown_signal` itself only installs a SIGTERM handler under
//! `#[cfg(unix)]` (Windows falls back to Ctrl-C alone).
#![cfg(unix)]

use serde_json::json;
use std::io::Read;
use std::net::TcpListener as StdTcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
    let path = dir.join(format!("signal-shutdown-{unique}.toml"));
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

#[tokio::test]
async fn sigterm_drains_inflight_request_then_exits_cleanly() {
    // A deliberately slow upstream stands in for a long provider call, so
    // there is a genuine in-flight request for the SIGTERM to race against.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "object": "chat.completion",
                    "id": "chatcmpl-sigterm-test",
                    "created": 1,
                    "model": "gpt",
                    "choices": [{
                        "index": 0,
                        "message": { "role": "assistant", "content": "done" },
                        "finish_reason": "stop"
                    }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
                }))
                .set_delay(Duration::from_millis(300)),
        )
        .mount(&upstream)
        .await;

    let port = free_port();
    let config = write_temp_config(
        "sigterm",
        &format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[[providers]]
name = "mock"
kind = "openai"
api_key_env = "SIGNAL_SHUTDOWN_TEST_API_KEY"
base_url = "{upstream_uri}"

[[providers.models]]
id = "gpt"
upstream_id = "gpt"
capabilities = ["chat"]
"#,
            upstream_uri = upstream.uri(),
        ),
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_lumen"))
        .arg("--config")
        .arg(&config)
        .env("SIGNAL_SHUTDOWN_TEST_API_KEY", "sk-test")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lumen binary");

    let base = format!("http://127.0.0.1:{port}");
    wait_until_ready(&base, Duration::from_secs(10)).await;

    // Kick off the slow request before signalling shutdown.
    let request_base = base.clone();
    let request = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{request_base}/v1/chat/completions"))
            .json(&json!({ "model": "gpt", "messages": [{ "role": "user", "content": "hi" }] }))
            .send()
            .await
    });

    // Let the request reach the handler (and the upstream mock) before the
    // real SIGTERM lands.
    tokio::time::sleep(Duration::from_millis(50)).await;
    send_signal(&child, libc::SIGTERM);

    // The in-flight request must still complete successfully - the real
    // signal handler must trigger the same graceful drain `serve()`'s
    // injected-oneshot tests already prove, not an abrupt kill.
    let resp = request
        .await
        .expect("request task panicked")
        .expect("request failed");
    assert_eq!(resp.status(), 200, "in-flight request must complete 200");
    let body: serde_json::Value = resp.json().await.expect("parse response json");
    assert_eq!(body["choices"][0]["message"]["content"], "done");

    // ...and the process itself exits 0 (well within the 30s hard drain
    // deadline - the only in-flight work was the 300ms mock delay).
    let status = wait_for_exit(&mut child, Duration::from_secs(10)).await;
    if !status.success() {
        dump_output(child, "sigterm");
        panic!("lumen did not exit cleanly after SIGTERM: {status:?}");
    }
}

#[tokio::test]
async fn sigint_shuts_down_promptly_with_no_inflight_requests() {
    let port = free_port();
    // No providers needed: /health touches neither the DB nor providers.
    let config = write_temp_config(
        "sigint",
        &format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}
"#
        ),
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_lumen"))
        .arg("--config")
        .arg(&config)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lumen binary");

    let base = format!("http://127.0.0.1:{port}");
    wait_until_ready(&base, Duration::from_secs(10)).await;

    send_signal(&child, libc::SIGINT);

    // With nothing in flight, the real signal path should shut down promptly
    // rather than idling out to the 30s hard drain deadline - a tight bound
    // here (well under that deadline) proves the signal itself drove the
    // exit, not the hard timeout.
    let status = wait_for_exit(&mut child, Duration::from_secs(5)).await;
    if !status.success() {
        dump_output(child, "sigint");
        panic!("lumen did not exit cleanly after SIGINT: {status:?}");
    }

    // A new connection now fails - the process is gone.
    let after = reqwest::get(format!("{base}/health")).await;
    assert!(after.is_err(), "expected connection to fail post-shutdown");
}
