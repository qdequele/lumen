//! Per-target request overrides and chat presets (ADR 014).
//!
//! Overrides act on the parsed request, restricted to a per-capability
//! allow-list checked at load, so an override can never silently do nothing.
//! Order for one attempt: defaults innermost first (so the level closest to
//! the foundation leaf wins, and a client value always wins), then `set` and
//! `drop` outermost first (so again the innermost level wins). A preset's
//! overrides count as the outermost level.

use std::sync::Arc;

use lumen_core::{
    Capability, ChatMessage, ChatRequest, EmbedRequest, MessageContent, RerankRequest,
};
use serde_json::{Map, Value};

use super::config::{OverridesConfig, PresetConfig, SystemPromptMode};

/// Most bytes a preset `system_prompt` may hold.
pub const MAX_SYSTEM_PROMPT_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Copy)]
enum Ty {
    Num,
    UInt,
    Int,
    Str,
    Bool,
    Obj,
    Stop,
}

const CHAT_FIELDS: &[(&str, Ty)] = &[
    ("temperature", Ty::Num),
    ("top_p", Ty::Num),
    ("max_tokens", Ty::UInt),
    ("max_completion_tokens", Ty::UInt),
    ("stop", Ty::Stop),
    ("seed", Ty::Int),
    ("presence_penalty", Ty::Num),
    ("frequency_penalty", Ty::Num),
    ("response_format", Ty::Obj),
    ("reasoning_effort", Ty::Str),
    ("parallel_tool_calls", Ty::Bool),
];
const EMBED_FIELDS: &[(&str, Ty)] = &[("dimensions", Ty::UInt), ("encoding_format", Ty::Str)];
const RERANK_FIELDS: &[(&str, Ty)] = &[("top_n", Ty::UInt)];

/// The overridable request fields (name and type) for `capability`; none for
/// systemone.
const fn fields(capability: Capability) -> &'static [(&'static str, Ty)] {
    match capability {
        Capability::Chat => CHAT_FIELDS,
        Capability::Embed => EMBED_FIELDS,
        Capability::Rerank => RERANK_FIELDS,
        Capability::Decisions => &[],
    }
}

