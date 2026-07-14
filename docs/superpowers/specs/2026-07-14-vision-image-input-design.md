# M8 — Vision (image input to chat)

**Status:** approved design, pre-implementation
**Date:** 2026-07-14
**Author:** LUMEN maintainers
**Supersedes non-goal:** `CLAUDE.md` → "support des images/audio" (this lifts the
first, narrowest slice of it: image *input* to chat).

## 1. Summary

Accept image inputs in `POST /v1/chat/completions` using the OpenAI
content-parts format, across three provider families:

- **OpenAI-family (pass-through):** `openai` + the 11 OpenAI-compatible kinds +
  `vllm`. Image parts are OpenAI-shaped already and forwarded verbatim.
- **Anthropic (translation):** `image_url` parts → Anthropic `image` source
  blocks.
- **Google/Gemini (translation):** `image_url` parts → Gemini `inline_data`.

Vision is a **sub-capability of chat**: same endpoint, same `Chat` routing
capability, same streaming path. No new top-level `Capability` variant.

## 2. Scope

**In scope**
- Widen `ChatMessage.content` to accept a string *or* an array of content parts.
- Per-model `modalities` declaration, surfaced in `GET /v1/models`.
- Enforcement: image parts to a non-vision model fail fast with `LM-2003` (400).
- Provider translation for Anthropic and Gemini (OpenAI-family is pass-through).
- Configurable request body-size limit with a vision-friendly default, mapped to
  the `LM-1002` JSON error envelope.
- Token accounting: trust upstream usage; text-only estimation fallback.
- Tests (wiremock) per provider + enforcement + accounting + body-limit +
  cancellation.

**Out of scope (later slices / backlog)**
- Image *generation*, audio (STT/TTS), image/audio *output*.
- Server-side image fetching, resizing, re-encoding, or validation of image
  bytes. The gateway never dereferences a user-supplied image URL.
- A per-image token *heuristic* for the estimation fallback (backlog item).
- Anthropic/Gemini "file"/GCS URI image sources (only inline base64 + —for
  providers that accept them— passthrough of remote URLs).

> **Update (M9 landed first):** the shared `ContentPart` / `ImageUrl` types now
> live in `crates/core/src/content.rs` (introduced by the multimodal-embeddings
> milestone) and should be **reused** here rather than redefined in `chat.rs`.
> `ContentPart.kind` (`"type"`) already defaults to `"text"`, and image-vs-text
> is dispatched by field presence, not `kind` — see that module. The M8
> `MessageContent` enum and `ChatMessage.content` widening below are unchanged;
> only the part types are shared. M8's "never fetch" stance for chat is
> unaffected by M9's opt-in embeddings fetch.

## 3. Core type change

`crates/core/src/chat.rs`. Widen `ChatMessage.content`:

```rust
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,   // was Option<String>
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// OpenAI overloads `content`: a bare string, or an array of typed parts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// `"content": "hello"` — tried first (serde untagged order matters).
    Text(String),
    /// `"content": [ {"type":"text",...}, {"type":"image_url",...} ]`
    Parts(Vec<ContentPart>),
}

/// One element of a `Parts` content array. Modelled as a typed struct with a
/// `flatten`ed `extra` map (the same idiom as `ChatMessage`/`ChatRequest`)
/// rather than an internally-tagged enum: serde does not permit an
/// `#[serde(untagged)]` catch-all variant inside a `#[serde(tag = "type")]`
/// enum, and we need unknown/future part types (e.g. `input_audio`) to survive
/// pass-through verbatim rather than 400.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub kind: String,                     // "text" | "image_url" | future
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,             // present when kind == "text"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<ImageUrl>,      // present when kind == "image_url"
    /// Any other fields (and the payload of unknown part types) preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}
```

Helpers on `MessageContent`:
- `fn text(&self) -> Cow<'_, str>` — concatenated `text` of all parts whose
  `kind == "text"` (borrowed directly for the `Text` variant; empty for an
  image-only message). Used by the token estimator and any text-only
  log/inspection path.
