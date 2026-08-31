//! Format-preserving TOML config document editors (ADR 012 Task 7).
//!
//! Pure text-in, text-out: parse a config document with `toml_edit`, perform
//! one targeted edit, and reserialize - preserving every comment and
//! formatting choice OUTSIDE the edited table, unlike a round trip through
//! `toml::Value` (which has no concept of comments or key order at all).
//!
//! Consumed by the admin config-editing handlers: each editor takes the
//! current document's text and returns the new text, ready to hand to
//! `ConfigContext::validate_document` and then `ConfigSource::persist`. This
//! module never validates the resulting document itself - a syntactically
//! sound but semantically invalid edit (e.g. two providers with the same
//! name) is caught by that later validation pass, not here.

use toml_edit::{ArrayOfTables, DocumentMut, Item};

use crate::config::ProviderConfig;

/// Failure parsing a config document's TOML text, or serializing a value
/// into one.
///
/// Never reaches an HTTP client directly: this module is pure text-to-text
/// with no handler of its own, so the caller (the admin config-editing
/// handlers) maps this onto the existing `GatewayError` taxonomy, which
/// carries the stable `LM-xxxx` codes documented in `docs/errors.md` - the
/// same convention `config_source::ConfigSourceError` follows.
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    /// `doc` (or the result of an edit) is not valid TOML.
    #[error("config document is not valid TOML: {0}")]
    Parse(#[from] toml_edit::TomlError),
    /// A value could not be serialized into a TOML item.
    #[error("failed to serialize value into the config document: {0}")]
    Serialize(#[from] toml_edit::ser::Error),
    /// The `providers` key exists but is not an array of tables, so no
    /// provider entry could be located, inserted or removed.
    #[error("`providers` is not an array of tables")]
    ProvidersNotArray,
}

/// Insert or replace the `[[providers]]` entry named `provider.name`.
///
/// Matches by the `name` field: if a provider with that name already exists
/// in the `providers` array of tables, its entry is replaced in place (so
/// its position among the other providers is preserved); otherwise
/// `provider` is appended as a new `[[providers]]` table. A document with no
/// `providers` key yet gets one. Comments and formatting elsewhere in the
/// document (`[server]`, other providers, ...) are untouched.
///
/// Nested array-of-struct fields (`provider.models`) serialize as an inline
/// array of inline tables (`models = [{ id = "...", ... }, ...]`), not the
/// exploded `[[providers.models]]` table form - `toml_edit::ser` never
/// chooses the latter on its own. This round-trips correctly (see the
/// `upsert_provider_round_trips_nested_models` test) but reads differently
/// from a hand-written config file that uses `[[providers.models]]`.
///
/// # Errors
/// [`EditError::Parse`] if `doc` is not valid TOML. [`EditError::Serialize`]
/// if `provider` cannot be serialized into a TOML table.
/// [`EditError::ProvidersNotArray`] if the document's `providers` key exists
/// but holds something other than an array of tables.
pub fn upsert_provider(doc: &str, provider: &ProviderConfig) -> Result<String, EditError> {
    let mut document = doc.parse::<DocumentMut>()?;
    let new_table = toml_edit::ser::to_document(provider)?.as_table().clone();

    let providers = providers_array_mut(&mut document)?;
    let existing_index = (0..providers.len())
        .find(|&i| provider_name_at(providers, i) == Some(provider.name.as_str()));

    match existing_index {
        Some(i) => {
            providers.replace(i, new_table);
        }
        None => providers.push(new_table),
    }

    Ok(document.to_string())
}

/// Remove the `[[providers]]` entry named `name`, if any.
///
/// Returns `Ok(None)` when no provider with that name exists in the document
/// (the caller maps this to the standard admin 404 envelope, like every
/// other unknown-id lookup). Comments and formatting elsewhere in the
/// document are untouched.
///
/// # Errors
/// [`EditError::Parse`] if `doc` is not valid TOML.
/// [`EditError::ProvidersNotArray`] if the document's `providers` key exists
/// but holds something other than an array of tables.
pub fn delete_provider(doc: &str, name: &str) -> Result<Option<String>, EditError> {
    let mut document = doc.parse::<DocumentMut>()?;
    let providers = providers_array_mut(&mut document)?;
    let index = (0..providers.len()).find(|&i| provider_name_at(providers, i) == Some(name));

    match index {
        Some(i) => {
            providers.remove(i);
            Ok(Some(document.to_string()))
        }
        None => Ok(None),
    }
}

/// Replace (or insert) the named top-level table wholesale, e.g.
/// `replace_section(doc, "tokenizer", &cfg.tokenizer)`.
///
/// The entire existing table at `section` (if any) is discarded and replaced
/// with `value`'s serialized form; there is no field-level merge. Comments
/// and formatting elsewhere in the document are untouched. As with
/// [`upsert_provider`], a nested array-of-struct field on `value` serializes
/// as an inline array of inline tables, not `[[section.field]]`.
///
/// # Errors
/// [`EditError::Parse`] if `doc` is not valid TOML. [`EditError::Serialize`]
/// if `value` cannot be serialized into a TOML table.
pub fn replace_section(
    doc: &str,
    section: &str,
    value: &impl serde::Serialize,
) -> Result<String, EditError> {
    let mut document = doc.parse::<DocumentMut>()?;
    let new_table = toml_edit::ser::to_document(value)?.as_table().clone();
    document
        .as_table_mut()
        .insert(section, Item::Table(new_table));
    Ok(document.to_string())
}

/// The `name` field of every `[[providers]]` entry, in document order.
///
/// # Errors
/// [`EditError::Parse`] if `doc` is not valid TOML.
/// [`EditError::ProvidersNotArray`] if the document's `providers` key exists
/// but holds something other than an array of tables.
pub fn provider_names(doc: &str) -> Result<Vec<String>, EditError> {
    let document = doc.parse::<DocumentMut>()?;
    let Some(item) = document.as_table().get("providers") else {
        return Ok(Vec::new());
    };
    let providers = item
        .as_array_of_tables()
        .ok_or(EditError::ProvidersNotArray)?;
    Ok(providers
        .iter()
        .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
        .map(str::to_owned)
        .collect())
}

/// The `providers` array of tables, creating an empty one if the document
/// has no `providers` key yet.
fn providers_array_mut(document: &mut DocumentMut) -> Result<&mut ArrayOfTables, EditError> {
    let item = document
        .as_table_mut()
        .entry("providers")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    item.as_array_of_tables_mut()
        .ok_or(EditError::ProvidersNotArray)
}

/// The `name` field of the table at `index` in `providers`, if present and a
/// string. `None` for an out-of-range index or a table with no string
/// `name`, never a panic.
fn provider_name_at(providers: &ArrayOfTables, index: usize) -> Option<&str> {
    providers.get(index)?.get("name")?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{TokenizerConfig, TokenizerMode};

    const DOC: &str = "# fleet config\n[server]\nport = 8080 # keep\n\n\
[[providers]]\nname = \"openai\"\nkind = \"openai\"\n\n\
[[providers]]\nname = \"ollama\"\nkind = \"ollama\"\n";

    #[test]
    fn upsert_replaces_matching_provider_and_keeps_comments() {
        let p: ProviderConfig =
            toml::from_str("name = \"ollama\"\nkind = \"ollama\"\nbase_url = \"http://o:11434\"")
                .unwrap();
        let out = upsert_provider(DOC, &p).unwrap();
        assert!(out.contains("# fleet config"));
        assert!(out.contains("port = 8080 # keep"));
        assert!(out.contains("base_url = \"http://o:11434\""));
        assert_eq!(provider_names(&out).unwrap(), ["openai", "ollama"]);
    }

    #[test]
    fn upsert_appends_when_no_matching_provider() {
        let p: ProviderConfig = toml::from_str("name = \"cohere\"\nkind = \"cohere\"").unwrap();
        let out = upsert_provider(DOC, &p).unwrap();
        assert_eq!(
            provider_names(&out).unwrap(),
            ["openai", "ollama", "cohere"]
        );
        assert!(out.contains("# fleet config"));
    }

    /// Pins that a nested `Vec<ModelConfig>` (an array of structs, not a
    /// scalar) survives `upsert_provider` intact: `toml_edit::ser` writes it
    /// as an inline array of inline tables (see `upsert_provider`'s doc
    /// comment), and this asserts the round trip is semantically lossless -
    /// deserializing the resulting document back to `Vec<ModelConfig>`
    /// yields the exact value that went in, not comparing TOML text (which
    /// would be brittle to the inline-vs-exploded form). A future
    /// `toml_edit` upgrade that silently drops or reorders a nested field
    /// would fail this test.
    #[test]
    fn upsert_provider_round_trips_nested_models() {
        let toml_src = r#"
            name = "openai"
            kind = "openai"

            [[models]]
            id = "gpt-4o"
            capabilities = ["chat"]
            cost_per_1m_input = 2.5

            [[models]]
            id = "gpt-4o-mini"
            capabilities = ["chat", "embed"]
            upstream_id = "gpt-4o-mini-2024"
        "#;
        let p: ProviderConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(p.models.len(), 2, "fixture sanity check");

        let out = upsert_provider(DOC, &p).unwrap();

        let cfg: crate::config::Config = toml::from_str(&out).unwrap();
        let openai = cfg
            .providers
            .iter()
            .find(|pr| pr.name == "openai")
            .expect("the replaced openai entry is still present");
        assert_eq!(openai.models, p.models);
    }

    #[test]
    fn delete_provider_removes_only_that_table() {
        let out = delete_provider(DOC, "openai").unwrap().unwrap();
        assert_eq!(provider_names(&out).unwrap(), ["ollama"]);
        assert!(delete_provider(DOC, "nope").unwrap().is_none());
    }

    #[test]
    fn replace_section_overwrites_scalar_table() {
        let out = replace_section(
            DOC,
            "tokenizer",
            &TokenizerConfig {
                mode: TokenizerMode::Accurate,
            },
        )
        .unwrap();
        assert!(out.contains("[tokenizer]"));
        assert!(out.contains("accurate"));
        assert!(out.contains("# fleet config"));
    }

    const DOC_WITH_TOKENIZER: &str = "# fleet config\n[server]\nport = 8080 # keep\n\n\
[tokenizer]\nmode = \"heuristic\"\n\n\
[[providers]]\nname = \"openai\"\nkind = \"openai\"\n";

    /// The other `replace_section` test only covers inserting a section that
    /// is absent; this covers the overwrite-when-present path: an existing
    /// `[tokenizer]` table with `mode = "heuristic"` must be replaced wholly
    /// (the old value gone, not merged alongside the new one), while
    /// comments and other tables outside `[tokenizer]` stay untouched.
    #[test]
    fn replace_section_overwrites_existing_section_in_place() {
        let out = replace_section(
            DOC_WITH_TOKENIZER,
            "tokenizer",
            &TokenizerConfig {
                mode: TokenizerMode::Accurate,
            },
        )
        .unwrap();
        assert!(
            !out.contains("heuristic"),
            "the old section's content must be gone, not merged: {out}"
        );
        assert!(out.contains("[tokenizer]"));
        assert!(out.contains("accurate"));
        assert!(out.contains("# fleet config"));
        assert!(out.contains("port = 8080 # keep"));
        assert_eq!(provider_names(&out).unwrap(), ["openai"]);
    }

    #[test]
    fn provider_names_empty_when_no_providers_key() {
        let doc = "[server]\nport = 8080\n";
        assert_eq!(provider_names(doc).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn parse_error_on_invalid_toml() {
        let err = provider_names("not = [valid").unwrap_err();
        assert!(matches!(err, EditError::Parse(_)));
    }
}
