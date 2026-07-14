# M8 — Vision (image input to chat) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Accept image inputs in `POST /v1/chat/completions` (OpenAI content-parts format) across OpenAI-family (pass-through), Anthropic, and Google/Gemini (translation).

**Architecture:** Widen `ChatMessage.content` from `Option<String>` to `Option<MessageContent>` (a string *or* an array of typed parts). Vision routes as the existing `Chat` capability — no new endpoint or routing capability. Per-model `modalities` are advertised in `GET /v1/models` and enforced (image → non-vision model = `LM-2003`, 400, before any upstream call). The gateway never fetches a user-supplied image URL: OpenAI/Anthropic accept remote URLs and fetch them; Gemini takes only inline base64, so a remote URL bound for Gemini is `LM-2004` (400).

**Tech Stack:** Rust, serde/serde_json, axum, wiremock + tokio::test.

## Global Constraints

- No `unwrap()` / `expect()` / `panic!()` outside tests and `main.rs` (justify any exception with a comment).
- No blocking the tokio runtime (no `std::thread::sleep`, no sync I/O).
- Every provider call takes a `CancellationToken`; dropping the client aborts upstream.
- Provider secrets never logged, never in returned errors, never in `Debug`.
- Clippy pedantic clean: `cargo clippy --workspace --all-targets -- -D warnings`.
- Every public item has a doc comment. Every error has a stable `LM-XXXX` code documented in `docs/errors.md`.
- Errors distinguish client (4xx) / upstream (502/503/504, named provider) / internal (500).
- Validation gate for every task: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`.
- Commits are atomic, one per task, imperative subject (`feat(core): ...`). No `Co-Authored-By` lines.

---

### Task 1: Core content types (`MessageContent` / `ContentPart` / `ImageUrl`)

**Files:**
- Modify: `crates/core/src/chat.rs` (widen `ChatMessage.content`; add the three types + helpers)
- Modify: `crates/core/src/tokens.rs:36-41` (`estimate_chat_prompt`) and its test helper `crates/core/src/tokens.rs:65-72`
- Test: inline `#[cfg(test)]` in `crates/core/src/chat.rs`

**Interfaces:**
- Produces:
  - `enum MessageContent { Text(String), Parts(Vec<ContentPart>) }` (`#[serde(untagged)]`)
  - `struct ContentPart { kind: String /* serde rename "type" */, text: Option<String>, image_url: Option<ImageUrl>, extra: Map<String, Value> }`
  - `struct ImageUrl { url: String, detail: Option<String> }`
  - `struct DataUri { media_type: String, base64_data: String }`
  - `MessageContent::text(&self) -> std::borrow::Cow<'_, str>`
  - `MessageContent::has_image(&self) -> bool`
  - `ImageUrl::as_data_uri(&self) -> Option<DataUri>`
  - `ImageUrl::is_remote(&self) -> bool`
  - `ChatMessage.content` is now `Option<MessageContent>`
- Consumes: nothing (foundation task).

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` in `crates/core/src/chat.rs` (create the module if absent; it currently has none):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_content_round_trips() {
        let json = r#"{"role":"user","content":"hello"}"#;
        let m: ChatMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(m.content, Some(MessageContent::Text(ref s)) if s == "hello"));
        // Re-serializes back to a bare string (OpenAI compatibility).
        let out = serde_json::to_value(&m).unwrap();
        assert_eq!(out["content"], "hello");
    }

    #[test]
    fn parts_with_image_round_trip_and_are_detected() {
        let json = r#"{"role":"user","content":[
            {"type":"text","text":"what is this?"},
            {"type":"image_url","image_url":{"url":"https://example.com/cat.png"}}
        ]}"#;
        let m: ChatMessage = serde_json::from_str(json).unwrap();
        let content = m.content.as_ref().unwrap();
        assert!(content.has_image());
        assert_eq!(content.text(), "what is this?");
        // image_url survives round-trip.
        let out = serde_json::to_value(&m).unwrap();
        assert_eq!(out["content"][1]["image_url"]["url"], "https://example.com/cat.png");
    }

    #[test]
    fn unknown_part_type_survives_round_trip() {
        let json = r#"{"role":"user","content":[
            {"type":"input_audio","input_audio":{"data":"AAAA","format":"wav"}}
        ]}"#;
        let m: ChatMessage = serde_json::from_str(json).unwrap();
        let content = m.content.as_ref().unwrap();
        assert!(!content.has_image());
        let out = serde_json::to_value(&m).unwrap();
        assert_eq!(out["content"][0]["type"], "input_audio");
        assert_eq!(out["content"][0]["input_audio"]["format"], "wav");
    }

    #[test]
    fn data_uri_is_parsed_and_remote_is_detected() {
        let inline = ImageUrl { url: "data:image/png;base64,iVBORw0KGgo=".to_owned(), detail: None };
        let parsed = inline.as_data_uri().unwrap();
        assert_eq!(parsed.media_type, "image/png");
        assert_eq!(parsed.base64_data, "iVBORw0KGgo=");
        assert!(!inline.is_remote());

        let remote = ImageUrl { url: "https://example.com/x.png".to_owned(), detail: None };
        assert!(remote.as_data_uri().is_none());
        assert!(remote.is_remote());
    }

    #[test]
    fn image_only_message_has_empty_text() {
        let json = r#"{"role":"user","content":[
            {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}
        ]}"#;
        let m: ChatMessage = serde_json::from_str(json).unwrap();
        assert_eq!(m.content.as_ref().unwrap().text(), "");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p core --lib chat::tests`
