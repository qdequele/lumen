# ADR 018 - Speech-to-text: file and live transcription capabilities

- Status: proposed
- Date: 2026-10-07
- Builds on: ADR 003, ADR 004, ADR 005, ADR 009, ADR 014, ADR 015
- Depends on: ADR 019 (client tokens, for browser and mobile callers)
- Amends: the v1 non-goal "image/audio support" (CLAUDE.md) and the
  "Audio (input/output)" item deferred to v2 in `docs/backlog.md`, for audio
  input transcribed to text only

## Context

Operators want transcription behind the same keys, budgets, quotas, token
accounting, fallbacks and metrics as chat, embeddings and rerank. They need
two modes:

- **File transcription.** A finished recording is uploaded and the transcript
  comes back, either whole or as streamed text events.
- **Live transcription.** Audio from a microphone, a call or a media stream is
  sent while it is being produced, and text comes back continuously over a
  session that can last an hour.

The market as of 2026-10:

| Provider | File models | Live models | Wire API |
|---|---|---|---|
| OpenAI | `gpt-transcribe` (recommended), `gpt-4o-transcribe`, `gpt-4o-mini-transcribe`, `gpt-4o-transcribe-diarize`, `whisper-1` | `gpt-live-transcribe`, `gpt-transcribe` | `POST /v1/audio/transcriptions` (multipart); Realtime WebSocket |
| Groq | `whisper-large-v3-turbo`, `whisper-large-v3` | none | OpenAI-compatible file API |
| Mistral | `voxtral-mini-2602` | Voxtral Realtime | OpenAI-compatible file API; own live protocol |
| vLLM (self-hosted) | open-weight Whisper and Voxtral | not covered here | OpenAI-compatible file API |
| ElevenLabs | `scribe_v2` | `scribe_v2_realtime` | `POST /v1/speech-to-text` (multipart, own fields); own WebSocket protocol |

Four properties make audio unlike every existing capability:

1. **The file request is multipart, not JSON**, and the `model` field may
   arrive after the file part, so the gateway cannot pick a provider before
   it has read the whole upload.
2. **Uploads are large.** OpenAI caps files at 25 MB, Groq at 100 MB,
   ElevenLabs at 5 GB. The global `RequestBodyLimitLayer` is sized for JSON.
3. **Billing is mostly by duration, not tokens.** OpenAI reports either
   `usage: {type: "tokens", ...}` or `usage: {type: "duration", seconds}`
   depending on the model, Groq and Mistral bill per minute or hour (Groq with
   a 10 s minimum), and ElevenLabs reports nothing but word timestamps.
   Pillar 5 (never-zero token count, ADR 003) still has to hold.
4. **A live session is long-lived and bidirectional.** Admission, budgets,
   quotas, fallback, cancellation and shutdown all assume one request and one
   response today.

## Decision

### 1. Two capabilities

- `transcribe`: `POST /v1/audio/transcriptions`, OpenAI wire format,
  optionally streamed as SSE (`stream=true`).
- `transcribe_live`: `GET /v1/realtime` upgraded to a WebSocket, speaking
  OpenAI's Realtime transcription-session protocol.

`Capability::Transcribe` and `Capability::TranscribeLive` serialize as
`"transcribe"` and `"transcribe_live"` everywhere a capability string appears
(config `capabilities = [...]`, `GET /v1/models`, the Prometheus `capability`
label, `usage_log.capability`, the admin usage filter). They are separate
because many models only serve one mode (`whisper-1`, every Groq model and
`scribe_v2` are file-only, `scribe_v2_realtime` is live-only), and a client
must be able to tell from `GET /v1/models` which mode a model supports.

Two traits in `crates/core`, each taking a `CancellationToken`:

