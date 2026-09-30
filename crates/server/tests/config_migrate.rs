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
}
