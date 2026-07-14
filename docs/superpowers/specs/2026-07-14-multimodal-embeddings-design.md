# M9 — Multimodal embeddings + guarded image fetch

**Status:** approved design, pre-implementation
**Date:** 2026-07-14
**Author:** LUMEN maintainers
**Relation to M8:** M8 (vision image input to chat) introduces the content-parts
shape but forbids the gateway from ever dereferencing an image URL. This
milestone lifts a *narrow, opt-in, guardrailed* exception for the **embeddings**
endpoint only, and introduces the shared `ContentPart` / `ImageUrl` core types
that M8 will reuse. M8's chat-vision "never fetch" stance is unchanged.

## 1. Summary

Accept image inputs in `POST /v1/embeddings` using the OpenAI content-parts
shape (same mental model as M8 chat vision), and add an **opt-in, guardrailed
server-side fetch stage** that resolves remote `http(s)` image URLs to inline
`data:` URIs *before* provider translation. Downstream providers therefore only
ever receive inline base64.

- **Content-parts input:** `input` accepts a string, an array of strings, or an
  array whose items are strings *or* arrays of typed content parts
  (`text` + `image_url`), mixable within one item.
- **Guarded fetch (default OFF):** when enabled, remote image URLs are fetched
  under SSRF and resource guards (private-IP block, scheme/host/prefix
  allowlists, size cap, timeout, MIME allowlist), base64-encoded, and rewritten
  in place to `data:` URIs.
- **Multimodal translation:** per-provider translation for every embedding
  provider that supports image input (Cohere, Voyage, Jina). Non-capable models
  fail fast via a `modalities` check.

Multimodal embedding is a **sub-capability of embed**: same endpoint, same
`Embed` routing capability, same batching path. No new top-level `Capability`
variant.

## 2. Scope

**In scope**
- Widen `EmbedInput` to accept content-parts items (text + image, mixed per item)
  while preserving the existing text-only `Single` / `Batch` fast paths verbatim.
- Shared `ContentPart` / `ImageUrl` types in `crates/core/src/content.rs`.
- Opt-in guarded image fetch stage (`crates/providers/src/image_fetch.rs`) with
  SSRF + resource guards, resolving remote URLs to `data:` URIs.
- Per-model `modalities` declaration (shared with M8), surfaced in
  `GET /v1/models`; fail-fast enforcement (`LM-2003`) for image input to a
  non-image-capable model.
- Multimodal request translation for Cohere, Voyage, Jina; text-only path
  unchanged for all providers.
- Token accounting: trust upstream usage; text-only estimation fallback.
- Tests (wiremock + mock image host): per-provider translation, fetch guards,
  enforcement, data-URI passthrough, accounting, cancellation.

**Out of scope (later slices / backlog)**
- Image *generation*, audio, any non-image modality (the `modalities` field
  parses `"audio"` etc. but only `"image"` is acted on).
- Server-side image resizing / re-encoding / format conversion. Bytes are
  fetched and base64-encoded as-is (subject to the size cap and MIME allowlist).
- Reusing the fetch stage for M8 chat vision (explicitly deferred; chat vision
  keeps "never fetch").
- Partial-batch results: a single sub-batch or single image failure fails the
  whole request (matches the existing embeddings contract).
- Caching of fetched images.

## 3. Core type changes

### 3.1 Shared content parts — `crates/core/src/content.rs` (NEW)

Extracted so M8 chat vision reuses the identical types (M8's design sketches
them in `chat.rs`; this milestone lands them first, in a shared module).

```rust
/// One element of a content-parts array. Modelled as a typed struct with a
/// `flatten`ed `extra` map (the codebase idiom) so unknown/future part types
/// survive round-trip verbatim rather than 400.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentPart {
    /// `"type"` defaults to `"text"` when omitted, so `{"text":"hi"}` and
    /// `{"image_url":{...}}` are valid without spelling out the type. Real
    /// OpenAI-shaped parts (which always send `type`) still parse.
    #[serde(rename = "type", default = "default_kind")]
    pub kind: String,                 // "text" | "image_url" | future
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<ImageUrl>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_kind() -> String { "text".to_owned() }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}
```