```rust
#[async_trait]
pub trait TranscriptionProvider: Send + Sync {
    async fn transcribe(&self, req: TranscriptionRequest, cancel: CancellationToken)
        -> Result<TranscriptionResponse, ProviderError>;
    async fn transcribe_stream(&self, req: TranscriptionRequest, cancel: CancellationToken)
        -> Result<BoxStream<'static, Result<TranscriptionEvent, ProviderError>>, ProviderError>;
}

#[async_trait]
pub trait LiveTranscriptionProvider: Send + Sync {
    /// Open an upstream session with the already validated session config.
    async fn open(&self, cfg: LiveSessionConfig, cancel: CancellationToken)
        -> Result<Box<dyn LiveSession>, ProviderError>;
}
```

`LiveSession` exposes a bounded sink of `LiveClientEvent` and a stream of
`LiveServerEvent`, both in LUMEN's neutral types (section 4). A provider whose
API has no native file streaming gets a default `transcribe_stream` that
calls `transcribe` and emits one `transcript.text.delta` with the whole text
followed by `transcript.text.done`, so clients code against one event shape.

Out of scope for this ADR: `/v1/audio/translations` (speech to English text),
text-to-speech, voice-agent Realtime sessions (`session.type = "realtime"`),
and WebRTC. Each is a separate decision.

### 2. File transcription: request handling

- **Route-scoped body limit.** `/v1/audio/transcriptions` gets its own limit,
  `[transcribe] max_upload_bytes` (default 25 MiB), and is exempt from the
  global JSON limit. Over the limit is `LM-1002` as today. A provider whose
  own cap is lower (per-model `max_upload_bytes`) answers `LM-1002` naming
  the provider before any upstream call.
- **The upload is buffered, deliberately.** The whole file is read into one
  refcounted `Bytes`. That is required twice over: the provider is only known
  once `model` has been read, which may be after the file part, and a retry
  or fallback (ADR 005) must replay the body. Replays cost a refcount, never
  a copy. Memory is bounded by a gateway-wide semaphore over buffered audio
  bytes, `[transcribe] max_inflight_upload_bytes` (default 256 MiB). An
  upload that cannot get its permit within `upload_permit_wait_ms` (default
  2000) is refused with `LM-5002` (503, `Retry-After`), so a burst of uploads
  degrades into retries, not into an out-of-memory kill.
- **Typed fields, everything else carried.** LUMEN parses the fields it acts
  on: `model`, `file`, `response_format`, `stream`, `language`, `languages[]`,
  `prompt`, `keywords[]`, `temperature`, `timestamp_granularities[]`,
  `chunking_strategy`, `include[]`. Other parts go into an `extra` map, as
  chat and embeddings already do (`crates/core/src/chat.rs`,
  `crates/core/src/embed.rs`): forwarded verbatim to OpenAI-compatible
  upstreams, and rejected with `LM-1001` by a translated provider in `strict`
  mode when it has no equivalent (the existing chat-extras rule).
- **Response formats.** `json` and `text` are guaranteed on every provider.
  `verbose_json`, `srt`, `vtt` and `diarized_json` are passed through on
  OpenAI-compatible upstreams. On a translated provider they are mapped where
  the upstream has the data (ElevenLabs `verbose_json` from its `words`) and
  otherwise rejected with `LM-1001` before any upstream call.
- **Streaming.** `stream=true` returns `text/event-stream` with
  `transcript.text.delta`, optional `transcript.text.segment`, and a final
  `transcript.text.done`. It uses the ADR 004 passthrough and the ADR 005 rule
  that a fallback is only possible before the first event is sent.

### 3. File transcription: providers in the first release

- **One OpenAI-compatible adapter** for the `openai`, `groq`, `mistral` and
  `vllm` kinds: multipart re-emitted from the buffered parts
  (`reqwest` gains its `multipart` feature; parts are built from `Bytes` so
  the file is not copied), bearer auth, upstream JSON or SSE passed through.
