//! ADR 014 migration: old `fallbacks` / Jev `rerank` configs become virtual models.

use lumen_server::config::Config;
use lumen_server::config_migrate::{hint_for, migrate_document, MigrateError};

fn loads(text: &str) -> Config {
    Config::load_text(text, "migrated")
        .unwrap_or_else(|e| panic!("migrated config must validate: {e}\n{text}"))
}

#[test]
fn simple_fallbacks_become_a_virtual_model_and_comments_survive() {
    let doc = r#"# my gateway
[[providers]]
name = "openai"
kind = "openai"
[[providers.models]]
id = "gpt-4o"
capabilities = ["chat"]
fallbacks = ["claude"]

[[providers]]
name = "anthropic"
kind = "anthropic"
[[providers.models]]
id = "claude"
capabilities = ["chat"]
"#;
    let m = migrate_document(doc).unwrap();
    assert!(m.changed);
    assert!(m.text.starts_with("# my gateway"));
    assert!(!m.text.contains("fallbacks"));
    let cfg = loads(&m.text);
    let vm = cfg
        .virtual_models
        .iter()
        .find(|v| v.id == "gpt-4o")
        .unwrap();
    let targets: Vec<&str> = vm.targets.iter().map(|t| t.model.as_str()).collect();
    assert_eq!(targets, vec!["openai/gpt-4o", "claude"]);
    assert!(cfg.providers[0]
        .models
        .iter()
        .any(|m| m.id == "openai/gpt-4o"));
}

#[test]
fn a_renamed_model_keeps_sending_its_old_upstream_id() {
    // `gpt-4o` has no `upstream_id`, so it sent `gpt-4o` upstream. Renamed to
    // `openai/gpt-4o`, it must still send `gpt-4o`, not its new id; an
    // explicit `upstream_id` is kept as is.
    let doc = r#"
[[providers]]
name = "openai"
kind = "openai"
[[providers.models]]
id = "gpt-4o"
capabilities = ["chat"]
fallbacks = ["mini"]
[[providers.models]]
id = "mini"
upstream_id = "gpt-4o-mini-2024-07-18"
capabilities = ["chat"]
fallbacks = ["gpt-4o"]
"#;
    let cfg = loads(&migrate_document(doc).unwrap().text);
    let upstream = |id: &str| {
        cfg.providers[0]
            .models
            .iter()
            .find(|m| m.id == id)
            .unwrap()
            .resolved_upstream_id()
            .to_owned()
    };
    assert_eq!(upstream("openai/gpt-4o"), "gpt-4o");
    assert_eq!(upstream("openai/mini"), "gpt-4o-mini-2024-07-18");

    let legacy: Config = toml::from_str(doc).unwrap();
    let hint = hint_for(&legacy, "gpt-4o").unwrap();
    assert!(hint.contains("upstream_id = \"gpt-4o\""), "{hint}");
    let hint = hint_for(&legacy, "mini").unwrap();
    assert!(!hint.contains("upstream_id"), "{hint}");
}

#[test]
fn mutual_fallbacks_migrate_without_a_cycle() {
    let doc = r#"
[[providers]]
name = "p"
kind = "openai"
[[providers.models]]
id = "a"
capabilities = ["chat"]
fallbacks = ["b"]
[[providers.models]]
id = "b"
capabilities = ["chat"]
fallbacks = ["a"]
"#;
    let cfg = loads(&migrate_document(doc).unwrap().text);
    let targets = |id: &str| -> Vec<String> {
        cfg.virtual_models
            .iter()
            .find(|v| v.id == id)
            .unwrap()
            .targets
            .iter()
            .map(|t| t.model.clone())
            .collect()
    };
    assert_eq!(targets("a"), vec!["p/a", "p/b"]);
    assert_eq!(targets("b"), vec!["p/b", "p/a"]);
}