- `fn has_image(&self) -> bool` — true if any part has `image_url.is_some()`
  (equivalently `kind == "image_url"`). Used by the enforcement check.

**Rationale (approaches considered).** (A, chosen) `MessageContent` as a typed
untagged enum (`Text` string vs `Parts` array), with each part a typed struct +
`flatten`ed `extra`. Type-safe access to `text`/`image_url`, forward-compatible
via `extra`/`kind`, and identical to the codebase's existing struct idiom.
(B) parts as raw `Vec<serde_json::Value>` — robust but pushes ad-hoc JSON poking
into every consumer. (C) a separate `content_parts` field — not
OpenAI-compatible, so it would not parse real vision requests. The originally
sketched internally-tagged enum with an `#[serde(untagged)]` `Other` variant was
rejected: serde does not support that combination.

**Blast radius.** Every site that does `m.content` as `Option<String>` updates:
- `crates/providers/src/anthropic/mod.rs::translate_request`
  (`m.content.clone().unwrap_or_default()` → parts-aware translation).
- `crates/providers/src/google/mod.rs` request translation (same).
- Token estimator (ADR 003 path): estimate `content.text()` only.
- OpenAI-family providers: `content` re-serializes to the identical JSON, so
  pass-through is unchanged, but the field type change must compile through them.

## 4. Config & routing

Per-model optional `modalities`, default `["text"]`:

```toml
[[providers.models]]
id = "gpt-4o"
capabilities = ["chat"]
modalities = ["text", "image"]   # NEW; default ["text"]
```

- `GET /v1/models` adds `"modalities": ["text","image"]` per model entry.
- Routing is unchanged: vision requests route as `Capability::Chat`.
- **Enforcement (fail fast):** in the chat handler, before the upstream call,
  if any message `has_image()` and the resolved model's modalities lack
  `"image"` → `LM-2003` (400). This saves a doomed upstream round-trip and gives
  a clear client error instead of a provider-shaped 4xx.

`modalities` is a plain `Vec<String>` in config (not an enum) so unknown future
modalities (`"audio"`, …) parse without a schema bump; only `"image"` is acted
on in this slice.

## 5. Provider translation (never fetch)

Two image URL forms per the OpenAI schema: a `data:` URI (base64 inline) or a
remote `http(s)` URL.

| Provider family | `data:` URI | `http(s)` URL |
|---|---|---|
| OpenAI-family / vLLM | forward verbatim | forward verbatim |
| Anthropic | `{type:"image",source:{type:"base64",media_type,data}}` | `{type:"image",source:{type:"url",url}}` (Anthropic fetches) |
| Gemini | `{inline_data:{mime_type,data}}` | **`LM-2004` (400)** — Gemini takes only inline bytes; gateway must not fetch |

**Data-URI parsing.** `data:<media_type>;base64,<payload>` → `(media_type,
payload)`. A malformed data URI (no `;base64,`, empty payload) → `LM-1001`
(400). Non-base64 image encodings in a data URI (rare) → `LM-1001`.

**Never fetch (pillar-mandated).** The gateway never dereferences a
user-supplied image URL: SSRF-safety (an attacker could aim it at internal
addresses) and the <1 ms / streaming perf pillar both forbid it. Providers that
accept remote URLs (OpenAI, Anthropic) fetch them themselves; Gemini can't, so
a remote URL bound for Gemini is a clean `LM-2004`, not a silent fetch.

Text parts translate as before (concatenated / preserved per each provider's
existing message shaping). Ordering of parts within a message is preserved.

## 6. Error taxonomy (`docs/errors.md`)

Two new `LM-2xxx` codes (`type: invalid_request`), siblings of `LM-2002`
("model exists but doesn't serve the requested capability"):