- **A new `elevenlabs` provider kind** with its own adapter for
  `POST /v1/speech-to-text`:
  - field mapping in both directions (`model` to `model_id`, `language` to
    `language_code`; `text` and `language_code` back to OpenAI `text` and
    `languages: [{code}]`), `xi-api-key` auth;
  - `timestamps_granularity` is always sent as `word`, because the end of the
    last word is the only duration ElevenLabs returns (section 5); the words
    are stripped from the response unless the client asked for
    `verbose_json`;
  - `tag_audio_events` defaults to `false` so transcripts match the other
    providers (a client can set it back through `extra`);
  - provider option `zero_retention = true` sends `enable_logging=false`;
  - `source_url`, `cloud_storage_url` and `webhook=true` are refused with
    `LM-1001`: the first two would make the provider fetch an arbitrary URL
    for the client, the third returns before the transcript exists, so it can
    be neither accounted nor served.
- Azure OpenAI deployments, Deepgram and AssemblyAI are follow-ups using the
  same traits.

### 4. Live transcription: the session

- **Client protocol: OpenAI Realtime, transcription sessions only.** The
  client connects to `GET /v1/realtime`, upgrades, and sends `session.update`
  with `session.type = "transcription"` and
  `audio.input.transcription.model`. LUMEN resolves the model (virtual models
  included, ADR 014) on that first `session.update`, checks the key, admits
  the first budget lease (section 6), then opens the upstream session. Fallback
  across a virtual model's targets is possible only until the upstream session
  is open; after that, an upstream failure ends the session.
- **Fixed for the life of the session:** the session type, the model and the
  input audio format. A later `session.update` may change transcription hints
  (`prompt`, `keywords`, `languages`, `delay`) but not those three
  (`LM-1001`). `session.type = "realtime"` (a voice agent) is `LM-2002`.
- **Events LUMEN understands.** Client to server: `session.update`,
  `input_audio_buffer.append`, `input_audio_buffer.commit`,
  `input_audio_buffer.clear`. Server to client: `session.created`,
  `session.updated`, `input_audio_buffer.committed`,
  `conversation.item.input_audio_transcription.delta`,
  `conversation.item.input_audio_transcription.completed`, `error`. Other
  events are relayed unchanged to and from an OpenAI upstream, and refused
  with an `error` event (`LM-1001`) by a translated provider.
- **ElevenLabs live mapping** (`wss://api.elevenlabs.io/v1/speech-to-text/realtime`):
  `model_id`, `audio_format` (`audio/pcm` at a supported rate to
  `pcm_<rate>`, `audio/pcmu` to `ulaw_8000`), `language_code` and
  `commit_strategy` (`manual` when `turn_detection` is null, `vad` otherwise)
  go in the connect query; `input_audio_buffer.append` becomes
  `input_audio_chunk` and `commit` becomes an empty chunk with
  `commit: true`; `committed_transcript` becomes `completed` with a
  LUMEN-generated `item_id`. ElevenLabs `partial_transcript` is a snapshot of
  the whole partial text, not a delta: LUMEN emits the new suffix as a
  `delta` when the snapshot extends the previous one and drops revisions,
  since `completed` is authoritative (OpenAI's own guidance tells clients to
  reconcile partial text against `completed`). An audio format the provider
  cannot take is `LM-2014` before the upstream session opens.
- **Mistral Voxtral Realtime** follows as a third live adapter once its
  protocol is pinned; it is not part of the first release.
- **Relay.** Each direction is a bounded channel (default 64 frames). When the
  upstream is slower than the client, LUMEN stops reading the client socket,
  so backpressure reaches the client through TCP instead of growing a buffer.
  Client frames are capped at `[transcribe_live] max_frame_bytes` (default
  1 MiB). For an OpenAI upstream, a frame is inspected for its `type` and the
  length of its `audio` field with borrowed deserialization and forwarded as
  the original text; it is never re-serialized.