#[test]
fn a_jev_reranker_becomes_a_remap_on_the_sibling_systemone_model() {
    let doc = r#"
[[providers]]
name = "typesafe"
kind = "typesafe"
api_key_env = "TYPESAFE_API_KEY"
[[providers.models]]
id = "jev"
upstream_id = "jev-latest"
capabilities = ["systemone"]
cost_per_1m_input = 0.042
[[providers.models]]
id = "jev-rerank"
upstream_id = "jev-latest"
capabilities = ["rerank"]
[providers.models.rerank]
instructions = "Could `document` be the cited precedent?"
"#;
    let m = migrate_document(doc).unwrap();
    let cfg = loads(&m.text);
    assert!(
        cfg.providers[0].models.iter().all(|m| m.id != "jev-rerank"),
        "the foundation reranker is gone"
    );
    let vm = cfg
        .virtual_models
        .iter()
        .find(|v| v.id == "jev-rerank")
        .unwrap();
    assert_eq!(vm.targets[0].model, "jev");
    let remap = vm.targets[0].remap.as_ref().unwrap();
    assert_eq!(
        remap.instructions.as_deref(),
        Some("Could `document` be the cited precedent?")
    );
}

#[test]
fn a_lone_jev_reranker_gets_a_new_systemone_foundation_model_with_its_price() {
    let doc = r#"
[[providers]]
name = "typesafe"
kind = "typesafe"
api_key_env = "TYPESAFE_API_KEY"
[[providers.models]]
id = "jev-rerank"
upstream_id = "jev-latest"
capabilities = ["rerank"]
cost_per_1m_input = 0.042
"#;
    let cfg = loads(&migrate_document(doc).unwrap().text);
    let created = cfg.providers[0]
        .models
        .iter()
        .find(|m| m.id == "typesafe/jev-latest")
        .unwrap();
    assert_eq!(
        created.capabilities,
        vec![lumen_core::Capability::SystemOne]
    );
    assert_eq!(created.cost_per_1m_input, Some(0.042));
    assert_eq!(
        cfg.virtual_models[0].targets[0].model,
        "typesafe/jev-latest"
    );
}

#[test]
fn a_multi_capability_model_is_noted() {
    let doc = r#"
[[providers]]
name = "cohere"
kind = "cohere"
[[providers.models]]
id = "embed-multi"
capabilities = ["embed", "rerank"]
fallbacks = ["other"]
[[providers.models]]
id = "other"
capabilities = ["embed", "rerank"]
"#;
    let m = migrate_document(doc).unwrap();
    assert!(
        m.notes
            .iter()
            .any(|n| n.contains("embed-multi") && n.contains("rerank")),
        "{:?}",
        m.notes
    );
    loads(&m.text);
}

#[test]
fn a_computed_id_that_already_exists_is_a_conflict() {
    let doc = r#"
[[providers]]
name = "openai"
kind = "openai"
[[providers.models]]
id = "gpt-4o"
capabilities = ["chat"]
fallbacks = ["openai/gpt-4o"]
[[providers.models]]
id = "openai/gpt-4o"
capabilities = ["chat"]
"#;
    match migrate_document(doc) {
        Err(MigrateError::Conflict(message)) => assert!(message.contains("openai/gpt-4o")),
        other => panic!("expected a conflict, got {other:?}"),
    }
}

#[test]
fn a_config_without_legacy_fields_is_unchanged() {
    let doc = "[[providers]]\nname = \"openai\"\nkind = \"openai\"\n";
    let m = migrate_document(doc).unwrap();
    assert!(!m.changed);
    assert_eq!(m.text, doc);
}

#[test]
fn a_fallback_naming_a_removed_jev_reranker_gets_its_remap_inline() {
    let doc = r#"
[[providers]]
name = "cohere"
kind = "cohere"
[[providers.models]]
id = "rerank-v3"
capabilities = ["rerank"]
fallbacks = ["jev-rerank"]
[[providers]]
name = "typesafe"
kind = "typesafe"
api_key_env = "TYPESAFE_API_KEY"
[[providers.models]]
id = "jev-rerank"
upstream_id = "jev-latest"
capabilities = ["rerank"]
[providers.models.rerank]
instructions = "Is `document` relevant?"
"#;
    let cfg = loads(&migrate_document(doc).unwrap().text);
    let vm = cfg
        .virtual_models
        .iter()
        .find(|v| v.id == "rerank-v3")
        .unwrap();
    assert_eq!(vm.targets[0].model, "cohere/rerank-v3");
    assert!(vm.targets[0].remap.is_none());
    assert_eq!(vm.targets[1].model, "typesafe/jev-latest");
    assert_eq!(
        vm.targets[1]
            .remap
            .as_ref()
            .unwrap()
            .instructions
            .as_deref(),
        Some("Is `document` relevant?")
    );
}

