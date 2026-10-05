# Logging

Logs are the third observability leg next to
[metrics](metrics.md) and the [usage log](usage-log.md). The gateway
writes structured logs to **stdout only** (no file appender, no rotation
of its own - that is the supervisor's or the log pipeline's job).

## What is never logged

Prompts, responses and provider secrets are treated as radioactive:

- **Request and response content is never logged**, at any level,
  including `debug` and `trace`. This is the sovereignty pillar, not a
  default you can toggle. The nearest thing is an upstream's own error
  message, which is logged only after request echoes are cut out of it
  (see [Upstream error messages](#upstream-error-messages)).
- **Provider API keys and the master key never appear** in logs or errors;
  the redacting `Debug` implementations keep them out of debug output, and
  tests enforce it.

What is logged is metadata: request ids, models, providers, HTTP status,
latency, operational events (boot, config reloads, circuit-breaker
transitions, flush failures), the upstream's own error message on a failed
call (below), and the client-supplied `x-lumen-metadata`
header when present (a bounded JSON object of up to 16 keys, whole header
capped at 4 KiB). **Operators must not include secrets or prompt content in
the metadata header**, as it is logged in full and stored in the `usage_log`
table.

### Upstream error messages

When an upstream answers a client error (4xx other than 429), the gateway
logs one `warn` line, `upstream returned an error`, with `provider`,
`status` and `upstream_error`, under the request span (same `request_id`
as the `request failed` line that follows). For example, a 400 from
Anthropic carries Anthropic's `error.message`. The rules (ADR 016):

- Only the vendor's message **string** is logged (`error.message`,
  `message`, `error_description`, `error`, `detail`). A body without one, a
  non-JSON body, or a structured validation report is never logged, since
  it can quote the request.
- The message is cut where a validation error starts echoing the request
  (`'input'`, `"input"`, `input_value`, `input=`, as pydantic-based hosts
  such as vLLM do), so request content does not reach the log.
- Probable credentials are replaced by `<redacted>`, then the message is cut
  to 512 characters.
- It never reaches the client, whose body stays the `LM-3003` envelope.

To drop the line entirely, filter its target at the subscriber, for example
`RUST_LOG=info,lumen_providers::mapping=error`.

## Format

`log_format` is a top-level config key:

```toml
log_format = "json"   # "pretty" (default) or "json"
```

- `pretty`: human-readable, colored, for local development.
- `json`: one JSON object per line with event fields flattened, for
  production pipelines (Loki, CloudWatch, journald + a parser, ...).

The format is boot-time only (logging initializes before anything else
runs, right after config parses).

## Level and filtering: `RUST_LOG`

Filtering uses the standard `tracing` env-filter syntax via the `RUST_LOG`
environment variable. When `RUST_LOG` is unset, the gateway runs at
**`info`**.

```bash
RUST_LOG=debug lumen --config config.toml            # everything, verbose
RUST_LOG=warn lumen --config config.toml             # quiet: warnings and errors
RUST_LOG="info,lumen=debug,lumen_server=debug" lumen ...   # gateway binary + HTTP layer
RUST_LOG="info,lumen_providers=debug" lumen ...      # debug for provider translation
```

Targets follow Rust module paths: the binary crate is `lumen` (its
per-request events use the `lumen::http` and `lumen::usage` targets); the
libraries are `lumen_server`, `lumen_providers`, `lumen_router`,
`lumen_auth`, `lumen_telemetry`, `lumen_core` (ADR 001 naming).

Where `debug` is specifically useful: **dropped OpenAI extras on
translated providers**. In lenient mode, a request field with no
equivalent on the target provider (see
[chat completions](../chat/completions.md)) is dropped with a `debug`
line naming the field. If a client swears it sent `logprobs` and nothing
happened, this is where the evidence is.

**Rejected metadata** needs no debug: malformed `x-lumen-metadata` never
fails a request; it is dropped with a `warn` line (visible at the default
level) and a `lumen_metadata_rejected_total` increment - see the
[usage log](usage-log.md).

## Docker and systemd

Both capture stdout natively:

```bash
docker logs <container>                  # or a logging driver
journalctl -u lumen.service -f           # systemd unit from the deployment page
```

For production pipelines set `log_format = "json"` (the
[monitoring rig](metrics.md#see-it-live-the-monitoring-rig) runs this way)
and let the collector parse one object per line.