| Code | HTTP | Meaning |
|---|---|---|
| `LM-2003` | 400 | Image input sent to a model not declared vision-capable (`modalities` lacks `"image"`). |
| `LM-2004` | 400 | Resolved provider requires inline base64 image data; remote image URLs are unsupported for it (Gemini). |

Reused:
- `LM-1001` (400) — malformed content parts / malformed data URI.
- `LM-1002` (413) — request body exceeded the configured size limit (§7).

No secret or image bytes ever appear in an error message.

## 7. Request body-size limit

Base64 images are large (a 5 MiB image ≈ 6.7 MiB base64 inside JSON).

- New config `server.max_body_bytes` (default **32 MiB**), applied to the chat
  route's `RequestBodyLimitLayer`.
- The tower-http body-limit rejection is mapped to `GatewayError::PayloadTooLarge`
  → `LM-1002` JSON envelope (today it is a raw 413 without our envelope; this
  closes the M2 backlog item).

## 8. Token accounting (Pillar 5, ADR 003 addendum)

- **Primary:** upstream `usage` is trusted verbatim. OpenAI, Anthropic, and
  Gemini all fold image tokens into `prompt_tokens`, so real accounting is
  correct with no extra work.
- **Fallback (upstream reported no usage):** estimate from `content.text()`
  only. Image parts contribute **0** to the estimate; the response is still
  flagged `estimated: true`. This undercounts image-heavy requests on the rare
  no-usage upstream (e.g. some vLLM builds).
- Documented as a known undercount here + a backlog item: "per-image token
  heuristic for the estimation fallback (OpenAI tile formula)." No new ADR; an
  addendum note is added to ADR 003.

## 9. Testing (wiremock, per the Definition of Done)

- **Core serde:** string content round-trips (regression); parts round-trip;
  an unknown part type (e.g. `{"type":"input_audio",...}`) survives round-trip
  verbatim via `kind` + `extra`; `text()` / `has_image()` unit tests; untagged
  order (a JSON string never parses as `Parts`).
- **OpenAI-family:** a vision request's parts reach the mock upstream byte-for-
  byte (pass-through).
- **Anthropic:** data-URI → base64 source block; remote URL → url source block;
  exact translated JSON asserted.
- **Gemini:** data-URI → `inline_data`; remote URL → `LM-2004` (400) and *no
  upstream request is made*.
- **Enforcement:** image part to a `["text"]` model → `LM-2003` (400), no
  upstream call.
- **Body limit:** an over-limit body → `LM-1002` (413) with JSON envelope.
- **Accounting:** vision request with no upstream usage → `estimated: true`,
  text tokens counted, no panic.
- **Cancellation:** client disconnect on an in-flight vision request still
  aborts the upstream call (the M4 guarantee holds for parts payloads).

## 10. Milestone breakdown (tests-first, one atomic commit each)

1. **Core types** — `MessageContent` / `ContentPart` / `ImageUrl` + `text()` /
   `has_image()`; serde round-trip + regression tests. Update the token
   estimator to `content.text()`.
2. **Config + routing** — `modalities` config field, `/v1/models` exposure,
   enforcement (`LM-2003`) + tests.
3. **Anthropic translation** — image blocks (base64 + url) + tests.
4. **Gemini translation** — `inline_data` + remote-URL `LM-2004` + tests.
5. **Body limit** — `server.max_body_bytes` + `LM-1002` envelope mapping + test.
6. **Finish** — accounting addendum, conformance suite, gate
   (`cargo test --workspace && cargo clippy --workspace --all-targets -- -D
   warnings && cargo fmt --check`), code-reviewer, docs-writer (README matrix,
   `docs/providers.md`, `docs/errors.md`, `config.example.toml`), CHANGELOG,
   ROADMAP/backlog updates.

## 11. Open questions / accepted defaults

- `server.max_body_bytes` default = **32 MiB** (accepted).
- No server-side image fetch/resize/re-encode (accepted non-goal).
- Anthropic/Gemini GCS/file URIs deferred (backlog).