#[test]
fn hint_for_names_the_replacement() {
    let doc = r#"
[[providers]]
name = "p"
kind = "openai"
[[providers.models]]
id = "a"
capabilities = ["chat"]
fallbacks = ["b"]
[[providers.models]]
id = "b"
capabilities = ["chat"]
"#;
    let cfg: Config = toml::from_str(doc).unwrap();
    let hint = hint_for(&cfg, "a").unwrap();
    assert!(hint.contains("'p/a'"), "{hint}");
    assert!(hint.contains("[[virtual_models]]"), "{hint}");
    assert!(hint_for(&cfg, "b").is_none());
}

const TYPESAFE: &str =
    "[[providers]]\nname = \"typesafe\"\nkind = \"typesafe\"\napi_key_env = \"TYPESAFE_API_KEY\"\n";

fn typesafe_doc(models: &str) -> String {
    format!("{TYPESAFE}{models}")
}

#[test]
fn two_lone_jev_rerankers_over_one_upstream_share_one_created_model() {
    let doc = typesafe_doc(
        r#"[[providers.models]]
id = "rerank-a"
upstream_id = "jev-latest"
capabilities = ["rerank"]
cost_per_1m_input = 0.042
[providers.models.rerank]
instructions = "Question A?"
[[providers.models]]
id = "rerank-b"
upstream_id = "jev-latest"
capabilities = ["rerank"]
cost_per_1m_input = 0.05
[providers.models.rerank]
instructions = "Question B?"
"#,
    );
    let m = migrate_document(&doc).unwrap();
    let cfg = loads(&m.text);
    let created: Vec<_> = cfg.providers[0]
        .models
        .iter()
        .filter(|m| m.id == "typesafe/jev-latest")
        .collect();
    assert_eq!(created.len(), 1);
    assert_eq!(cfg.providers[0].models.len(), 1, "both rerankers are gone");
    for id in ["rerank-a", "rerank-b"] {
        let vm = cfg.virtual_models.iter().find(|v| v.id == id).unwrap();
        assert_eq!(vm.targets[0].model, "typesafe/jev-latest");
    }
    assert!(
        m.notes
            .iter()
            .any(|n| n.contains("rerank-b") && n.contains("typesafe/jev-latest")),
        "differing prices are noted: {:?}",
        m.notes
    );
}

#[test]
fn a_sibling_that_is_itself_renamed_is_targeted_by_its_new_id() {
    // The reranker comes first so the result cannot depend on document order.
    let doc = typesafe_doc(
        r#"[[providers.models]]
id = "jev-rerank"
upstream_id = "jev-latest"
capabilities = ["rerank"]
[[providers.models]]
id = "jev"
upstream_id = "jev-latest"
capabilities = ["systemone"]
fallbacks = ["jev-backup"]
[[providers.models]]
id = "jev-backup"
upstream_id = "jev-backup"
capabilities = ["systemone"]
"#,
    );
    let cfg = loads(&migrate_document(&doc).unwrap().text);
    let vm = cfg
        .virtual_models
        .iter()
        .find(|v| v.id == "jev-rerank")
        .unwrap();
    assert_eq!(vm.targets[0].model, "typesafe/jev");
    assert!(vm.targets[0].remap.is_some());
}

#[test]
fn a_systemone_fallback_naming_a_systemone_rerank_jev_model_has_no_remap() {
    let doc = typesafe_doc(
        r#"[[providers.models]]
id = "s"
upstream_id = "jev-small"
capabilities = ["systemone"]
fallbacks = ["jev"]
[[providers.models]]
id = "jev"
upstream_id = "jev-latest"
capabilities = ["systemone", "rerank"]
"#,
    );
    let cfg = loads(&migrate_document(&doc).unwrap().text);
    let s = cfg.virtual_models.iter().find(|v| v.id == "s").unwrap();
    let targets: Vec<&str> = s.targets.iter().map(|t| t.model.as_str()).collect();
    assert_eq!(targets, vec!["typesafe/s", "typesafe/jev"]);
    assert!(s.targets.iter().all(|t| t.remap.is_none()));
    let jev = cfg.virtual_models.iter().find(|v| v.id == "jev").unwrap();
    assert_eq!(jev.targets[0].model, "typesafe/jev");
    assert!(jev.targets[0].remap.is_some());
}

