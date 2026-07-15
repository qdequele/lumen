//! Binary-level tests for `lumen --check-config` (issue #21): CI / deploy
//! pipelines shell out to the compiled binary, so the contract that matters
//! is the actual exit code and stdout/stderr, not just the library function
//! it delegates to.

use std::io::Write;
use std::process::Command;

/// Write `toml` to a fresh temp file and return the guard (dropping it
/// deletes the file).
fn write_config(toml: &str) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().expect("create temp config file");
    file.write_all(toml.as_bytes())
        .expect("write temp config file");
    file
}

fn lumen() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lumen"))
}

#[test]
fn check_config_exits_zero_and_prints_ok_for_a_valid_config() {
    let file = write_config(
        r#"
        [[providers]]
        name = "openai-main"
        kind = "openai"

        [[providers.models]]
        id = "gpt-4o"
        capabilities = ["chat"]
        "#,
    );

    let output = lumen()
        .args(["--check-config", "--config"])
        .arg(file.path())
        .output()
        .expect("run lumen --check-config");

    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("config OK"), "stdout was: {stdout}");
    assert!(stdout.contains("1 provider"), "stdout was: {stdout}");
}

#[test]
fn check_config_exits_non_zero_and_prints_the_reason_for_an_invalid_config() {
    // server.port = 0 fails Config's own semantic validation.
    let file = write_config("[server]\nport = 0\n");

    let output = lumen()
        .args(["--check-config", "--config"])
        .arg(file.path())
        .output()
        .expect("run lumen --check-config");

    assert!(
        !output.status.success(),
        "expected a non-zero exit for an invalid config"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("port"),
        "expected the error to name the offending field: {stderr}"
    );
}

#[test]
fn check_config_exits_non_zero_for_a_provider_missing_its_required_base_url() {
    // vllm has no vendor-default base_url, so this is only caught once the
    // registry is built - proving check_config does more than just parse.
    let file = write_config(
        r#"
        [[providers]]
        name = "self-hosted"
        kind = "vllm"

        [[providers.models]]
        id = "local-model"
        capabilities = ["chat"]
        "#,
    );

    let output = lumen()
        .args(["--check-config", "--config"])
        .arg(file.path())
        .output()
        .expect("run lumen --check-config");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("base_url"), "stderr was: {stderr}");
}

#[test]
fn check_config_exits_non_zero_for_a_missing_file() {
    let output = lumen()
        .args([
            "--check-config",
            "--config",
            "/tmp/lumen-check-config-does-not-exist.toml",
        ])
        .output()
        .expect("run lumen --check-config");

    assert!(!output.status.success());
}

#[test]
fn check_config_never_binds_a_port_or_hangs() {
    // A config with a real server section: if check_config accidentally
    // booted the server it would either hang (waiting for shutdown) or bind
    // the port. Both are ruled out by the process exiting promptly.
    let file = write_config(
        r"
        [server]
        port = 8099
        ",
    );

    let output = lumen()
        .args(["--check-config", "--config"])
        .arg(file.path())
        .output()
        .expect("run lumen --check-config");

    assert!(output.status.success());
}

#[test]
fn help_text_documents_check_config() {
    let output = lumen().arg("--help").output().expect("run lumen --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--check-config"), "stdout was: {stdout}");
}