/// A compiled `overrides` table.
#[derive(Debug, Clone, PartialEq)]
pub struct Overrides {
    set: Vec<(&'static str, Value)>,
    default: Vec<(&'static str, Value)>,
    drop: Vec<&'static str>,
}

impl Overrides {
    /// Compile and validate against `capability`'s allow-list.
    ///
    /// # Errors
    /// An unknown or disallowed field, a value of the wrong type, or an
    /// empty table.
    pub fn compile(cfg: &OverridesConfig, capability: Capability) -> Result<Self, String> {
        let allowed = fields(capability);
        let lookup = |name: &str| -> Result<(&'static str, Ty), String> {
            allowed
                .iter()
                .find(|(n, _)| *n == name)
                .copied()
                .ok_or_else(|| {
                    let names: Vec<&str> = allowed.iter().map(|(n, _)| *n).collect();
                    format!(
                        "override field `{name}` is not allowed on {capability} (allowed: {})",
                        if names.is_empty() {
                            "none".to_owned()
                        } else {
                            names.join(", ")
                        }
                    )
                })
        };
        let typed = |map: &Map<String, Value>| -> Result<Vec<(&'static str, Value)>, String> {
            map.iter()
                .map(|(key, value)| {
                    let (name, ty) = lookup(key)?;
                    check(ty, key, value)?;
                    Ok((name, value.clone()))
                })
                .collect()
        };
        let set = typed(&cfg.set)?;
        let default = typed(&cfg.default)?;
        let drop = cfg
            .drop
            .iter()
            .map(|key| lookup(key).map(|(name, _)| name))
            .collect::<Result<Vec<_>, _>>()?;
        if set.is_empty() && default.is_empty() && drop.is_empty() {
            return Err("`overrides` must set, default or drop at least one field".to_owned());
        }
        Ok(Self { set, default, drop })
    }
}

/// Check that `value` has the type `ty` allows, or fail naming the override `key`.
fn check(ty: Ty, key: &str, value: &Value) -> Result<(), String> {
    let ok = match ty {
        Ty::Num => value.is_number(),
        Ty::UInt => value.as_u64().is_some_and(|n| u32::try_from(n).is_ok()),
        Ty::Int => value.is_i64() || value.is_u64(),
        Ty::Str => value.is_string(),
        Ty::Bool => value.is_boolean(),
        Ty::Obj => value.is_object(),
        Ty::Stop => {
            value.is_string()
                || value
                    .as_array()
                    .is_some_and(|a| a.iter().all(Value::is_string))
        }
    };
    if ok {
        Ok(())
    } else {
        Err(format!("override `{key}` has the wrong type"))
    }
}

/// A request whose allow-listed fields can be read and rewritten by name.
pub trait Overridable {
    /// Whether the field is present.
    fn has_field(&self, name: &str) -> bool;
    /// Set the field (the value was type-checked at load).
    fn set_field(&mut self, name: &str, value: &Value);
    /// Remove the field.
    fn remove_field(&mut self, name: &str);
}

impl Overridable for ChatRequest {
    fn has_field(&self, name: &str) -> bool {
        match name {
            "temperature" => self.temperature.is_some(),
            "top_p" => self.top_p.is_some(),
            "max_tokens" => self.max_tokens.is_some(),
            "stop" => self.stop.is_some(),
            other => self.extra.contains_key(other),
        }
    }

    // Sampling parameters are small; f32 is the field's own precision.
    #[allow(clippy::cast_possible_truncation)]
    fn set_field(&mut self, name: &str, value: &Value) {
        match name {
            "temperature" => self.temperature = value.as_f64().map(|v| v as f32),
            "top_p" => self.top_p = value.as_f64().map(|v| v as f32),
            "max_tokens" => self.max_tokens = value.as_u64().and_then(|v| u32::try_from(v).ok()),
            "stop" => self.stop = Some(value.clone()),
            other => {
                self.extra.insert(other.to_owned(), value.clone());
            }
        }
    }

    fn remove_field(&mut self, name: &str) {
        match name {
            "temperature" => self.temperature = None,
            "top_p" => self.top_p = None,
            "max_tokens" => self.max_tokens = None,
            "stop" => self.stop = None,
            other => {
                self.extra.remove(other);
            }
        }
    }
}

impl Overridable for EmbedRequest {
    fn has_field(&self, name: &str) -> bool {
        match name {
            "dimensions" => self.dimensions.is_some(),
            "encoding_format" => self.encoding_format.is_some(),
            _ => false,
        }
    }

    fn set_field(&mut self, name: &str, value: &Value) {
        match name {
            "dimensions" => self.dimensions = value.as_u64().and_then(|v| u32::try_from(v).ok()),
            "encoding_format" => self.encoding_format = value.as_str().map(str::to_owned),
            _ => {}
        }
    }

    fn remove_field(&mut self, name: &str) {
        match name {
            "dimensions" => self.dimensions = None,
            "encoding_format" => self.encoding_format = None,
            _ => {}
        }
    }
}

impl Overridable for RerankRequest {
    fn has_field(&self, name: &str) -> bool {
        name == "top_n" && self.top_n.is_some()
    }

    fn set_field(&mut self, name: &str, value: &Value) {
        if name == "top_n" {
            self.top_n = value.as_u64().and_then(|v| u32::try_from(v).ok());
        }
    }

    fn remove_field(&mut self, name: &str) {
        if name == "top_n" {
            self.top_n = None;
        }
    }
}

/// Apply an attempt's override chain (outermost first) to `req`.
pub fn apply_chain<R: Overridable>(chain: &[Arc<Overrides>], req: &mut R) {
    for level in chain.iter().rev() {
        for (name, value) in &level.default {
            if !req.has_field(name) {
                req.set_field(name, value);
            }
        }
    }
    for level in chain {
        for (name, value) in &level.set {
            req.set_field(name, value);
        }
        for name in &level.drop {
            req.remove_field(name);
        }
    }
}

/// One request field standing in for the whole request, so a single field's
/// effective value can be read without cloning the request.
struct FieldProbe<'a> {
    name: &'a str,
    value: Option<Value>,
}

impl Overridable for FieldProbe<'_> {
    fn has_field(&self, name: &str) -> bool {
        name == self.name && self.value.is_some()
    }