#[test]
fn every_price_of_a_lone_reranker_is_carried_including_searches() {
    let doc = typesafe_doc(
        r#"[[providers.models]]
id = "jev-rerank"
upstream_id = "jev-latest"
capabilities = ["rerank"]
cost_per_1m_input = 0.042
cost_per_1m_output = 0.1
cost_per_1k_searches = 2.5
"#,
    );
    let m = migrate_document(&doc).unwrap();
    let cfg = loads(&m.text);
    let created = &cfg.providers[0].models[0];
    assert_eq!(created.cost_per_1m_input, Some(0.042));
    assert_eq!(created.cost_per_1m_output, Some(0.1));
    assert_eq!(created.cost_per_1k_searches, Some(2.5));
    assert!(m.notes.is_empty(), "nothing was lost: {:?}", m.notes);
}

#[test]
fn a_price_that_cannot_be_carried_to_the_sibling_is_noted() {
    let doc = typesafe_doc(
        r#"[[providers.models]]
id = "jev"
upstream_id = "jev-latest"
capabilities = ["systemone"]
cost_per_1m_input = 0.042
[[providers.models]]
id = "jev-rerank"
upstream_id = "jev-latest"
capabilities = ["rerank"]
cost_per_1k_searches = 2.5
"#,
    );
    let m = migrate_document(&doc).unwrap();
    loads(&m.text);
    assert!(
        m.notes
            .iter()
            .any(|n| n.contains("jev-rerank") && n.contains("cost_per_1k_searches")),
        "{:?}",
        m.notes
    );
}

#[test]
fn a_renamed_systemone_rerank_jev_model_tells_systemone_clients_the_new_id() {
    let doc = typesafe_doc(
        r#"[[providers.models]]
id = "jev"
upstream_id = "jev-latest"
capabilities = ["systemone", "rerank"]
"#,
    );
    let m = migrate_document(&doc).unwrap();
    loads(&m.text);
    assert!(
        m.notes.iter().any(|n| n.contains("'jev'")
            && n.contains("systemone")
            && n.contains("'typesafe/jev'")),
        "{:?}",
        m.notes
    );
}

#[test]
fn hint_for_a_renamed_systemone_rerank_model_says_to_drop_rerank() {
    let doc = typesafe_doc(
        r#"[[providers.models]]
id = "jev"
upstream_id = "jev-latest"
capabilities = ["systemone", "rerank"]
[providers.models.rerank]
instructions = "Q?"
"#,
    );
    let cfg: Config = toml::from_str(&doc).unwrap();
    let hint = hint_for(&cfg, "jev").unwrap();
    assert!(hint.contains("'typesafe/jev'"), "{hint}");
    assert!(hint.contains("remove `rerank`"), "{hint}");
    assert!(hint.contains("[providers.models.rerank]"), "{hint}");
}

#[test]
fn hint_for_a_lone_reranker_says_to_create_the_systemone_model() {
    let doc = typesafe_doc(
        r#"[[providers.models]]
id = "jev-rerank"
upstream_id = "jev-latest"
capabilities = ["rerank"]
cost_per_1m_input = 0.042
cost_per_1k_searches = 2.5
"#,
    );
    let cfg: Config = toml::from_str(&doc).unwrap();
    let hint = hint_for(&cfg, "jev-rerank").unwrap();
    assert!(hint.contains("[[providers.models]]"), "{hint}");
    assert!(hint.contains("id = \"typesafe/jev-latest\""), "{hint}");
    assert!(hint.contains("upstream_id = \"jev-latest\""), "{hint}");
    assert!(hint.contains("capabilities = [\"systemone\"]"), "{hint}");
    assert!(hint.contains("cost_per_1m_input = 0.042"), "{hint}");
    assert!(hint.contains("cost_per_1k_searches = 2.5"), "{hint}");
    assert!(hint.contains("remove that foundation model"), "{hint}");
}

