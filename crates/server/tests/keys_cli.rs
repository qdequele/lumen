//! Binary-level tests for `lumen keys create` / `lumen keys list` (issue #68):
//! the offline bootstrap path shells out to the compiled binary, so the
//! contract that matters is exit codes, stdout JSON and stderr text - not the
//! library functions behind them.

use std::io::Write;
use std::process::Command;

use lumen_auth::key::hash_key;
use lumen_auth::store::KeyStore;

/// A valid master key value (64 hex chars).
fn master_key() -> String {
    "a".repeat(64)
}

/// Write an auth-enabled config whose SQLite file lives in `dir`, and return
/// the config path plus the db path.
fn write_auth_config(dir: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let db_path = dir.path().join("keys-cli.db");
    let config_path = dir.path().join("config.toml");
    let toml = format!(
        "[auth]\nenabled = true\ndb_path = \"{}\"\n",
        db_path.display()
    );
    let mut file = std::fs::File::create(&config_path).expect("create temp config");
    file.write_all(toml.as_bytes()).expect("write temp config");
    (config_path, db_path)
}

fn lumen() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lumen"))
}

#[tokio::test]
async fn keys_create_persists_a_key_that_authenticates_by_hash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (config_path, db_path) = write_auth_config(&dir);

    let output = lumen()
        .args(["keys", "create", "--config"])
        .arg(&config_path)
        .args([
            "--name",
            "team-search",
            "--budget-max",
            "50",
            "--rpm-limit",
            "60",
            "--tpm-limit",
            "100000",
        ])
        .env("LUMEN_MASTER_KEY", master_key())
        .output()
        .expect("run lumen keys create");

    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // stdout is the one-time plaintext plus the record, as one JSON object
    // (the same shape as the `POST /admin/keys` response).
    let stdout = String::from_utf8_lossy(&output.stdout);
    let created: serde_json::Value = serde_json::from_str(&stdout).expect("stdout must be JSON");
    let key = created["key"].as_str().expect("`key` must be a string");
    assert!(key.starts_with("fg-"), "unexpected key shape: {key}");
    assert_eq!(created["name"], "team-search");
    assert_eq!(created["budget_max"], 50.0);
    assert_eq!(created["rpm_limit"], 60);
    assert_eq!(created["tpm_limit"], 100_000);

    // The plaintext never reaches stderr (the only other output stream).
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains(key),
        "plaintext key leaked to stderr: {stderr}"
    );

    // The key row is really in the configured database, addressable by the
    // hash of the printed plaintext - i.e. it will authenticate at next boot.
    let store = KeyStore::connect(&format!("sqlite://{}", db_path.display()))
        .await
        .expect("open the auth db the CLI wrote");
    let record = store
        .find_by_hash(&hash_key(key))
        .await
        .expect("query by hash")
        .expect("created key must be findable by its hash");
    assert_eq!(record.name, "team-search");
    assert_eq!(created["id"], record.id.as_str());
}

#[test]
fn keys_list_shows_records_but_never_plaintext_or_hashes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (config_path, _db_path) = write_auth_config(&dir);

    let create = lumen()
        .args(["keys", "create", "--config"])
        .arg(&config_path)
        .args(["--name", "bootstrap"])
        .env("LUMEN_MASTER_KEY", master_key())
        .output()
        .expect("run lumen keys create");
    assert!(create.status.success());
    let created: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&create.stdout)).expect("create JSON");
    let plaintext = created["key"].as_str().expect("plaintext").to_owned();

    let list = lumen()
        .args(["keys", "list", "--config"])
        .arg(&config_path)
        .env("LUMEN_MASTER_KEY", master_key())
        .output()
        .expect("run lumen keys list");
    assert!(
        list.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&list.stderr)
    );

    let stdout = String::from_utf8_lossy(&list.stdout);
    let records: serde_json::Value = serde_json::from_str(&stdout).expect("list must be JSON");
    let records = records.as_array().expect("list must be a JSON array");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["name"], "bootstrap");
    assert_eq!(records[0]["id"], created["id"]);

    // Records only: no plaintext, no hash column.
    assert!(
        !stdout.contains(&plaintext),
        "plaintext key leaked into `keys list` output"
    );
    assert!(
        !stdout.contains(&hash_key(&plaintext)),
        "key hash leaked into `keys list` output"
    );
}

#[test]
fn keys_create_requires_the_master_key_env() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (config_path, _db_path) = write_auth_config(&dir);

    let output = lumen()
        .args(["keys", "create", "--config"])
        .arg(&config_path)
        .args(["--name", "nope"])
        .env_remove("LUMEN_MASTER_KEY")
        .output()
        .expect("run lumen keys create");

    assert!(
        !output.status.success(),
        "creating a key without the master key must fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("LUMEN_MASTER_KEY"),
        "stderr must name the missing env var: {stderr}"
    );
}

#[test]
fn keys_create_refuses_a_config_with_auth_disabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[auth]\nenabled = false\n").expect("write config");

    let output = lumen()
        .args(["keys", "create", "--config"])
        .arg(&config_path)
        .args(["--name", "nope"])
        .env("LUMEN_MASTER_KEY", master_key())
        .output()
        .expect("run lumen keys create");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("auth"),
        "stderr must explain that auth is disabled: {stderr}"
    );
}

#[test]
fn keys_list_requires_the_master_key_env() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (config_path, _db_path) = write_auth_config(&dir);

    let output = lumen()
        .args(["keys", "list", "--config"])
        .arg(&config_path)
        .env_remove("LUMEN_MASTER_KEY")
        .output()
        .expect("run lumen keys list");

    assert!(
        !output.status.success(),
        "listing keys without the master key must fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("LUMEN_MASTER_KEY"),
        "stderr must name the missing env var: {stderr}"
    );
}

#[test]
fn keys_parse_errors_exit_2_and_print_the_keys_help() {
    // Missing --name is a parse error: exit code 2 (like every arg error)
    // and the KEYS help on stderr, not the top-level server help.
    let output = lumen()
        .args(["keys", "create"])
        .output()
        .expect("run lumen keys create with no name");

    assert_eq!(
        output.status.code(),
        Some(2),
        "arg errors must exit with code 2"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--name"),
        "stderr must name the missing flag: {stderr}"
    );
    assert!(
        stderr.contains("lumen keys create --name"),
        "stderr must show the keys usage, not the server usage: {stderr}"
    );
    assert!(
        !stderr.contains("--check-config"),
        "the top-level server help must not be printed for a keys error: {stderr}"
    );
}

#[test]
fn keys_create_rejects_non_finite_and_negative_budgets_at_the_binary_level() {
    for bad in ["NaN", "inf", "-5"] {
        let output = lumen()
            .args(["keys", "create", "--name", "x", "--budget-max", bad])
            .output()
            .expect("run lumen keys create with a bad budget");
        assert_eq!(
            output.status.code(),
            Some(2),
            "'--budget-max {bad}' must be refused with exit code 2"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--budget-max"),
            "stderr for '{bad}' must name the flag: {stderr}"
        );
    }
}

#[test]
fn help_text_documents_the_keys_subcommand() {
    let output = lumen().arg("--help").output().expect("run lumen --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("keys"), "stdout was: {stdout}");

    let output = lumen()
        .args(["keys", "--help"])
        .output()
        .expect("run lumen keys --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("create"), "stdout was: {stdout}");
    assert!(stdout.contains("list"), "stdout was: {stdout}");
}