    fn set_field(&mut self, name: &str, value: &Value) {
        if name == self.name {
            self.value = Some(value.clone());
        }
    }

    fn remove_field(&mut self, name: &str) {
        if name == self.name {
            self.value = None;
        }
    }
}

/// The value field `name` ends up with once `chain` is applied to a request
/// where it is `current` (the same `set` / `default` / `drop` rules as
/// [`apply_chain`]).
#[must_use]
pub fn effective_field(
    chain: &[Arc<Overrides>],
    name: &str,
    current: Option<Value>,
) -> Option<Value> {
    let mut probe = FieldProbe {
        name,
        value: current,
    };
    apply_chain(chain, &mut probe);
    probe.value
}

/// A compiled chat preset.
#[derive(Clone)]
pub struct Preset {
    system_prompt: Option<String>,
    mode: SystemPromptMode,
    overrides: Option<Arc<Overrides>>,
}

// Manual Debug: the prompt is operator content and must never reach a log.
impl std::fmt::Debug for Preset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Preset")
            .field(
                "system_prompt_bytes",
                &self.system_prompt.as_ref().map(String::len),
            )
            .field("mode", &self.mode)
            .field("overrides", &self.overrides.is_some())
            .finish()
    }
}

impl Preset {
    /// Compile and validate a preset.
    ///
    /// # Errors
    /// A blank or oversized prompt, invalid overrides, or an empty preset.
    pub fn compile(cfg: &PresetConfig) -> Result<Self, String> {
        if let Some(prompt) = &cfg.system_prompt {
            if prompt.trim().is_empty() {
                return Err("preset `system_prompt` must not be blank".to_owned());
            }
            if prompt.len() > MAX_SYSTEM_PROMPT_BYTES {
                return Err(format!(
                    "preset `system_prompt` is {} bytes; the limit is 32 KiB",
                    prompt.len()
                ));
            }
        }
        let overrides = cfg
            .overrides
            .as_ref()
            .map(|o| Overrides::compile(o, Capability::Chat))
            .transpose()?
            .map(Arc::new);
        if cfg.system_prompt.is_none() && overrides.is_none() {
            return Err("`preset` must set a system_prompt or overrides".to_owned());
        }
        Ok(Self {
            system_prompt: cfg.system_prompt.clone(),
            mode: cfg.system_prompt_mode,
            overrides,
        })
    }

    /// The preset's overrides (the outermost level of every attempt).
    #[must_use]
    pub fn overrides(&self) -> Option<&Arc<Overrides>> {
        self.overrides.as_ref()
    }