Expected: FAIL — `MessageContent` / `ImageUrl` / `as_data_uri` do not exist (compile error).

- [ ] **Step 3: Implement the types and helpers**

In `crates/core/src/chat.rs`, add `use std::borrow::Cow;` to the imports and replace the `ChatMessage.content` field type. Change line 18 from:

```rust
    pub content: Option<String>,
```
to:
```rust
    pub content: Option<MessageContent>,
```

Then add, below `ChatMessage` (before `ChatRequest`):

```rust
/// Message content: OpenAI overloads this as either a bare string or an array
/// of typed parts (text and images). `untagged` so a JSON string deserializes
/// to `Text` and a JSON array to `Parts`; order matters (string tried first).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// `"content": "hello"`.
    Text(String),
    /// `"content": [ {"type":"text",...}, {"type":"image_url",...} ]`.
    Parts(Vec<ContentPart>),
}

/// One element of a `Parts` array. A typed struct with a `flatten`ed `extra`
/// map (the codebase idiom) rather than an internally-tagged enum: serde
/// forbids an untagged catch-all variant inside `tag = "type"`, and unknown /
/// future part types (e.g. `input_audio`) must survive pass-through verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentPart {
    /// The part discriminator: `"text"`, `"image_url"`, or a future type.
    #[serde(rename = "type")]
    pub kind: String,
    /// Present when `kind == "text"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Present when `kind == "image_url"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<ImageUrl>,
    /// Any other fields (and the payload of unknown part types), preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// An `image_url` part's value: a URL (remote `http(s)` or a `data:` URI) plus
/// an optional `detail` hint. The gateway never dereferences `url`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageUrl {
    /// A remote `http(s)://` URL or a `data:<media_type>;base64,<payload>` URI.
    pub url: String,
    /// Optional resolution hint (`"low"`/`"high"`/`"auto"`), forwarded untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A decoded `data:` URI: its media type and its base64 payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataUri {
    /// e.g. `image/png`.
    pub media_type: String,
    /// The base64 payload (still encoded — never decoded on the hot path).
    pub base64_data: String,
}

impl MessageContent {
    /// The concatenated text of the content (borrowed for `Text`; the joined
    /// `text` parts for `Parts`). Empty for an image-only message. Used by the
    /// token estimator and any text-only inspection path.
    #[must_use]
    pub fn text(&self) -> Cow<'_, str> {
        match self {
            MessageContent::Text(s) => Cow::Borrowed(s),
            MessageContent::Parts(parts) => {
                let mut out = String::new();
                for p in parts {
                    if p.kind == "text" {
                        if let Some(t) = &p.text {
                            out.push_str(t);
                        }
                    }
                }
                Cow::Owned(out)
            }
        }
    }

    /// Whether any part carries an image.
    #[must_use]
    pub fn has_image(&self) -> bool {
        matches!(self, MessageContent::Parts(parts) if parts.iter().any(|p| p.image_url.is_some()))
    }
}

impl ImageUrl {
    /// Parse a `data:<media_type>;base64,<payload>` URI, or `None` for any other
    /// (e.g. remote) URL. Only base64 data URIs are recognised.
    #[must_use]
    pub fn as_data_uri(&self) -> Option<DataUri> {
        let rest = self.url.strip_prefix("data:")?;
        let (media_type, payload) = rest.split_once(";base64,")?;
        if media_type.is_empty() || payload.is_empty() {
            return None;
        }
        Some(DataUri {
            media_type: media_type.to_owned(),
            base64_data: payload.to_owned(),
        })
    }

    /// Whether this is a remote `http(s)` URL (which the gateway forwards but
    /// never fetches).
    #[must_use]
    pub fn is_remote(&self) -> bool {
        self.url.starts_with("http://") || self.url.starts_with("https://")
    }
}
```

Now fix the token estimator. In `crates/core/src/tokens.rs`, change line 39 from:

```rust
        .map(|m| PER_MESSAGE_OVERHEAD + m.content.as_deref().map_or(0, estimate_text))
```
to:
```rust
        .map(|m| {
            PER_MESSAGE_OVERHEAD
                + m.content
                    .as_ref()
                    .map_or(0, |c| estimate_text(&c.text()))
        })
```

And fix the test helper at `crates/core/src/tokens.rs:66-71`, changing:

```rust
            content: Some(content.to_owned()),
```
to:
```rust
            content: Some(crate::chat::MessageContent::Text(content.to_owned())),
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p core --lib`
Expected: PASS — chat + tokens tests green.

- [ ] **Step 5: Fix any other in-crate `content` consumers, then gate**

`content` is now `Option<MessageContent>`, so any remaining `Option<String>` usage in the `core` crate must compile. Run the gate:

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: providers/server may fail to compile (Tasks 3–5 fix them). If `core` itself fails elsewhere, fix that usage now.
Then: `cargo clippy -p core --all-targets -- -D warnings && cargo fmt`
Expected: clean for `core`.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/chat.rs crates/core/src/tokens.rs
git commit -m "feat(core): image content parts in chat messages (MessageContent)"
```

---

### Task 2: `modalities` config, `/v1/models` exposure, enforcement (`LM-2003`)