/// R10: the boot error, the reload log and the admin `LM-1001` body all carry
/// the `Config::load_text` error, so the hint must never copy the operator's
/// remap text. `lumen config migrate` still carries it into the document.
#[test]
fn the_legacy_rerank_hint_never_copies_remap_text() {
    let doc = typesafe_doc(
        r#"[[providers.models]]
id = "jev"
upstream_id = "jev-latest"
capabilities = ["systemone", "rerank"]
[providers.models.rerank]
instructions = "SENTINEL-INSTRUCTIONS"
criteria.true = "SENTINEL-YES"
criteria.false = "SENTINEL-NO"
"#,
    );
    let err = Config::load_text(&doc, "legacy").unwrap_err().to_string();
    for sentinel in ["SENTINEL-INSTRUCTIONS", "SENTINEL-YES", "SENTINEL-NO"] {
        assert!(!err.contains(sentinel), "{sentinel} leaked: {err}");
    }
    assert!(
        err.contains("<copy from your [providers.models.rerank] block>"),
        "{err}"
    );
    assert!(err.contains("[[virtual_models]]"), "{err}");

    let m = migrate_document(&doc).unwrap();
    for sentinel in ["SENTINEL-INSTRUCTIONS", "SENTINEL-YES", "SENTINEL-NO"] {
        assert!(m.text.contains(sentinel), "{sentinel} missing: {}", m.text);
    }
}

/// A legacy field with no virtual-model equivalent (a rerank block on a
/// non-typesafe model) prints no dangling "Equivalent:" clause.
#[test]
fn a_legacy_field_without_an_equivalent_has_no_equivalent_clause() {
    let doc = r#"
[[providers]]
name = "p"
kind = "cohere"
api_key_env = "KEY"
[[providers.models]]
id = "m"
capabilities = ["rerank"]
[providers.models.rerank]
instructions = "SENTINEL-INSTRUCTIONS"
"#;
    let cfg: Config = toml::from_str(doc).unwrap();
    assert!(hint_for(&cfg, "m").is_none());
    let err = Config::load_text(doc, "legacy").unwrap_err().to_string();
    assert!(err.contains("lumen config migrate"), "{err}");
    assert!(!err.contains("Equivalent"), "{err}");
    assert!(!err.contains("SENTINEL-INSTRUCTIONS"), "{err}");
}

mod cli {
    use std::process::Command;

    const LEGACY: &str = "[[providers]]\nname = \"p\"\nkind = \"openai\"\n[[providers.models]]\nid = \"a\"\ncapabilities = [\"chat\"]\nfallbacks = [\"b\"]\n[[providers.models]]\nid = \"b\"\ncapabilities = [\"chat\"]\n";

    fn migrate(path: &std::path::Path, extra: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_lumen"))
            .args(["config", "migrate", "-c"])
            .arg(path)
            .args(extra)
            .output()
            .unwrap()
    }

    #[test]
    fn dry_run_prints_without_writing_and_a_real_run_backs_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, LEGACY).unwrap();

        let out = migrate(&path, &["--dry-run"]);
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("[[virtual_models]]"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), LEGACY);
        assert!(!dir.path().join("config.toml.bak").exists());

        let out = migrate(&path, &[]);
        assert!(out.status.success());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml.bak")).unwrap(),
            LEGACY
        );
        let migrated = std::fs::read_to_string(&path).unwrap();
        assert!(migrated.contains("[[virtual_models]]") && !migrated.contains("fallbacks"));

        let out = migrate(&path, &[]);
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("nothing to migrate"));
    }

    #[cfg(unix)]
    #[test]
    fn a_real_run_keeps_the_file_mode_and_rewrites_through_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        std::fs::write(&real, LEGACY).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let out = migrate(&link, &[]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::read_to_string(&real)
            .unwrap()
            .contains("[[virtual_models]]"));
        let mode = std::fs::metadata(&real).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
