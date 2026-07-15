# Multimodal embeddings

An embed model opts into image input by declaring the `image` modality:

```toml
[[providers.models]]
id = "embed-multilingual"
capabilities = ["embed"]
modalities = ["text", "image"]
```

Once opted in, `input` items may be a bare string or an array of content
parts (`{"type":"text",...}` / `{"type":"image_url",...}`), and both kinds
of item can be mixed within one batch. Image input sent to a model without
the `image` modality is rejected with `LM-2003` (400) before any upstream
call. See [Embeddings](embeddings.md) for the other accepted `input` shapes.

## Image sources

`image_url.url` is a `data:<media-type>;base64,<payload>` inline URI or a
remote `http(s)` URL.

- **`data:` URIs always work** - the bytes travel in the request body, no
  fetch required.
- **Remote `http(s)` URLs require `[image_fetch] enabled = true`.** With
  fetching disabled (the default), a remote URL is rejected with `LM-2005`
  (400) - pass a `data:` URI instead, or enable fetching.

## The guarded fetch

When `[image_fetch]` is enabled, a remote URL is fetched server-side,
base64-encoded, and inlined before being sent to the provider. The fetch is
guarded:

- The private/loopback/link-local IP block is non-configurable (always on).
- The connection is pinned to the resolved address that was vetted against
  that block, so a DNS answer cannot rebind to a different address after the
  check (DNS-rebinding safe).
- Scheme, host, and URL-prefix allowlists (`allowed_schemes`,
  `allowed_hosts`, `allowed_url_prefixes`).
- A streamed size cap (`max_bytes`) and a fetch timeout (`timeout_ms`).
- Only `image/*` response content types are accepted.
- Redirects are re-validated against the same guards, not just the original
  URL.
- A per-request cap on the number of images fetched.

A request rejected by any of these guards fails with `LM-2006` (400); the
specific reason is logged server-side only, never returned to the client. A
URL that passes the guards but fails at the remote host (network error,
timeout, or error status) fails with `LM-2007` (502). See
[Error codes](../errors.md).

## Config reference

```toml
# ---------------------------------------------------------------------------
# Multimodal image fetching (M9) - OFF by default.
#
# When enabled, a remote http(s) image URL in a /v1/embeddings content-parts
# request is fetched server-side, base64-encoded, and inlined as a data: URI
# before being sent to the provider. Disabled → such a URL is rejected with
# LM-2005 (clients must pass a data: URI themselves).
#
# Guards (always on when enabled): the private/loopback/link-local IP block is
# non-configurable; the connection is pinned to the vetted resolved address
# (DNS-rebinding safe); downloads are capped and time-bounded; only image/*
# content types are accepted. Restrict `allowed_hosts` / `allowed_url_prefixes`
# to your own asset origins in production - leaving both empty with fetching on
# logs a startup warning.
# ---------------------------------------------------------------------------
[image_fetch]
enabled = false
max_bytes = 10485760          # 10 MiB per image
timeout_ms = 5000             # per-fetch timeout
allowed_schemes = ["https"]   # add "http" to allow plaintext
allowed_hosts = []            # e.g. ["cdn.example.com", ".mycompany.com"]; empty = any public host
allowed_url_prefixes = []     # e.g. ["https://cdn.example.com/images/"]; empty = no prefix restriction
```

## Per-provider semantics

Not every multimodal provider embeds mixed input the same way: Cohere
(embed-v4) and Voyage embed one combined text+image vector per item, while
Jina embeds one modality per item - a mixed item is sent as its image, and
its caption text is not combined into the vector. See
[Providers](../providers.md) for the full per-kind notes.

## Accounting

Every fetched or inlined image is counted: `lumen_media_total` and
`lumen_media_bytes_total` in Prometheus, and matching columns in the
`usage_log` table. See [Metrics](../operations/metrics.md).