    /// Insert the stored system prompt according to the mode.
    pub fn apply_prompt(&self, req: &mut ChatRequest) {
        let Some(prompt) = &self.system_prompt else {
            return;
        };
        match self.mode {
            SystemPromptMode::Prepend => {}
            // `developer` is OpenAI's newer name for a client system prompt.
            SystemPromptMode::Replace => req.messages.retain(|m| !m.is_system_role()),
            SystemPromptMode::IfAbsent => {
                if req.messages.iter().any(ChatMessage::is_system_role) {
                    return;
                }
            }
        }
        req.messages.insert(
            0,
            ChatMessage {
                role: "system".to_owned(),
                content: Some(MessageContent::Text(prompt.clone())),
                name: None,
                extra: Map::new(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtual_models::config::{OverridesConfig, PresetConfig, SystemPromptMode};
    use lumen_core::ChatRequest;
    use serde_json::json;

    fn chat(json: Value) -> ChatRequest {
        serde_json::from_value(json).unwrap()
    }

    fn ov(v: Value, cap: Capability) -> Arc<Overrides> {
        let cfg: OverridesConfig = serde_json::from_value(v).unwrap();
        Arc::new(Overrides::compile(&cfg, cap).unwrap())
    }

    #[test]
    fn set_default_and_drop_on_chat() {
        let mut req = chat(json!({ "model": "m", "messages": [], "seed": 7, "temperature": 0.9 }));
        let o = ov(
            json!({ "set": { "max_tokens": 800 }, "default": { "temperature": 0.2, "top_p": 0.5 }, "drop": ["seed"] }),
            Capability::Chat,
        );
        apply_chain(&[o], &mut req);
        assert_eq!(req.max_tokens, Some(800));
        assert_eq!(
            req.temperature,
            Some(0.9),
            "default must not override a client value"
        );
        assert_eq!(req.top_p, Some(0.5));
        assert!(!req.extra.contains_key("seed"));
    }

    #[test]
    fn effective_field_follows_set_default_and_drop() {
        let outer = ov(json!({ "set": { "max_tokens": 100 } }), Capability::Chat);
        let inner = ov(
            json!({ "default": { "max_tokens": 5 }, "drop": ["seed"] }),
            Capability::Chat,
        );
        let chain = [outer, inner];
        assert_eq!(
            effective_field(&chain, "max_tokens", Some(json!(9))),
            Some(json!(100))
        );
        assert_eq!(effective_field(&chain, "seed", Some(json!(1))), None);
        let defaults = [ov(
            json!({ "default": { "max_tokens": 5 } }),
            Capability::Chat,
        )];
        assert_eq!(
            effective_field(&defaults, "max_tokens", None),
            Some(json!(5))
        );
        assert_eq!(
            effective_field(&defaults, "max_tokens", Some(json!(7))),
            Some(json!(7))
        );
    }

    #[test]
    fn the_level_closest_to_the_leaf_wins() {
        let outer = ov(
            json!({ "set": { "max_tokens": 100 }, "default": { "temperature": 0.1 } }),
            Capability::Chat,
        );
        let inner = ov(
            json!({ "set": { "max_tokens": 200 }, "default": { "temperature": 0.7 } }),
            Capability::Chat,
        );
        let mut req = chat(json!({ "model": "m", "messages": [] }));
        apply_chain(&[outer, inner], &mut req);
        assert_eq!(req.max_tokens, Some(200));
        assert_eq!(req.temperature, Some(0.7));
    }

    #[test]
    fn extra_fields_go_through_the_extra_map() {
        let mut req = chat(json!({ "model": "m", "messages": [] }));
        apply_chain(
            &[ov(
                json!({ "set": { "reasoning_effort": "low", "response_format": { "type": "json_object" } } }),
                Capability::Chat,
            )],
            &mut req,
        );
        assert_eq!(req.extra["reasoning_effort"], json!("low"));
        assert_eq!(req.extra["response_format"]["type"], json!("json_object"));
    }

    #[test]
    fn embed_and_rerank_fields() {
        let mut e: lumen_core::EmbedRequest =
            serde_json::from_value(json!({ "model": "m", "input": "x" })).unwrap();
        apply_chain(
            &[ov(
                json!({ "set": { "dimensions": 256 } }),
                Capability::Embed,
            )],
            &mut e,
        );
        assert_eq!(e.dimensions, Some(256));
        let mut r: lumen_core::RerankRequest =
            serde_json::from_value(json!({ "model": "m", "query": "q", "documents": ["a"] }))
                .unwrap();
        apply_chain(
            &[ov(json!({ "default": { "top_n": 3 } }), Capability::Rerank)],
            &mut r,
        );
        assert_eq!(r.top_n, Some(3));
    }

    #[test]
    fn compile_rejects_unknown_fields_wrong_types_and_empty_tables() {
        let bad = |v: Value, cap| {
            let cfg: OverridesConfig = serde_json::from_value(v).unwrap();
            Overrides::compile(&cfg, cap).unwrap_err()
        };
        assert!(bad(json!({ "set": { "stream": true } }), Capability::Chat).contains("not allowed"));
        assert!(
            bad(json!({ "set": { "max_tokens": "big" } }), Capability::Chat).contains("wrong type")
        );
        assert!(
            bad(json!({ "set": { "max_tokens": -1 } }), Capability::Chat).contains("wrong type")
        );
        assert!(
            bad(json!({ "set": { "dimensions": 3 } }), Capability::Chat).contains("not allowed")
        );
        assert!(
            bad(json!({ "set": { "top_n": 3 } }), Capability::Decisions).contains("not allowed")
        );
        assert!(bad(json!({}), Capability::Chat).contains("at least one"));
    }

    fn preset(prompt: &str, mode: SystemPromptMode) -> Preset {
        Preset::compile(&PresetConfig {
            system_prompt: Some(prompt.into()),
            system_prompt_mode: mode,
            overrides: None,
        })
        .unwrap()
    }

    fn roles_and_text(req: &ChatRequest) -> Vec<(String, String)> {
        req.messages
            .iter()
            .map(|m| {
                (
                    m.role.clone(),
                    m.content
                        .as_ref()
                        .map(|c| c.text().into_owned())
                        .unwrap_or_default(),
                )
            })
            .collect()
    }

    #[test]
    fn preset_modes() {
        let base = || {
            chat(json!({ "model": "m", "messages": [
            { "role": "system", "content": "client sys" }, { "role": "user", "content": "hi" } ] }))
        };

        let mut r = base();
        preset("P", SystemPromptMode::Prepend).apply_prompt(&mut r);
        assert_eq!(roles_and_text(&r)[0], ("system".into(), "P".into()));
        assert_eq!(r.messages.len(), 3);

        let mut r = base();
        preset("P", SystemPromptMode::Replace).apply_prompt(&mut r);
        assert_eq!(
            roles_and_text(&r),
            vec![("system".into(), "P".into()), ("user".into(), "hi".into())]
        );

        let mut r = base();
        preset("P", SystemPromptMode::IfAbsent).apply_prompt(&mut r);
        assert_eq!(r.messages.len(), 2, "client already sent a system message");

        let mut r =
            chat(json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] }));
        preset("P", SystemPromptMode::IfAbsent).apply_prompt(&mut r);
        assert_eq!(r.messages[0].role, "system");
    }

    /// A client `developer` message is a client system prompt (OpenAI's
    /// newer name for it): `replace` drops it and `if_absent` sees it.
    #[test]
    fn preset_modes_treat_developer_as_a_client_system_prompt() {
        let base = || {
            chat(json!({ "model": "m", "messages": [
            { "role": "developer", "content": "client dev" }, { "role": "user", "content": "hi" } ] }))
        };

        let mut r = base();
        preset("P", SystemPromptMode::Replace).apply_prompt(&mut r);
        assert_eq!(
            roles_and_text(&r),
            vec![("system".into(), "P".into()), ("user".into(), "hi".into())]
        );

        let mut r = base();
        preset("P", SystemPromptMode::IfAbsent).apply_prompt(&mut r);
        assert_eq!(
            roles_and_text(&r),
            vec![
                ("developer".into(), "client dev".into()),
                ("user".into(), "hi".into())
            ]
        );
    }

    #[test]
    fn preset_validation_and_debug_never_prints_the_prompt() {
        assert!(Preset::compile(&PresetConfig {
            system_prompt: Some("  ".into()),
            ..PresetConfig::default()
        })
        .unwrap_err()
        .contains("blank"));
        let big = "x".repeat(MAX_SYSTEM_PROMPT_BYTES + 1);
        assert!(Preset::compile(&PresetConfig {
            system_prompt: Some(big),
            ..PresetConfig::default()
        })
        .unwrap_err()
        .contains("32"));
        assert!(Preset::compile(&PresetConfig::default())
            .unwrap_err()
            .contains("system_prompt or overrides"));
        let p = preset("TOP-SECRET-PROMPT", SystemPromptMode::Prepend);
        assert!(!format!("{p:?}").contains("TOP-SECRET-PROMPT"));
    }
}