**Part dispatch is by field presence, not `kind`.** Because `kind` now defaults
to `"text"`, `kind` and the populated field can disagree (an untyped
`{"image_url":{...}}` carries `kind == "text"`). Therefore image detection and
provider translation dispatch on **which field is set**: a part is an image iff
`image_url.is_some()`; otherwise it is text (`text`, else empty). `kind` is
retained for round-trip fidelity and forward-compat (unknown part types keep
their declared `kind` + `extra`), but it never drives the image-vs-text
decision. `has_image()` is defined as "any part with `image_url.is_some()`".

### 3.2 Widened `EmbedInput` — `crates/core/src/embed.rs`

```rust
/// Input to an embedding request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbedInput {
    /// `"input": "hello"` — unchanged text fast path.
    Single(String),
    /// `"input": ["a","b"]` — unchanged text fast path.
    Batch(Vec<String>),
    /// `"input": ["a", [{"type":"text",...},{"type":"image_url",...}], ...]`
    /// A heterogeneous batch; each item is text or an array of content parts.
    Multi(Vec<EmbedItem>),
}

/// One item of a multimodal embedding batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbedItem {
    /// A bare string item, tried first (untagged order matters).
    Text(String),
    /// An array of typed content parts (text and/or image), order preserved.
    Parts(Vec<ContentPart>),
}
```

**Untagged ordering.** `Single` (string) and `Batch` (array of strings) are
tried before `Multi` (array whose items may be arrays/objects). A JSON string
never parses as `Batch`/`Multi`; a `["a","b"]` array parses as `Batch` (all
strings) and only falls to `Multi` when an item is itself an array of objects.
Serde untagged tries variants in declaration order and this ordering is asserted
by a regression test.

**Helpers on `EmbedInput`:**
- `fn len(&self) -> usize` — item count (unchanged semantics; `Multi` → number
  of items).
- `fn is_empty(&self) -> bool`.
- `fn text_iter(&self) -> impl Iterator<Item = &str>` — concatenated text across
  all items (image parts contribute nothing); used by `estimate_embed_input`.
- `fn has_image(&self) -> bool` — any `image_url` part present. Used by
  enforcement.
- The existing `iter(&self) -> impl Iterator<Item = &str>` stays for the
  text-only paths; on `Multi` it yields each item's concatenated text.

**Blast radius.** The text-only `Single` / `Batch` variants are byte-identical
to today, so `batch.rs` text batching, Cohere's `texts` mapping, and the
OpenAI-compatible passthrough providers (Voyage, Jina, OpenAI, Ollama, TEI) all
compile and behave unchanged for text requests. New code paths are only entered
when `Multi` is present. `estimate_embed_input` switches to `text_iter()`.

## 4. Guarded image fetch — `crates/providers/src/image_fetch.rs` (NEW)

A resolution stage invoked by the embeddings handler **before** batching and
provider translation. It walks every `ContentPart` with `kind == "image_url"`:

- `data:` URI → left untouched (no fetch).
- remote `http(s)` URL:
  - fetch **disabled** → `LM-2005` (400).
  - fetch **enabled** → validate against guards (§4.1); on pass, fetch the
    bytes, base64-encode, and **rewrite the part's `image_url.url` in place** to
    `data:<mime>;base64,<payload>`. On guard failure → `LM-2006` (400); on
    upstream fetch failure/timeout → `LM-2007` (502).
- any other scheme → `LM-2006` (400).

After this stage, all `image_url` values are `data:` URIs, so provider
translation (§5) never sees a remote URL.

### 4.1 Guards (config `[image_fetch]`, §6)