- **Session end.** A session ends on client close, upstream close,
  `max_session_seconds` (default 3600, capped by config and by a client
  token, ADR 019), `idle_timeout_seconds` without audio (default 120), a
  refused budget lease, or the parent key being disabled or deleted. Every
  end path cancels the upstream (rule 3), settles the last lease and writes
  one `usage_log` row. Before closing, LUMEN sends an `error` event carrying
  the `LM-` code, then closes with application code `4000 + HTTP status`
  (`4402` budget, `4429` quota or session limit, `4403` scope, `4502`
  upstream, `4500` internal) and the code as the close reason. Normal ends
  close with `1000`, shutdown with `1001`.
- **Shutdown and reload.** On SIGTERM the gateway stops accepting upgrades and
  gives open sessions `[transcribe_live] drain_timeout_seconds` (default 30)
  before closing them with `1001`. A config hot reload never touches open
  sessions; it applies to the next one.
- **Concurrency.** `[transcribe_live] max_sessions` (gateway-wide) and
  `max_sessions_per_key` bound open sessions; over either is `LM-4006` (429)
  at upgrade time. Counters are per instance, as RPM and TPM already are.

### 5. Measuring audio duration

`audio_ms` (integer milliseconds) becomes a first-class usage unit, next to
tokens and rerank search units. Sources, in priority order:

- **File transcription:**
  1. upstream `usage: {type: "duration", seconds}`, a `verbose_json`
     `duration`, or for ElevenLabs the `end` of the last word;
  2. a local probe of the buffered upload: WAV and FLAC headers, MP3 Xing or
     VBRI headers (constant bitrate computed from size), Ogg's last-page
     granule position. All are O(header) on bytes already in memory. A
     headerless variable-bitrate MP3 that needs a frame scan is probed on
     `spawn_blocking`;
  3. otherwise (M4A, MP4, WebM), size divided by an assumed 32 kbit/s,
     flagged `estimated`.

  The probe runs before the upstream call, so admission reserves against a
  measured duration rather than a guess.
- **Live transcription:** exact, from the audio LUMEN relays. PCM16 at rate
  `r` is `bytes / (2r)` seconds, G.711 is `bytes / 8000`. The byte count comes
  from the base64 length of each `audio` field, so no frame is decoded to be
  counted.

### 6. Accounting, pricing and budgets

- **Tokens (ADR 003, never zero).** Input tokens are upstream-reported when
  present (OpenAI `usage.type = "tokens"`, Realtime `completed` events that
  carry usage); otherwise `audio_ms` times a per-model
  `audio_tokens_per_second`, flagged `estimated`. Its default is calibrated
  against OpenAI-reported counts before release and documented. Output tokens
  are upstream-reported or the existing text heuristic over the transcript.
  Speech with a non-zero duration therefore never reports zero tokens.
- **Prices.** New per-model fields `cost_per_minute_audio` and
  `min_billed_seconds` (per request, or per committed turn for live; Groq
  needs 10). Cost is `cost_per_minute_audio` times billed minutes plus the
  existing `cost_per_1m_input` and `cost_per_1m_output` times tokens. An
  operator sets whichever applies and leaves the rest at zero, the same
  pattern as rerank's search-unit price plus input tokens.
- **File admission** is unchanged: one `admit` with the cost of the probed
  duration, one `settle` with the real figure.
- **Live admission uses leases.** A session opens with a normal `admit`
  (counts one request against RPM) that reserves `lease_seconds` of audio
  (default 30). Audio is forwarded upstream only while the lease covers it.
  When it is used up, LUMEN settles the lease with its real cost and admits
  the next one through a new `admit_lease` path that skips the RPM window and
  does not count as a new request in the ADR 015 counters. If the next lease
  is refused, no further audio is forwarded and the session ends with
  `LM-4001`, `LM-4003` or `LM-4004`. Audio is metered before it is forwarded,
  so a duration-priced model never overruns a hard budget, a token-priced
  one overruns it only by the gap between estimated and reported tokens
  (the same rule as every other capability: the real figure wins, the next
  admission is refused), and key revocation takes effect within one lease. TPM is debited per
  lease with the estimated input tokens.