**Files:**
- Modify: `crates/core/src/error.rs` (add `ImageInputNotSupported` → `LM-2003`; add `ImageUrlNotSupported` → `LM-2004` now so both codes land together)
- Modify: `crates/providers/src/registry.rs` (`ModelSpec.modalities`, `LoadedModelSummary.modalities`, `model_modalities` map, `Registry::modalities`)
- Modify: `crates/server/src/config.rs` (`ModelConfig.modalities` + `default_modalities`; thread into `ModelSpec` at `config.rs:698-701`)
- Modify: `crates/server/src/models.rs` (`ModelEntry.modalities`)
- Modify: `crates/core/src/provider.rs` (add `ChatProvider::accepts_remote_image_url` default method)
- Modify: `crates/server/src/chat.rs` (enforcement pre-flight)
- Test: `crates/core/src/error.rs` (code test), `crates/server/tests/chat.rs` (enforcement 400)

**Interfaces:**
- Consumes: `MessageContent::has_image`, `ImageUrl::is_remote` (Task 1).
- Produces:
  - `GatewayError::ImageInputNotSupported { model: String }` → `code()` `"LM-2003"`, `http_status()` 400, `error_type()` `InvalidRequest`.
  - `GatewayError::ImageUrlNotSupported { provider: String }` → `"LM-2004"`, 400, `InvalidRequest`.
  - `Registry::modalities(&self, model_id: &str) -> Option<Vec<String>>`
  - `ChatProvider::accepts_remote_image_url(&self) -> bool` (default `true`)
  - `ModelConfig.modalities: Vec<String>` (default `["text"]`)

- [ ] **Step 1: Write the failing tests**

Add to `crates/core/src/error.rs` `error_codes_are_stable` test (after the `LM-2010` assert near line 496):

```rust
        assert_eq!(
            GatewayError::ImageInputNotSupported { model: "gpt".into() }.code(),
            "LM-2003"
        );
        assert_eq!(
            GatewayError::ImageUrlNotSupported { provider: "google".into() }.code(),
            "LM-2004"
        );
```