A remote URL is fetched only if **all** hold:

1. **Scheme** ∈ `allowed_schemes` (default `["https"]`; `"http"` opt-in).
2. **Host allowlist** — `allowed_hosts` empty, or the parsed URL host matches an
   entry: exact match, or a leading-dot suffix entry (`.mycompany.com` matches
   `assets.mycompany.com` and `mycompany.com`). Matching is on the parsed host
   component only, never a substring of the raw URL (so
   `https://evil.com/?x=cdn.example.com` cannot slip through).
3. **Prefix allowlist** — `allowed_url_prefixes` empty, or the URL string starts
   with one of the configured prefixes.
4. **Private-IP block (always on, non-configurable)** — resolve the host's DNS;
   if **any** resolved address is loopback, private (RFC 1918), link-local
   (169.254/16, fe80::/10), unique-local (fc00::/7), unspecified, or otherwise
   non-global, reject. The connection is **pinned to a vetted resolved IP**
   (via `reqwest`'s `resolve`/pre-resolved address) so a second DNS lookup at
   connect time cannot rebind to an internal address (DNS-rebinding safe).
5. **Size cap** — `max_bytes` (default 10 MiB). Enforced while streaming the
   body; a declared or observed over-limit body is rejected without buffering
   the whole payload. A `Content-Length` over the cap is rejected before
   reading.
6. **MIME allowlist** — response `Content-Type` must be `image/*`; the concrete
   subtype becomes the `data:` URI media type. A missing/again non-image type
   → `LM-2006`.

Additional properties:
- **Timeout** — `timeout_ms` (default 5000) bounds the whole fetch.
- **Cancellation** — the fetch honors the request `CancellationToken`; a client
  disconnect aborts in-flight fetches (consistent with the M4 guarantee).
- **Bounded concurrency** — images within one request are fetched concurrently
  with a small bound (reuse `batch::DEFAULT_CONCURRENCY` = 4).
- **No redirects to internal targets** — redirects are followed only if the
  redirect target re-passes guards 1–4; otherwise the fetch fails `LM-2006`.
  (Implemented by disabling automatic redirects and validating each hop, or an
  equivalent redirect policy.)
- **No secrets / no internal detail** — the client-facing error carries only the
  coarse `LM-2006`/`LM-2007` category; the specific rejected address or reason
  is logged server-side at `debug`, never returned or logged at info+ with URL
  query strings.

### 4.2 Performance-pillar note (conscious exception)

Fetching adds network latency **in the request path**, which the <1 ms-overhead
pillar otherwise forbids. This is an accepted, bounded exception because: it is
**opt-in and default OFF**; it applies to the **embeddings** path (not the chat
streaming hot path); it is bounded by `timeout_ms`; and it is user-initiated
enrichment, not gateway proxy overhead. Documented in `docs/adr/` as an addendum
note; no behavior change when disabled (the stage is a no-op for text and
`data:` URIs).

## 5. Provider translation (multimodal)

Text-only requests (`Single` / `Batch`, or a `Multi` of only `Text` items with
no images) use each provider's **existing** text path unchanged. When image
parts are present, the capable providers translate to their native multimodal
request, using the inline base64 produced by §4:

| Provider | Text-only | Multimodal (image parts present) |
|---|---|---|
| **Cohere** | `POST /v2/embed` `{ texts, input_type }` (today) | `POST /v2/embed` with `inputs` content array (embed-v4 multimodal): each item → `{ content: [ {type:"text",text}, {type:"image_url", image_url:{url:<data-uri>}} ] }` |
| **Voyage** | OpenAI-compatible `/v1/embeddings` (today) | `POST /v1/multimodalembeddings` `{ inputs: [ { content: [ {type:"text",text}, {type:"image_base64", image_base64:<data-uri>} ] } ] }` |
| **Jina** | OpenAI-compatible `/v1/embeddings` (today) | `POST /v1/embeddings` with `input: [ {text}, {image:<base64-or-data-uri>} ]` per Jina's multimodal shape |
| **OpenAI / Ollama / TEI** | text only | **not image-capable** → rejected by §6 enforcement (`LM-2003`) before upstream |

- Exact translated JSON is asserted per provider in tests (§8).
- Part ordering within an item is preserved.
- `max_batch_size` for multimodal is provider-specific; batching (`batch.rs`)
  extends to split `Multi` item lists the same way it splits text batches,
  reassembling in original order. (Multimodal batch ceilings are typically
  smaller; each provider keeps its own constant.)

## 6. Config & routing

### 6.1 Per-model `modalities` (shared with M8)

```toml
[[providers.models]]
id = "voyage-multimodal-3"
capabilities = ["embed"]
modalities = ["text", "image"]   # NEW; default ["text"]
```

- `GET /v1/models` adds `"modalities"` per model entry.
- Routing is unchanged: multimodal embed requests route as `Capability::Embed`.
- **Enforcement (fail fast):** in the embeddings handler, before the upstream
  call, if the input `has_image()` and the resolved model's modalities lack
  `"image"` → `LM-2003` (400). No upstream round-trip.
- `modalities` is a plain `Vec<String>` (not an enum) so future modalities parse
  without a schema bump.

### 6.2 `[image_fetch]` config block (default OFF)

```toml
[image_fetch]
enabled = false               # opt-in; server-side fetch of remote image URLs
max_bytes = 10485760          # 10 MiB per image
timeout_ms = 5000             # per-fetch timeout
allowed_schemes = ["https"]   # add "http" to allow plaintext
allowed_hosts = []            # e.g. ["cdn.example.com", ".mycompany.com"]; empty = any public host
allowed_url_prefixes = []     # e.g. ["https://cdn.example.com/images/"]; empty = no prefix restriction
```

- Defaults are safe: fetching off; when on, `https` only, private IPs always
  blocked.
- **Startup warning:** if `enabled = true` while both `allowed_hosts` and
  `allowed_url_prefixes` are empty, log a `warn` at startup ("image fetch enabled
  with no host/prefix allowlist; only scheme and private-IP guards apply"). Not
  an error — the private-IP block still holds.
- `server.max_body_bytes` (introduced by M8, default 32 MiB) also applies to the
  embeddings route, since inline `data:` URIs and fetched images inflate the JSON
  body. If M8 has not yet landed, this milestone adds `server.max_body_bytes`
  wiring for the embeddings route as part of its work.

## 7. Error taxonomy (`docs/errors.md`)

| Code | HTTP | Meaning |
|---|---|---|
| `LM-2003` | 400 | Image input sent to a model whose `modalities` lacks `"image"` *(shared with M8)*. |
| `LM-2005` | 400 | A remote image URL was supplied but server-side image fetch is disabled. |
| `LM-2006` | 400 | Remote image URL rejected by a fetch guard (scheme, host/prefix allowlist, private-IP block, size cap, or non-image MIME). Client response carries no internal address or reason detail. |
| `LM-2007` | 502 | The upstream image host failed or timed out during a permitted fetch. |

Reused:
- `LM-1001` (400) — malformed content parts / malformed `data:` URI.
- `LM-1002` (413) — request body exceeded `server.max_body_bytes`.

No image bytes, no internal IPs, and no secrets ever appear in an error message.

## 8. Token accounting (Pillar 5, ADR 003 addendum)

- **Primary:** upstream `usage` is trusted verbatim; multimodal providers fold
  image tokens into `prompt_tokens`, so accounting is correct with no extra work.
- **Fallback (upstream reported no usage):** estimate from text parts only via
  `text_iter()`; image parts contribute **0**; the response is flagged
  `estimated: true`. Documented as a known undercount for image-heavy requests
  on no-usage upstreams. A backlog item covers a future per-image heuristic. No
  new ADR; an addendum note is added to ADR 003 (shared with M8's addendum).

## 9. Testing (wiremock + mock image host, per the Definition of Done)

- **Core serde:** text `Single`/`Batch` round-trip (regression); `Multi` with
  text + image parts round-trips; a part with no `type` defaults to
  `kind == "text"`; an untyped `{"image_url":{...}}` is still detected as an
  image (dispatch by field presence, §3.1); an unknown part type survives
  verbatim via `kind` + `extra`; `text_iter()` / `has_image()` unit tests;
  untagged order (a
  string never parses as `Batch`/`Multi`; an all-strings array parses as
  `Batch`).
- **Fetch guards (unit + wiremock image host):** private/loopback/link-local IP
  rejected (`LM-2006`, no upstream fetch to provider); disallowed scheme
  rejected; host not in `allowed_hosts` rejected; URL not matching
  `allowed_url_prefixes` rejected; over-`max_bytes` body rejected without full
  buffering; non-image `Content-Type` rejected; happy path fetches bytes and
  produces a correct `data:` URI; redirect to an internal target rejected.
- **Disabled fetch:** remote URL with `enabled = false` → `LM-2005` (400), no
  provider call.
- **data-URI passthrough:** a `data:` URI is forwarded without any fetch.
- **Provider translation:** Cohere / Voyage / Jina multimodal request JSON
  asserted exactly against the mock upstream; text-only requests still hit the
  existing paths byte-for-byte.
- **Enforcement:** image part to a `["text"]` embed model → `LM-2003` (400), no
  upstream call.
- **Accounting:** multimodal request with no upstream usage → `estimated: true`,
  text tokens counted, image parts contribute 0, no panic.
- **Cancellation:** client disconnect during an in-flight image fetch aborts the
  fetch (and any in-flight upstream call).

## 10. Milestone breakdown (tests-first, one atomic commit each)

1. **Core types** — `content.rs` (`ContentPart`/`ImageUrl`), widen `EmbedInput`
   with `Multi`/`EmbedItem`, `text_iter()`/`has_image()`; serde round-trip +
   regression + untagged-order tests. Update `estimate_embed_input` to
   `text_iter()`.
2. **Config + routing** — `modalities` config field, `/v1/models` exposure,
   `[image_fetch]` config block + startup warning, enforcement (`LM-2003`) +
   tests.
3. **Guarded fetch** — `image_fetch.rs` with all §4.1 guards, `data:`-URI
   rewrite, `LM-2005/2006/2007`, cancellation; unit + mock-host tests.
4. **Cohere multimodal translation** — `/v2/embed` `inputs` content array +
   tests.
5. **Voyage + Jina multimodal translation** — `/v1/multimodalembeddings` and
   Jina `input` list + tests; batching (`batch.rs`) extension for `Multi`.
6. **Finish** — accounting addendum, body-limit wiring if M8 hasn't landed,
   conformance suite, gate (`cargo test --workspace && cargo clippy --workspace
   --all-targets -- -D warnings && cargo fmt --check`), code-reviewer,
   docs-writer (README matrix, `docs/providers.md`, `docs/errors.md`,
   `config.example.toml`), CHANGELOG, ROADMAP/backlog updates, and a note in the
   M8 spec that `ContentPart`/`ImageUrl` now live in `crates/core/src/content.rs`.

## 11. Open questions / accepted defaults

- `image_fetch.enabled` default = **false** (opt-in) — accepted.
- `image_fetch.max_bytes` default = **10 MiB**, `timeout_ms` = **5000**,
  `allowed_schemes` = **["https"]** — accepted.
- Empty `allowed_hosts`/`allowed_url_prefixes` with fetch enabled = allow any
  public host (private-IP block still on) + startup `warn` — accepted.
- No server-side image resize/re-encode — accepted non-goal.
- Fetch stage not reused for M8 chat vision in this slice — accepted.