- **Settlement of reported tokens.** When upstream usage arrives after the
  audio it covers (per-turn `completed` events), the next lease settlement,
  or the final one, trues up the difference.
- **Surfaces.**
  - Response: OpenAI `usage` when the format is JSON, with LUMEN filling it
    (`usage.type = "duration"` plus `estimated: true`) when the upstream gave
    none, as the decisions capability does (ADR 017). For `text`, `srt` and `vtt`, the same figures go
    in an `x-lumen-usage` response header. Live sessions get them in the final
    event before close.
  - Prometheus: `lumen_audio_seconds_total{capability, model, provider,
    estimated}`, `lumen_live_sessions{model, provider}` (gauge),
    `lumen_live_session_seconds` (histogram), plus the existing token
    counters.
  - `usage_log`: a nullable `audio_ms` column (new migration). One row per
    file request and one per live session.
  - Lab events (ADR 015) are unchanged: they carry settled cost, which already
    includes audio.

### 7. Privacy

Audio, transcripts, prompts and keywords are request content: never logged,
never in `usage_log`, never in error messages, exactly like chat prompts.
Provider keys stay in headers built at call time and never appear in a
WebSocket URL LUMEN logs.

### 8. New error codes

| Code | HTTP | Meaning |
|---|---|---|
| `LM-2014` | 400 | The live session's input audio format is not supported by the resolved provider. |
| `LM-4006` | 429 | Live session limit reached (gateway-wide or per key). |
| `LM-5002` | 503 | Gateway audio buffer capacity reached; retry after `Retry-After`. |

`LM-4007` and `LM-4008` are defined by ADR 019. Existing codes keep their
meaning: an oversize upload is `LM-1002`, a model without the capability is
`LM-2002`.

### 9. Opt-in and dependencies

Nothing changes for an operator who declares no model with `transcribe` or
`transcribe_live`. New dependency features: axum `multipart` and `ws`,
reqwest `multipart`, plus `tokio-tungstenite` (rustls) for upstream
WebSockets. No `openssl`, no audio decoding library.

## Consequences

- Two new routes, two traits, a new provider kind and a new usage unit. The
  file path keeps the existing shape (resolve, admit, execute, settle); the
  live path is a new shape (resolve, admit, open, relay with leases, settle).
- File uploads cost memory, up to the configured ceiling. This is the price
  of fallback and of `model` arriving after the file, and it is bounded.
- Live sessions add per-instance state (open sessions, leases) that a fleet
  restart drops. A dropped session is settled up to its last lease, and the
  client reconnects.
- A provider failing mid-session is visible to the client. There is no silent
  live fallback.
- Pillar 1 applies per frame for live sessions: relay overhead is benchmarked
  (`cargo bench`) and must stay under 1 ms p99 per frame.
- Duration probing covers the common containers. Anything else is estimated
  and labeled as such.
- The OpenAI Realtime protocol is younger and changes faster than the file
  API. LUMEN pins the event set above and treats an unknown upstream event as
  a relay-through for OpenAI and an error for translated providers.

## Alternatives considered

- **Stream uploads to the provider without buffering.** Saves memory, but
  makes fallback impossible and fails when `model` comes after the file.
  Rejected.
- **One `transcribe` capability with a per-model `live` flag.** Hides from
  `GET /v1/models` which mode a model serves. Rejected.
- **A LUMEN-specific live protocol.** Simpler to translate to, but no client
  SDK speaks it. Rejected in favor of OpenAI's transcription session, like
  every other LUMEN endpoint.
- **Charge live sessions once, at the end.** Lets one session overrun a hard
  budget by its whole length. Rejected for leases.
- **Count duration by decoding audio** (for example with `symphonia`). Exact
  for every container, but a heavy dependency with CPU cost on the request
  path. Rejected for header probes plus labeled estimates.
- **Proxy provider-issued browser tokens** (OpenAI `ek_`, ElevenLabs
  single-use tokens). Lets browsers bypass the gateway's budgets and keys.
  Rejected; see ADR 019.