Add an enforcement integration test to `crates/server/tests/chat.rs` (follow the existing helpers in that file — `config_from`, `spawn`, `post_chat`; mirror the nearest existing test's setup). The model is declared with default modalities (text only), so an image part is rejected pre-flight:

```rust
#[tokio::test]
async fn image_to_a_non_vision_model_is_rejected_with_lm_2003() {
    // Upstream must never be called; mount nothing that would 200.
    let upstream = wiremock::MockServer::start().await;
    let cfg = format!(
        r#"
        [[providers]]
        name = "openai"
        kind = "openai"
        base_url = "{}"
        [[providers.models]]
        id = "gpt"
        capabilities = ["chat"]
        "#,
        upstream.uri()
    );
    let base = spawn(&config_from(&cfg)).await;

    let body = serde_json::json!({
        "model": "gpt",
        "messages": [{"role":"user","content":[
            {"type":"text","text":"hi"},
            {"type":"image_url","image_url":{"url":"https://example.com/x.png"}}
        ]}]
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["error"]["code"], "LM-2003");
    // The upstream was never contacted.
    assert!(upstream.received_requests().await.unwrap().is_empty());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p core --lib error::tests::error_codes_are_stable`
Expected: FAIL — the two variants don't exist.

- [ ] **Step 3: Add the error variants**

In `crates/core/src/error.rs`, add to the routing section of `GatewayError` (after `EmptyDocuments`, ~line 175):

```rust
    /// An image content part was sent to a model whose declared `modalities`
    /// do not include `"image"`. Rejected before any upstream call.
    #[error("model '{model}' does not accept image input")]
    ImageInputNotSupported { model: String },

    /// The resolved provider only accepts inline base64 image data; a remote
    /// image URL was supplied (Gemini). The gateway never fetches the URL.
    #[error("provider '{provider}' requires inline base64 image data; remote image URLs are not supported")]
    ImageUrlNotSupported { provider: String },
```

In `code()` (after the `EmptyDocuments` arm):
```rust
            GatewayError::ImageInputNotSupported { .. } => "LM-2003",
            GatewayError::ImageUrlNotSupported { .. } => "LM-2004",
```

In `http_status()`, add both to the existing `400` arm:
```rust
            GatewayError::InvalidRequest(_)
            | GatewayError::UnsupportedCapability { .. }
            | GatewayError::EmptyDocuments
            | GatewayError::ImageInputNotSupported { .. }
            | GatewayError::ImageUrlNotSupported { .. } => 400,
```

In `error_type()`, add both to the `InvalidRequest` arm (alongside `EmptyDocuments`).

- [ ] **Step 4: Add `modalities` through config → registry → models**

`crates/server/src/config.rs`, add to `ModelConfig` (after `capabilities`, ~line 302):
```rust
    /// Modalities this model accepts as input. Defaults to `["text"]`; add
    /// `"image"` to allow image content parts (vision). Unknown modalities
    /// parse but are ignored in this release.
    #[serde(default = "default_modalities")]
    pub modalities: Vec<String>,
```
Add the default fn near `default_body_limit` (~line 363):
```rust
fn default_modalities() -> Vec<String> {
    vec!["text".to_owned()]
}
```

`crates/providers/src/registry.rs`:
- Add to `ModelSpec` (after `capabilities`, ~line 35):
  ```rust
      /// Declared input modalities (e.g. `["text","image"]`).
      pub modalities: Vec<String>,
  ```
- Add to `LoadedModelSummary` (after `capabilities`, ~line 170):
  ```rust
      /// Declared input modalities.
      pub modalities: Vec<String>,
  ```
- Add a `model_modalities: HashMap<String, Vec<String>>` field to the inner struct that holds `model_capabilities` (~line 184), populate it in the same loop that fills `model_capabilities` (~line 282): `inner_modalities.entry(model.id.clone()).or_default().extend(model.modalities.iter().cloned());` (mirror the capabilities population), and set `modalities: model.modalities.clone()` when building each `LoadedModelSummary` (~line 290).
- Add the accessor next to `capabilities()` (~line 252):
  ```rust
      /// The modalities declared for a model id, if known.
      #[must_use]
      pub fn modalities(&self, model_id: &str) -> Option<Vec<String>> {
          self.inner.load().model_modalities.get(model_id).cloned()
      }
  ```

`crates/server/src/config.rs` — thread modalities into `ModelSpec` at ~line 698:
```rust
                    .map(|m| ModelSpec {
                        id: m.id.clone(),
                        upstream_id: m.resolved_upstream_id().to_owned(),
                        capabilities: m.capabilities.clone(),
                        modalities: m.modalities.clone(),
                    })
```

`crates/server/src/models.rs` — add to `ModelEntry` (after `capabilities`):
```rust
    /// Input modalities this model accepts (`text`, `image`).
    pub modalities: Vec<String>,
```
and in the `.map(|m| ModelEntry { ... })` (~line 43):
```rust
            modalities: m.modalities.clone(),
```

- [ ] **Step 5: Add the provider capability flag + handler enforcement**

`crates/core/src/provider.rs`, add to the `ChatProvider` trait (after `chat_stream_bytes`, as a defaulted method):
```rust
    /// Whether this provider can accept a remote (`http(s)`) image URL in a
    /// content part. Providers that only accept inline base64 image bytes
    /// (Gemini) return `false`, so the gateway rejects a remote URL with
    /// `LM-2004` rather than forwarding one the upstream cannot fetch.
    fn accepts_remote_image_url(&self) -> bool {
        true
    }
```

`crates/server/src/chat.rs`, in `chat()` after the chain is resolved (after line 69, `let chain = ...?;`), add the pre-flight:
```rust
    enforce_image_support(&state, &client_model, &chain, &req)?;
```
And add the helper (private fn in the same module):
```rust
/// Reject image inputs the resolved route cannot serve, before any upstream
/// call: `LM-2003` if the model is not declared vision-capable, `LM-2004` if a
/// remote image URL is bound for a provider that only takes inline base64.
fn enforce_image_support(
    state: &AppState,
    client_model: &str,
    chain: &[lumen_router::ChatChainLink],
    req: &ChatRequest,
) -> Result<(), GatewayError> {
    let has_image = req
        .messages
        .iter()
        .any(|m| m.content.as_ref().is_some_and(lumen_core::MessageContent::has_image));
    if !has_image {
        return Ok(());
    }
    // LM-2003: model must declare the "image" modality.
    let vision_ok = state
        .registry
        .modalities(client_model)
        .is_some_and(|mods| mods.iter().any(|m| m == "image"));
    if !vision_ok {
        return Err(GatewayError::ImageInputNotSupported {
            model: client_model.to_owned(),
        });
    }
    // LM-2004: if the PRIMARY provider can't take a remote URL, reject one.
    let has_remote_url = req.messages.iter().any(|m| {
        matches!(m.content.as_ref(), Some(lumen_core::MessageContent::Parts(parts))
            if parts.iter().any(|p| p.image_url.as_ref().is_some_and(lumen_core::ImageUrl::is_remote)))
    });
    if has_remote_url && !chain[0].route.provider.accepts_remote_image_url() {
        return Err(GatewayError::ImageUrlNotSupported {
            provider: chain[0].route.provider_name.clone(),
        });
    }
    Ok(())
}
```
Add `MessageContent`, `ImageUrl` to the `lumen_core` import line at `crates/server/src/chat.rs:36` if referring to them unqualified is preferred; the snippet above uses fully-qualified paths so no import change is required.

Also fix the non-streaming output estimate at `crates/server/src/chat.rs:245-248` (it uses `content.as_deref()`):
```rust
                c.message
                    .content
                    .as_ref()
                    .map_or(0, |c| tokens::estimate_text(&c.text()))
```

- [ ] **Step 6: Update all `ModelSpec` / `LoadedModelSummary` / `ModelConfig` constructor sites in tests**

The new required struct fields break existing struct-literal constructions in tests across `registry.rs`, `config.rs`, and any provider test building a `ModelSpec`. Find them:

Run: `cargo test --workspace 2>&1 | grep -E "missing field|error\[" | head -30`
For each, add `modalities: vec!["text".to_owned()]` (or `vec![]`) to the literal. TOML-based configs need no change (the field defaults).

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p core --lib error::tests && cargo test -p server --test chat image_to_a_non_vision_model_is_rejected_with_lm_2003`
Expected: PASS.

- [ ] **Step 8: Gate + commit**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt`
```bash
git add crates/core/src/error.rs crates/core/src/provider.rs crates/providers/src/registry.rs crates/server/src/config.rs crates/server/src/models.rs crates/server/src/chat.rs
git commit -m "feat(server): per-model modalities, /v1/models exposure, vision enforcement (LM-2003/2004)"
```

---

### Task 3: Anthropic image translation

**Files:**
- Modify: `crates/providers/src/anthropic/mod.rs` (`translate_request` default-role arm + image-block helper)
- Test: `crates/providers/src/anthropic/mod.rs` (`#[cfg(test)]`) or the crate's existing anthropic test file — assert the exact translated JSON.

**Interfaces:**
- Consumes: `MessageContent`, `ContentPart`, `ImageUrl::as_data_uri`, `ImageUrl::is_remote` (Task 1).
- Produces: Anthropic request bodies whose image parts become `image` source blocks.

- [ ] **Step 1: Write the failing tests**

Add to the anthropic tests (mirror the file's existing `translate_request` tests; if none, add a `#[cfg(test)] mod tests`). The function is private, so tests live in the same module.

```rust
#[test]
fn data_uri_image_becomes_a_base64_source_block() {
    use lumen_core::{ChatMessage, ChatRequest, ContentPart, ImageUrl, MessageContent};
    let req = ChatRequest {
        model: "claude".to_owned(),
        messages: vec![ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Parts(vec![
                ContentPart { kind: "text".to_owned(), text: Some("describe".to_owned()), image_url: None, extra: Default::default() },
                ContentPart { kind: "image_url".to_owned(), text: None,
                    image_url: Some(ImageUrl { url: "data:image/png;base64,AAAA".to_owned(), detail: None }),
                    extra: Default::default() },
            ])),
            name: None,
            extra: Default::default(),
        }],
        temperature: None, top_p: None, max_tokens: None, n: None, stop: None,
        stream: false, extra: Default::default(),
    };
    let body = serde_json::to_value(translate_request(&req, false)).unwrap();
    let blocks = &body["messages"][0]["content"];
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], "describe");
    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["source"]["type"], "base64");
    assert_eq!(blocks[1]["source"]["media_type"], "image/png");
    assert_eq!(blocks[1]["source"]["data"], "AAAA");
}

#[test]
fn remote_url_image_becomes_a_url_source_block() {
    use lumen_core::{ChatMessage, ChatRequest, ContentPart, ImageUrl, MessageContent};
    let req = ChatRequest {
        model: "claude".to_owned(),
        messages: vec![ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Parts(vec![
                ContentPart { kind: "image_url".to_owned(), text: None,
                    image_url: Some(ImageUrl { url: "https://ex.com/c.png".to_owned(), detail: None }),
                    extra: Default::default() },
            ])),
            name: None, extra: Default::default(),
        }],
        temperature: None, top_p: None, max_tokens: None, n: None, stop: None,
        stream: false, extra: Default::default(),
    };
    let body = serde_json::to_value(translate_request(&req, false)).unwrap();
    let block = &body["messages"][0]["content"][0];
    assert_eq!(block["type"], "image");
    assert_eq!(block["source"]["type"], "url");
    assert_eq!(block["source"]["url"], "https://ex.com/c.png");
}

#[test]
fn text_only_message_stays_a_plain_string() {
    use lumen_core::{ChatMessage, ChatRequest, MessageContent};
    let req = ChatRequest {
        model: "claude".to_owned(),
        messages: vec![ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Text("hello".to_owned())),
            name: None, extra: Default::default(),
        }],
        temperature: None, top_p: None, max_tokens: None, n: None, stop: None,
        stream: false, extra: Default::default(),
    };
    let body = serde_json::to_value(translate_request(&req, false)).unwrap();
    assert_eq!(body["messages"][0]["content"], "hello");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p providers anthropic 2>&1 | tail -20`
Expected: FAIL — compile error (`translate_request` still assumes `Option<String>`).

- [ ] **Step 3: Implement the translation**

In `crates/providers/src/anthropic/mod.rs`:

Replace line 183 (`let text = m.content.clone().unwrap_or_default();`) with a text extractor that works on the new type:
```rust
        let text = m
            .content
            .as_ref()
            .map(|c| c.text().into_owned())
            .unwrap_or_default();
```

Replace the default `role => messages.push(...)` arm (lines 233-236) with an image-aware content builder:
```rust
            role => messages.push(AnthropicMessage {
                role: role.to_owned(),
                content: anthropic_content(m.content.as_ref(), &text),
            }),
```

Add these helpers below `translate_request` (import `lumen_core::{ImageUrl, MessageContent}` at the top and `serde_json::json` is already used):
```rust
/// Build an Anthropic message `content`: a plain string when there are no
/// images, else an array of `text`/`image` blocks (order preserved).
fn anthropic_content(content: Option<&MessageContent>, text: &str) -> serde_json::Value {
    match content {
        Some(MessageContent::Parts(parts)) if parts.iter().any(|p| p.image_url.is_some()) => {
            let mut blocks = Vec::with_capacity(parts.len());
            for p in parts {
                if let Some(img) = &p.image_url {
                    blocks.push(anthropic_image_block(img));
                } else if p.kind == "text" {
                    if let Some(t) = &p.text {
                        blocks.push(json!({ "type": "text", "text": t }));
                    }
                }
            }
            serde_json::Value::Array(blocks)
        }
        // No images (string, text-only parts, or none): a plain string.
        _ => serde_json::Value::String(text.to_owned()),
    }
}

/// Translate one OpenAI `image_url` into an Anthropic image source block.
/// `data:` URIs become a `base64` source; remote URLs a `url` source (Anthropic
/// fetches it). The gateway never fetches the URL itself.
fn anthropic_image_block(image: &ImageUrl) -> serde_json::Value {
    if let Some(data) = image.as_data_uri() {
        json!({
            "type": "image",
            "source": { "type": "base64", "media_type": data.media_type, "data": data.base64_data },
        })
    } else {
        json!({
            "type": "image",
            "source": { "type": "url", "url": image.url },
        })
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p providers anthropic`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/anthropic/mod.rs
git commit -m "feat(providers): Anthropic image content translation (base64 + url source blocks)"
```

---

### Task 4: Google/Gemini image translation (`inline_data`; remote URL → `LM-2004`)

**Files:**
- Modify: `crates/providers/src/google/mod.rs` (`GeminiPart` gains inline-data support; `translate_request` becomes fallible; the provider overrides `accepts_remote_image_url`)
- Test: `crates/providers/src/google/mod.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `MessageContent`, `ImageUrl::as_data_uri`/`is_remote` (Task 1); `ProviderError::Translation` (existing); `ChatProvider::accepts_remote_image_url` (Task 2).
- Produces: Gemini request bodies whose data-URI images become `inline_data` parts; a remote URL yields `ProviderError::Translation` on the rare fallback path (the handler's `LM-2004` pre-flight covers the primary-Gemini path).

- [ ] **Step 1: Write the failing tests**

Add to google tests (private `translate_request` → same-module tests):

```rust
#[test]
fn data_uri_image_becomes_inline_data() {
    use lumen_core::{ChatMessage, ChatRequest, ContentPart, ImageUrl, MessageContent};
    let req = ChatRequest {
        model: "gemini".to_owned(),
        messages: vec![ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Parts(vec![
                ContentPart { kind: "text".to_owned(), text: Some("what?".to_owned()), image_url: None, extra: Default::default() },
                ContentPart { kind: "image_url".to_owned(), text: None,
                    image_url: Some(ImageUrl { url: "data:image/jpeg;base64, /9j/".to_owned().replace(' ', ""), detail: None }),
                    extra: Default::default() },
            ])),
            name: None, extra: Default::default(),
        }],
        temperature: None, top_p: None, max_tokens: None, n: None, stop: None,
        stream: false, extra: Default::default(),
    };
    let body = serde_json::to_value(translate_request(&req).unwrap()).unwrap();
    let parts = &body["contents"][0]["parts"];
    assert_eq!(parts[0]["text"], "what?");
    assert_eq!(parts[1]["inline_data"]["mime_type"], "image/jpeg");
    assert_eq!(parts[1]["inline_data"]["data"], "/9j/");
}

#[test]
fn remote_url_image_is_a_translation_error() {
    use lumen_core::{ChatMessage, ChatRequest, ContentPart, ImageUrl, MessageContent};
    let req = ChatRequest {
        model: "gemini".to_owned(),
        messages: vec![ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Parts(vec![
                ContentPart { kind: "image_url".to_owned(), text: None,
                    image_url: Some(ImageUrl { url: "https://ex.com/c.png".to_owned(), detail: None }),
                    extra: Default::default() },
            ])),
            name: None, extra: Default::default(),
        }],
        temperature: None, top_p: None, max_tokens: None, n: None, stop: None,
        stream: false, extra: Default::default(),
    };
    assert!(matches!(translate_request(&req), Err(lumen_core::ProviderError::Translation(_))));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p providers google 2>&1 | tail -20`
Expected: FAIL — compile error (`GeminiPart` is text-only; `translate_request` is infallible).

- [ ] **Step 3: Implement the translation**

In `crates/providers/src/google/mod.rs`:

Change `GeminiPart` (line ~102) to carry either text or inline data:
```rust
#[derive(Serialize)]
struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(rename = "inline_data", skip_serializing_if = "Option::is_none")]
    inline_data: Option<GeminiInlineData>,
}

#[derive(Serialize)]
struct GeminiInlineData {
    mime_type: String,
    data: String,
}

impl GeminiPart {
    fn text(s: String) -> Self {
        Self { text: Some(s), inline_data: None }
    }
    fn image(mime_type: String, data: String) -> Self {
        Self { text: None, inline_data: Some(GeminiInlineData { mime_type, data }) }
    }
}
```

Update the existing text-part construction sites (the system arm at ~line 178 and the default arm at ~line 188) from `GeminiPart { text }` to `GeminiPart::text(text)`.

Change `translate_request` signature (line 170) to return a `Result`:
```rust
fn translate_request(req: &ChatRequest) -> Result<GeminiRequest, lumen_core::ProviderError> {
```
Replace line 174 (`let text = m.content.clone().unwrap_or_default();`) with:
```rust
        let text = m
            .content
            .as_ref()
            .map(|c| c.text().into_owned())
            .unwrap_or_default();
```
Replace the default `role => contents.push(...)` arm (lines 182-189) with parts building:
```rust
            role => contents.push(GeminiContent {
                role: if role == "assistant" { "model".to_owned() } else { role.to_owned() },
                parts: gemini_parts(m.content.as_ref(), &text)?,
            }),
```
Make the `for m in &req.messages` loop able to `?`, and end the function with `Ok(GeminiRequest { ... })` instead of the bare struct.

Add the parts helper (import `lumen_core::{MessageContent, ProviderError}`):
```rust
/// Build Gemini `parts` from a message: data-URI images become `inline_data`;
/// a remote image URL is a translation error (Gemini takes only inline bytes,
/// and the gateway never fetches the URL). Text-only content is one text part.
fn gemini_parts(
    content: Option<&MessageContent>,
    text: &str,
) -> Result<Vec<GeminiPart>, ProviderError> {
    match content {
        Some(MessageContent::Parts(parts)) if parts.iter().any(|p| p.image_url.is_some()) => {
            let mut out = Vec::with_capacity(parts.len());
            for p in parts {
                if let Some(img) = &p.image_url {
                    let data = img.as_data_uri().ok_or_else(|| {
                        ProviderError::Translation(
                            "Gemini requires inline base64 image data; remote image URLs are not supported".to_owned(),
                        )
                    })?;
                    out.push(GeminiPart::image(data.media_type, data.base64_data));
                } else if p.kind == "text" {
                    if let Some(t) = &p.text {
                        out.push(GeminiPart::text(t.clone()));
                    }
                }
            }
            Ok(out)
        }
        _ => Ok(vec![GeminiPart::text(text.to_owned())]),
    }
}
```

Update `translate_request`'s callers in the same file (the `chat` and `chat_stream`/`chat_stream_bytes` impls) to propagate the `Result`: change `let body = translate_request(&req);` to `let body = translate_request(&req)?;` (they already return `Result<_, ProviderError>`).

Override the trait flag on the Google provider `impl ChatProvider for GoogleProvider` block:
```rust
    fn accepts_remote_image_url(&self) -> bool {
        false
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p providers google`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/google/mod.rs
git commit -m "feat(providers): Gemini inline_data image translation; reject remote URLs"
```

---

### Task 5: Body-size limit → `LM-1002` envelope

**Files:**
- Modify: `crates/server/src/state.rs` (`AppState.body_limit` + `with_body_limit` builder)
- Modify: wherever `AppState` is assembled at boot (`crates/server/src/lifecycle.rs` or `main.rs` — grep for `build_app(` and the `AppState::new` call) to set `body_limit`
- Modify: `crates/server/src/chat.rs` (map a 413 `JsonRejection` to `PayloadTooLarge`)
- Test: `crates/server/tests/chat.rs`

**Interfaces:**
- Consumes: `GatewayError::PayloadTooLarge { limit }` (existing), `ServerConfig.body_limit` (existing).
- Produces: `AppState.body_limit: usize`.

- [ ] **Step 1: Write the failing test**

Add to `crates/server/tests/chat.rs`. Set a tiny body limit in config and send an oversized body:

```rust
#[tokio::test]
async fn oversized_body_returns_lm_1002_envelope() {
    let upstream = wiremock::MockServer::start().await;
    let cfg = format!(
        r#"
        [server]
        body_limit = 256

        [[providers]]
        name = "openai"
        kind = "openai"
        base_url = "{}"
        [[providers.models]]
        id = "gpt"
        capabilities = ["chat"]
        "#,
        upstream.uri()
    );
    let base = spawn(&config_from(&cfg)).await;

    let big = "x".repeat(4096);
    let body = serde_json::json!({ "model": "gpt", "messages": [{"role":"user","content": big}] });
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["error"]["code"], "LM-1002");
}
```

(If `config_from`/`spawn` don't thread `[server] body_limit` yet, ensure the test harness builds the app with `cfg.server.body_limit` — see Step 3.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p server --test chat oversized_body_returns_lm_1002_envelope`
Expected: FAIL — currently an oversized body maps to `LM-1001` (400), not `LM-1002` (413).

- [ ] **Step 3: Implement**

`crates/server/src/state.rs` — add the field (near the other pub fields, ~line 64) and a builder (near the other `with_*`):
```rust
    /// Configured max request body size in bytes (for the `LM-1002` message).
    pub body_limit: usize,
```
Initialize it in `AppState::new` (default to the config default) and add:
```rust
    /// Set the request body-size limit surfaced in `LM-1002`.
    #[must_use]
    pub fn with_body_limit(mut self, body_limit: usize) -> Self {
        self.body_limit = body_limit;
        self
    }
```
Set `body_limit: default_body_limit()` (import from config) or `10 * 1024 * 1024` in `new`, so existing construction sites need no change.

At the boot assembly site (grep `build_app(`): set `let state = state.with_body_limit(config.server.body_limit);` before `build_app(state, config.server.body_limit)`. Make the test harness do the same.

`crates/server/src/chat.rs` — replace the body map_err at lines 59-60:
```rust
    // Malformed body → LM-1001; over-limit body → LM-1002 (both in our envelope).
    let Json(req) = payload.map_err(|e| {
        if e.status() == axum::http::StatusCode::PAYLOAD_TOO_LARGE {
            GatewayError::PayloadTooLarge { limit: state.body_limit }
        } else {
            GatewayError::InvalidRequest(e.body_text())
        }
    })?;
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p server --test chat oversized_body_returns_lm_1002_envelope`
Expected: PASS.

If the 413 does not surface as a `JsonRejection` (some tower versions short-circuit at the layer), instead add a small response-mapping middleware that rewrites a bare `413` into the `LM-1002` envelope; keep the same assertion. Verify which path fires with `cargo test ... -- --nocapture` before choosing.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/state.rs crates/server/src/chat.rs crates/server/src/lifecycle.rs
git commit -m "feat(server): map over-limit request bodies to the LM-1002 envelope"
```

---

### Task 6: OpenAI-family passthrough test, accounting note, conformance, docs, gate

**Files:**
- Test: `crates/server/tests/chat.rs` (OpenAI-family passthrough + no-usage estimate)
- Modify: `docs/errors.md`, `docs/providers.md`, `README.md`, `config.example.toml`, `docs/adr/003-*.md` (addendum), `CHANGELOG.md`, `ROADMAP.md`, `docs/backlog.md`

**Interfaces:** Consumes everything from Tasks 1–5.

- [ ] **Step 1: Write the passthrough + accounting tests**

Add to `crates/server/tests/chat.rs`. A model declared with `modalities = ["text","image"]`; assert the mock upstream receives the image part verbatim, and that a response with no usage still yields `estimated: true`.

```rust
#[tokio::test]
async fn openai_family_forwards_image_parts_verbatim() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id":"c","object":"chat.completion","created":0,"model":"gpt",
            "choices":[{"index":0,"message":{"role":"assistant","content":"a cat"},"finish_reason":"stop"}]
        })))
        .mount(&upstream)
        .await;
    let cfg = format!(
        r#"
        [[providers]]
        name = "openai"
        kind = "openai"
        base_url = "{}"
        [[providers.models]]
        id = "gpt"
        capabilities = ["chat"]
        modalities = ["text", "image"]
        "#,
        upstream.uri()
    );
    let base = spawn(&config_from(&cfg)).await;
    let body = serde_json::json!({
        "model":"gpt",
        "messages":[{"role":"user","content":[
            {"type":"text","text":"what is this?"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}
        ]}]
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&body).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    // The upstream received the image part unchanged.
    let reqs = upstream.received_requests().await.unwrap();
    let sent: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(sent["messages"][0]["content"][1]["image_url"]["url"], "data:image/png;base64,AAAA");
    // No upstream usage → estimated flag set, never a silent zero.
    let got: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(got["usage"]["estimated"], true);
    assert!(got["usage"]["prompt_tokens"].as_u64().unwrap() > 0);
}
```

- [ ] **Step 2: Run to verify it passes** (the behavior already exists after Tasks 1–2)

Run: `cargo test -p server --test chat openai_family_forwards_image_parts_verbatim`
Expected: PASS. If the image part is altered, fix the OpenAI provider serialization (it should send `ChatRequest` verbatim).

- [ ] **Step 3: Update docs**

- `docs/errors.md`: add `LM-2003` (400) and `LM-2004` (400) rows to the `LM-2xxx` table with the meanings from the spec.
- `docs/providers.md`: add a "Vision (image input)" section — the OpenAI content-parts request shape, which kinds support it (OpenAI-family pass-through, Anthropic, Gemini), the `modalities = ["text","image"]` per-model config, and the never-fetch / Gemini-needs-base64 (`LM-2004`) rule.
- `README.md`: note vision support in the capability/matrix section.
- `config.example.toml`: add a commented `modalities = ["text", "image"]` line to a chat model example.
- `docs/adr/003-*.md`: append an addendum — image tokens are trusted from upstream usage; the local estimation fallback counts text only (images contribute 0, response still flagged `estimated`); a per-image heuristic is deferred.
- `docs/backlog.md`: add "per-image token heuristic for the estimation fallback (OpenAI tile formula)" and "Anthropic/Gemini file/GCS image URIs".
- `CHANGELOG.md`: add an `Added — Vision (image input to chat)` entry under `[Unreleased]`.
- `ROADMAP.md`: check off M8 / record the vision slice.

- [ ] **Step 4: Full gate**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`
Expected: all green.

- [ ] **Step 5: Code review + commit**

Dispatch the `code-reviewer` agent on the diff (correctness, safety rules, error taxonomy, secrets). Address findings, then:
```bash
git add -A
git commit -m "feat: vision image input to chat (docs, conformance, accounting note)"
```

---

## Self-Review

**Spec coverage:**
- §3 core type change → Task 1. ✓
- §4 config/modalities/`/v1/models`/enforcement `LM-2003` → Task 2. ✓
- §5 provider translation (OpenAI passthrough Task 6; Anthropic Task 3; Gemini + `LM-2004` Tasks 2/4) + never-fetch → `accepts_remote_image_url` (Task 2) + Gemini `Translation` guard (Task 4). ✓
- §6 error taxonomy `LM-2003`/`LM-2004` → Task 2; `LM-1001`/`LM-1002` reuse → Tasks 1/5. ✓
- §7 body limit → Task 5. ✓
- §8 accounting (trust upstream; text-only fallback) → Tasks 1 (`text()`) + 6 (ADR addendum + test). ✓
- §9 testing → each task's tests + Task 6 conformance. ✓

**Placeholder scan:** No TBD/TODO; every code step shows real code; every command has expected output. ✓

**Type consistency:** `MessageContent`, `ContentPart` (`kind`/`text`/`image_url`/`extra`), `ImageUrl` (`url`/`detail`), `DataUri` (`media_type`/`base64_data`), `MessageContent::text()/has_image()`, `ImageUrl::as_data_uri()/is_remote()`, `Registry::modalities()`, `ChatProvider::accepts_remote_image_url()`, `GatewayError::ImageInputNotSupported/ImageUrlNotSupported`, `AppState::with_body_limit` — names used consistently across tasks. ✓

**Known cross-task break (called out in-task):** adding required fields to `ModelSpec`/`LoadedModelSummary` breaks existing struct literals in tests — Task 2 Step 6 sweeps them. `translate_request` becoming fallible (Gemini) updates its in-file callers — Task 4 Step 3.
