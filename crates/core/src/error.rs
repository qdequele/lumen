//! Error taxonomy.
//!
//! Two layers:
//!
//! * [`ProviderError`] - what a provider returns. Never contains secrets.
//! * [`GatewayError`] - what the gateway returns to the client. Carries a
//!   stable `LM-XXXX` code (documented in `docs/errors.md`), an HTTP status,
//!   and a coarse [`ErrorType`].
//!
//! The taxonomy always distinguishes three situations (see `CLAUDE.md` rule 8):
//! a client error (4xx / `invalid_request`), an upstream provider error
//! (502/503/504 / `upstream_error`, always naming the provider), and an
//! internal gateway error (500 / `internal`). A gateway malfunction must never
//! be reported as a misleading 401 (lesson: OpenRouter outages).
//!
//! One more situation sits outside that three-way split: a client-initiated
//! cancel (499 / `client_cancelled`, `LM-6xxx`, see `docs/adr/006-*.md`). The
//! gateway didn't malfunction and the client isn't at fault, so it is kept
//! out of both `internal` and `invalid_request` rather than inflating either
//! (issue #11).

use crate::capability::Capability;
use serde::Serialize;
use std::time::Duration;

/// An error returned by a provider implementation.
///
/// Variants never embed API keys, `Authorization` headers or prompt content.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderError {
    /// The upstream returned a non-success HTTP status.
    #[error("provider '{provider}' returned HTTP {status}")]
    Upstream {
        provider: String,
        status: u16,
        /// Whether retrying (possibly on a fallback) may succeed.
        retryable: bool,
    },

    /// The upstream did not respond within the configured timeout (read /
    /// overall). Distinct from [`ConnectTimeout`](ProviderError::ConnectTimeout).
    #[error("provider '{provider}' timed out")]
    Timeout { provider: String },

    /// The gateway could not establish a connection to the upstream within the
    /// connect timeout - the TCP/TLS handshake never completed. Distinct from a
    /// read timeout so operators can tell a dead host from a slow one (LM-3012).
    #[error("provider '{provider}' connection timed out")]
    ConnectTimeout { provider: String },

    /// The upstream produced no first sign of life (response headers, or the
    /// first SSE frame) within the first-token deadline (M6 §6.4). Imposed by
    /// the gateway, but modelled here so it flows through the retry/fallback
    /// executor like any other retryable timeout; surfaces as LM-3011.
    #[error("provider '{provider}' produced no first token in time")]
    FirstTokenTimeout { provider: String },

    /// The upstream could not be reached at all (DNS failure, connection
    /// refused, TLS error) - distinct from an HTTP error status.
    #[error("provider '{provider}' is unreachable")]
    Unavailable { provider: String },

    /// The downstream client disconnected; the upstream call was aborted.
    #[error("request cancelled")]
    Cancelled,

    /// A request or response could not be translated to/from the upstream schema.
    #[error("translation error: {0}")]
    Translation(String),

    /// The provider handling this attempt only accepts inline base64 image
    /// data, but the request carried a remote (`http`/`https`) image URL.
    ///
    /// Distinct from [`Translation`](ProviderError::Translation): this is a
    /// client-input problem (LM-2004), not a malformed/unparseable upstream
    /// response. It surfaces when the LM-2004 pre-flight in the handler only
    /// inspected the primary route (`chain[0]`) and a fallback link that
    /// cannot take a remote URL (e.g. Gemini) ends up being attempted - the
    /// gateway never fetches the URL on the caller's behalf, so the honest
    /// answer is a 4xx naming the incapable provider, not a 502 (GH #13).
    #[error("provider '{provider}' requires inline base64 image data; remote image URLs are not supported")]
    ImageUrlNotSupported { provider: String },

    /// The client set a request field the provider cannot honor, and the
    /// provider is running in strict mode (rather than silently dropping it).
    /// A client fault: never retried, never counted against provider health,
    /// and surfaced as a 400 (`LM-1001`). Names the field and provider.
    #[error("provider '{provider}' does not support the '{field}' field")]
    UnsupportedField {
        /// The provider that rejected the field.
        provider: String,
        /// The offending request field (e.g. `dimensions`).
        field: String,
    },

    /// The request carried an input shape the provider cannot consume at all
    /// (e.g. pre-tokenized token-id arrays sent to a text-only embed API).
    /// Rejected BEFORE any upstream call so the client gets an honest 400
    /// (`LM-1001`) instead of an empty result or an opaque upstream error
    /// (rule 8). A client fault: never retried, never counted against provider
    /// health.
    #[error("provider '{provider}' does not support {reason}")]
    UnsupportedInput {
        /// The provider that rejected the input.
        provider: String,
        /// What the input was (e.g. `pre-tokenized input (token id arrays)`).
        reason: String,
    },

    /// The upstream signalled rate limiting (HTTP 429).
    #[error("provider '{provider}' rate limited the request")]
    RateLimited {
        provider: String,
        /// The upstream `Retry-After`, if provided.
        retry_after: Option<Duration>,
    },
}

impl ProviderError {
    /// Whether retrying this call (on the same provider, or a fallback) may
    /// succeed. Retryable: 5xx upstream, connect/read timeouts, unreachable
    /// host, 429. Never retryable: a client-fault 4xx, a schema/translation
    /// error (deterministic), an image-incapable provider (deterministic -
    /// no fallback further down the chain is any more likely to fetch the
    /// URL), or a cancellation (M6 §6.1 - never retry 4xx).
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Upstream { retryable, .. } => *retryable,
            ProviderError::Timeout { .. }
            | ProviderError::ConnectTimeout { .. }
            | ProviderError::FirstTokenTimeout { .. }
            | ProviderError::Unavailable { .. }
            | ProviderError::RateLimited { .. } => true,
            ProviderError::Cancelled
            | ProviderError::Translation(_)
            | ProviderError::ImageUrlNotSupported { .. }
            | ProviderError::UnsupportedField { .. }
            | ProviderError::UnsupportedInput { .. } => false,
        }
    }

    /// Whether this failure indicates the *provider* is unhealthy and should
    /// count against its circuit breaker. A deterministic client/translation
    /// error, an image-incapable provider, or a cancellation says nothing
    /// about provider health.
    #[must_use]
    pub const fn is_provider_fault(&self) -> bool {
        match self {
            ProviderError::Upstream { retryable, .. } => *retryable,
            ProviderError::Timeout { .. }
            | ProviderError::ConnectTimeout { .. }
            | ProviderError::FirstTokenTimeout { .. }
            | ProviderError::Unavailable { .. }
            | ProviderError::RateLimited { .. } => true,
            ProviderError::Cancelled
            | ProviderError::Translation(_)
            | ProviderError::ImageUrlNotSupported { .. }
            | ProviderError::UnsupportedField { .. }
            | ProviderError::UnsupportedInput { .. } => false,
        }
    }

    /// The upstream `Retry-After` hint, if this error carried one (429).
    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        match self {
            ProviderError::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

/// Which gateway-side per-key quota tripped (distinct stable codes: RPM is
/// `LM-4002`, TPM is `LM-4003` - pinned by the M5 spec).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaKind {
    /// Requests per minute.
    Rpm,
    /// Tokens per minute.
    Tpm,
}

impl std::fmt::Display for QuotaKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            QuotaKind::Rpm => "requests-per-minute",
            QuotaKind::Tpm => "tokens-per-minute",
        })
    }
}

/// Coarse client-facing error category. Serialized as the `type` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorType {
    /// The client's request was rejected (4xx).
    InvalidRequest,
    /// An upstream provider failed (502/503/504).
    UpstreamError,
    /// The gateway itself malfunctioned (500).
    Internal,
    /// The client disconnected before the request completed (issue #11).
    /// Kept distinct from `Internal` so a client hanging up never inflates
    /// the internal-error metrics/alerts a gateway malfunction would.
    ClientCancelled,
}

/// An error the gateway returns to the client.
#[derive(Debug, Clone, thiserror::Error)]
pub enum GatewayError {
    // ---- Request errors (LM-1xxx) -------------------------------------------
    /// Malformed or invalid request body / parameters.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// The request body exceeded the configured size limit.
    #[error("payload too large (limit {limit} bytes)")]
    PayloadTooLarge { limit: usize },

    // ---- Routing errors (LM-2xxx) -------------------------------------------
    /// No model matched the requested id.
    #[error("model '{0}' not found")]
    ModelNotFound(String),

    /// The model exists but does not serve the requested capability.
    #[error("model '{model}' does not support capability '{capability}'")]
    UnsupportedCapability {
        model: String,
        capability: Capability,
    },

    /// A rerank request supplied no documents to score.
    #[error("`documents` must not be empty")]
    EmptyDocuments,

    /// An image content part was sent to a model whose declared `modalities`
    /// do not include `"image"`. Rejected before any upstream call. Shared by
    /// chat vision (M8) and multimodal embeddings (M9).
    #[error("model '{model}' does not accept image input")]
    ImageInputNotSupported { model: String },

    /// The resolved provider only accepts inline base64 image data; a remote
    /// image URL was supplied (Gemini). The gateway never fetches the URL.
    #[error("provider '{provider}' requires inline base64 image data; remote image URLs are not supported")]
    ImageUrlNotSupported { provider: String },

    /// A provider-native image source (Anthropic `file_id`, Gemini `fileUri`
    /// / GCS URI) was supplied, but the resolved primary provider is not the
    /// one that reference belongs to. Rejected before any upstream call: an
    /// honest client error rather than the 502 a translation failure would
    /// otherwise produce (`source` names the reference kind, e.g.
    /// `"anthropic-file"` or `"gemini-file"`).
    #[error(
        "provider '{provider}' does not support the '{source_kind}' provider-native image source"
    )]
    ImageSourceNotSupported {
        provider: String,
        source_kind: &'static str,
    },

    /// A remote image URL was supplied to `/v1/embeddings` but server-side image
    /// fetching is disabled (M9). The operator must enable `[image_fetch]` or
    /// the client must inline the image as a `data:` URI.
    #[error("remote image fetching is disabled; inline the image as a data: URI")]
    ImageFetchDisabled,

    /// A remote image URL was rejected by a fetch guard (scheme, host/prefix
    /// allowlist, private-IP block, size cap, non-image content type, or the
    /// per-request image count cap) (M9). The reason may be logged server-side
    /// at `debug`, but is never returned: it must not leak internal network
    /// topology.
    #[error("image URL rejected by fetch policy")]
    ImageUrlRejected,

    /// A permitted image fetch failed at the remote host (network error,
    /// timeout, or error status) (M9). The remote host's fault, so 502.
    #[error("failed to fetch a remote image")]
    ImageFetchFailed,

    // ---- Upstream errors (LM-3xxx) ------------------------------------------
    /// An upstream provider rate limited the request (HTTP 429).
    #[error("upstream provider '{provider}' rate limited the request")]
    UpstreamRateLimited {
        provider: String,
        retry_after: Option<Duration>,
    },

    /// An upstream provider returned a response the gateway could not parse
    /// (malformed / schema mismatch). The upstream's fault, so 502 - never 500.
    #[error("upstream provider '{provider}' returned an unparseable response")]
    UpstreamInvalidResponse { provider: String },

    /// An upstream provider returned an error status.
    #[error("upstream provider '{provider}' returned an error (HTTP {status})")]
    Upstream { provider: String, status: u16 },

    /// No healthy upstream was available (circuit open / all fallbacks failed).
    #[error("upstream provider '{provider}' is unavailable")]
    UpstreamUnavailable { provider: String },

    /// An upstream provider timed out.
    #[error("upstream provider '{provider}' timed out")]
    UpstreamTimeout { provider: String },

    /// An upstream stream ended without its terminator (e.g. no `[DONE]`),
    /// so the response is truncated. The upstream's fault → 502.
    #[error("upstream provider '{provider}' stream ended prematurely")]
    UpstreamStreamInterrupted { provider: String },

    /// An upstream produced no first token within the configured deadline.
    #[error("upstream provider '{provider}' produced no first token in time")]
    UpstreamFirstTokenTimeout { provider: String },

    /// The connection to an upstream could not be established within the
    /// connect timeout (M6 §6.4). Distinct from a read timeout for debugging.
    #[error("upstream provider '{provider}' connection timed out")]
    UpstreamConnectTimeout { provider: String },

    /// The whole request (all retries and fallbacks) exceeded the configured
    /// total timeout (M6 §6.4). Names the provider tried last.
    #[error("request to upstream provider '{provider}' exceeded the total timeout")]
    UpstreamTotalTimeout { provider: String },

    /// The circuit breaker for a provider is open and no fallback remained
    /// (M6 §6.3). Advertises the cooldown remainder as `Retry-After`.
    #[error("upstream provider '{provider}' circuit is open")]
    CircuitOpen {
        provider: String,
        /// How long until the breaker will admit a probe again.
        retry_after: Option<Duration>,
    },

    // ---- Auth / budget errors (LM-4xxx, codes pinned by the M5 spec) --------
    /// The virtual key's hard budget is exhausted. Enforced *before* any
    /// upstream call, so a rejected request never leaks spend.
    #[error("budget exceeded for this key")]
    BudgetExceeded,

    /// A gateway-side per-key quota (RPM or TPM) was exceeded.
    #[error("{quota} quota exceeded for this key")]
    QuotaExceeded {
        quota: QuotaKind,
        retry_after: Option<Duration>,
    },

    /// Missing or invalid virtual key. Deliberately does not say *why* the
    /// key is invalid (unknown / disabled / expired) - that would let a
    /// caller probe key state.
    #[error("authentication required")]
    Unauthorized,

    // ---- Internal errors (LM-5xxx) ------------------------------------------
    /// An internal gateway malfunction. The detail is logged, never returned.
    #[error("internal error: {0}")]
    Internal(String),

    // ---- Client-cancellation (LM-6xxx, issue #11) ----------------------------
    /// The client disconnected before the request completed and the upstream
    /// call was aborted. Deliberately its own variant rather than
    /// `Internal`: the gateway didn't malfunction, so this must never count
    /// against, or alert on, internal/5xx error rates.
    #[error("client cancelled the request")]
    ClientCancelled,
}

impl GatewayError {
    /// The stable `LM-XXXX` code for this error. Documented in `docs/errors.md`.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            GatewayError::InvalidRequest(_) => "LM-1001",
            GatewayError::PayloadTooLarge { .. } => "LM-1002",
            GatewayError::ModelNotFound(_) => "LM-2001",
            GatewayError::UnsupportedCapability { .. } => "LM-2002",
            GatewayError::ImageInputNotSupported { .. } => "LM-2003",
            GatewayError::ImageUrlNotSupported { .. } => "LM-2004",
            GatewayError::ImageSourceNotSupported { .. } => "LM-2008",
            GatewayError::ImageFetchDisabled => "LM-2005",
            GatewayError::ImageUrlRejected => "LM-2006",
            GatewayError::ImageFetchFailed => "LM-2007",
            GatewayError::EmptyDocuments => "LM-2010",
            GatewayError::UpstreamRateLimited { .. } => "LM-3001",
            GatewayError::UpstreamInvalidResponse { .. } => "LM-3002",
            GatewayError::Upstream { .. } => "LM-3003",
            GatewayError::UpstreamUnavailable { .. } => "LM-3004",
            GatewayError::UpstreamTimeout { .. } => "LM-3005",
            GatewayError::UpstreamStreamInterrupted { .. } => "LM-3010",
            GatewayError::UpstreamFirstTokenTimeout { .. } => "LM-3011",
            GatewayError::UpstreamConnectTimeout { .. } => "LM-3012",
            GatewayError::UpstreamTotalTimeout { .. } => "LM-3013",
            GatewayError::CircuitOpen { .. } => "LM-3020",
            GatewayError::BudgetExceeded => "LM-4001",
            GatewayError::QuotaExceeded {
                quota: QuotaKind::Rpm,
                ..
            } => "LM-4002",
            GatewayError::QuotaExceeded {
                quota: QuotaKind::Tpm,
                ..
            } => "LM-4003",
            GatewayError::Unauthorized => "LM-4004",
            GatewayError::Internal(_) => "LM-5001",
            GatewayError::ClientCancelled => "LM-6001",
        }
    }

    /// The HTTP status code to return for this error.
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        match self {
            GatewayError::InvalidRequest(_)
            | GatewayError::UnsupportedCapability { .. }
            | GatewayError::EmptyDocuments
            | GatewayError::ImageInputNotSupported { .. }
            | GatewayError::ImageUrlNotSupported { .. }
            | GatewayError::ImageSourceNotSupported { .. }
            | GatewayError::ImageFetchDisabled
            | GatewayError::ImageUrlRejected => 400,
            GatewayError::Unauthorized => 401,
            GatewayError::BudgetExceeded => 402,
            GatewayError::ModelNotFound(_) => 404,
            GatewayError::PayloadTooLarge { .. } => 413,
            GatewayError::QuotaExceeded { .. } | GatewayError::UpstreamRateLimited { .. } => 429,
            // Nonstandard, but the conventional "client closed request" status
            // (nginx 499): the client is normally already gone, so this status
            // exists for logs/metrics, not for anything the client reads. Not
            // a 5xx - never counted against internal-error rates (issue #11).
            GatewayError::ClientCancelled => 499,
            GatewayError::Internal(_) => 500,
            GatewayError::Upstream { .. }
            | GatewayError::UpstreamInvalidResponse { .. }
            | GatewayError::UpstreamStreamInterrupted { .. }
            | GatewayError::ImageFetchFailed => 502,
            GatewayError::UpstreamUnavailable { .. } | GatewayError::CircuitOpen { .. } => 503,
            GatewayError::UpstreamTimeout { .. }
            | GatewayError::UpstreamFirstTokenTimeout { .. }
            | GatewayError::UpstreamConnectTimeout { .. }
            | GatewayError::UpstreamTotalTimeout { .. } => 504,
        }
    }

    /// The coarse category for the `type` field.
    #[must_use]
    pub const fn error_type(&self) -> ErrorType {
        match self {
            GatewayError::InvalidRequest(_)
            | GatewayError::ModelNotFound(_)
            | GatewayError::UnsupportedCapability { .. }
            | GatewayError::ImageInputNotSupported { .. }
            | GatewayError::ImageUrlNotSupported { .. }
            | GatewayError::ImageSourceNotSupported { .. }
            | GatewayError::ImageFetchDisabled
            | GatewayError::ImageUrlRejected
            | GatewayError::EmptyDocuments
            | GatewayError::PayloadTooLarge { .. }
            | GatewayError::Unauthorized
            | GatewayError::BudgetExceeded
            | GatewayError::QuotaExceeded { .. } => ErrorType::InvalidRequest,
            GatewayError::Upstream { .. }
            | GatewayError::ImageFetchFailed
            | GatewayError::UpstreamInvalidResponse { .. }
            | GatewayError::UpstreamUnavailable { .. }
            | GatewayError::UpstreamTimeout { .. }
            | GatewayError::UpstreamStreamInterrupted { .. }
            | GatewayError::UpstreamFirstTokenTimeout { .. }
            | GatewayError::UpstreamConnectTimeout { .. }
            | GatewayError::UpstreamTotalTimeout { .. }
            | GatewayError::CircuitOpen { .. }
            | GatewayError::UpstreamRateLimited { .. } => ErrorType::UpstreamError,
            GatewayError::Internal(_) => ErrorType::Internal,
            GatewayError::ClientCancelled => ErrorType::ClientCancelled,
        }
    }

    /// The `Retry-After` hint (seconds) to advertise, if any.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            GatewayError::QuotaExceeded { retry_after, .. }
            | GatewayError::UpstreamRateLimited { retry_after, .. }
            | GatewayError::CircuitOpen { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// The message returned to the client.
    ///
    /// Internal errors are deliberately opaque so implementation details never
    /// leak; the underlying [`Display`](std::fmt::Display) text is for logs only.
    #[must_use]
    pub fn public_message(&self) -> String {
        match self {
            GatewayError::Internal(_) => "internal error".to_owned(),
            other => other.to_string(),
        }
    }

    /// Build the serializable error envelope returned in the HTTP body.
    #[must_use]
    pub fn to_envelope(&self) -> ErrorEnvelope {
        ErrorEnvelope {
            error: ErrorBody {
                code: self.code(),
                message: self.public_message(),
                error_type: self.error_type(),
            },
        }
    }

    /// Convert a [`ProviderError`] into a client-facing error, attaching the
    /// provider name where the provider error did not already carry it.
    #[must_use]
    pub fn from_provider(provider: &str, err: ProviderError) -> Self {
        match err {
            ProviderError::Upstream {
                provider: p,
                status,
                ..
            } => GatewayError::Upstream {
                provider: p_or(provider, p),
                status,
            },
            ProviderError::Timeout { provider: p } => GatewayError::UpstreamTimeout {
                provider: p_or(provider, p),
            },
            ProviderError::ConnectTimeout { provider: p } => GatewayError::UpstreamConnectTimeout {
                provider: p_or(provider, p),
            },
            ProviderError::FirstTokenTimeout { provider: p } => {
                GatewayError::UpstreamFirstTokenTimeout {
                    provider: p_or(provider, p),
                }
            }
            ProviderError::Unavailable { provider: p } => GatewayError::UpstreamUnavailable {
                provider: p_or(provider, p),
            },
            ProviderError::RateLimited {
                provider: p,
                retry_after,
            } => GatewayError::UpstreamRateLimited {
                provider: p_or(provider, p),
                retry_after,
            },
            // A malformed / unparseable upstream response is the upstream's
            // fault → 502 (LM-3002), never a gateway 500.
            ProviderError::Translation(_) => GatewayError::UpstreamInvalidResponse {
                provider: provider.to_owned(),
            },
            // The resolved link cannot take a remote image URL (LM-2004): a
            // client-input problem, never the provider's fault, even when it
            // surfaces from a fallback link rather than the pre-flight check
            // (GH #13). Never a 502.
            ProviderError::ImageUrlNotSupported { provider: p } => {
                GatewayError::ImageUrlNotSupported {
                    provider: p_or(provider, p),
                }
            }
            // The client set a field the provider cannot honor under strict mode:
            // a client fault → 400 (LM-1001), never retried or failed over.
            ProviderError::UnsupportedField { provider: p, field } => {
                GatewayError::InvalidRequest(format!(
                    "provider '{}' does not support the '{field}' field",
                    p_or(provider, p)
                ))
            }
            // The input shape itself cannot be consumed by this provider (e.g.
            // token-id arrays to a text-only embed API): a client fault → 400
            // (LM-1001), rejected before any upstream call.
            ProviderError::UnsupportedInput {
                provider: p,
                reason,
            } => GatewayError::InvalidRequest(format!(
                "provider '{}' does not support {reason}",
                p_or(provider, p)
            )),
            // A client-initiated cancel is not a gateway malfunction: its own
            // LM-6001 / 499, so it never inflates `internal`/5xx metrics or
            // alerts the way `GatewayError::Internal` would (issue #11).
            ProviderError::Cancelled => GatewayError::ClientCancelled,
        }
    }
}

/// Prefer a non-empty embedded provider name, else fall back to the router's.
fn p_or(fallback: &str, embedded: String) -> String {
    if embedded.is_empty() {
        fallback.to_owned()
    } else {
        embedded
    }
}

impl From<ProviderError> for GatewayError {
    fn from(err: ProviderError) -> Self {
        GatewayError::from_provider("upstream", err)
    }
}

/// The `{ "error": { ... } }` wrapper of an error response body.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

/// The error object itself.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    /// Stable `LM-XXXX` code.
    pub code: &'static str,
    /// Human-readable, secret-free message.
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: ErrorType,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // One flat table pinning every stable LM-XXXX code; splitting it would
    // only scatter the mapping this test exists to keep in one place.
    #[allow(clippy::too_many_lines)]
    fn error_codes_are_stable() {
        assert_eq!(GatewayError::InvalidRequest("x".into()).code(), "LM-1001");
        assert_eq!(GatewayError::PayloadTooLarge { limit: 1 }.code(), "LM-1002");
        // Routing errors (pinned by the M2 spec).
        assert_eq!(GatewayError::ModelNotFound("x".into()).code(), "LM-2001");
        assert_eq!(
            GatewayError::UnsupportedCapability {
                model: "m".into(),
                capability: Capability::Embed
            }
            .code(),
            "LM-2002"
        );
        // Upstream errors (LM-3001 / LM-3002 pinned by the M2 spec).
        assert_eq!(
            GatewayError::UpstreamRateLimited {
                provider: "openai".into(),
                retry_after: None
            }
            .code(),
            "LM-3001"
        );
        assert_eq!(
            GatewayError::UpstreamInvalidResponse {
                provider: "openai".into()
            }
            .code(),
            "LM-3002"
        );
        assert_eq!(
            GatewayError::Upstream {
                provider: "openai".into(),
                status: 500
            }
            .code(),
            "LM-3003"
        );
        // Empty rerank documents (pinned by the M3 spec).
        assert_eq!(GatewayError::EmptyDocuments.code(), "LM-2010");
        // Vision (M8) + multimodal-embeddings image-fetch (M9) codes.
        assert_eq!(
            GatewayError::ImageInputNotSupported {
                model: "gpt".into()
            }
            .code(),
            "LM-2003"
        );
        assert_eq!(
            GatewayError::ImageUrlNotSupported {
                provider: "google".into()
            }
            .code(),
            "LM-2004"
        );
        assert_eq!(GatewayError::ImageFetchDisabled.code(), "LM-2005");
        assert_eq!(GatewayError::ImageUrlRejected.code(), "LM-2006");
        assert_eq!(GatewayError::ImageFetchFailed.code(), "LM-2007");
        assert_eq!(GatewayError::ImageUrlRejected.http_status(), 400);
        assert_eq!(GatewayError::ImageFetchFailed.http_status(), 502);
        // Provider-native image source misrouted to the wrong provider (issue #12).
        let mismatch = GatewayError::ImageSourceNotSupported {
            provider: "openai".into(),
            source_kind: "anthropic-file",
        };
        assert_eq!(mismatch.code(), "LM-2008");
        assert_eq!(mismatch.http_status(), 400);
        assert_eq!(mismatch.error_type(), ErrorType::InvalidRequest);
        // Streaming upstream faults (M4).
        assert_eq!(
            GatewayError::UpstreamStreamInterrupted {
                provider: "openai".into()
            }
            .code(),
            "LM-3010"
        );
        assert_eq!(
            GatewayError::UpstreamFirstTokenTimeout {
                provider: "openai".into()
            }
            .code(),
            "LM-3011"
        );
        // Auth / budget codes (pinned by the M5 spec).
        assert_eq!(GatewayError::BudgetExceeded.code(), "LM-4001");
        assert_eq!(
            GatewayError::QuotaExceeded {
                quota: QuotaKind::Rpm,
                retry_after: None
            }
            .code(),
            "LM-4002"
        );
        assert_eq!(
            GatewayError::QuotaExceeded {
                quota: QuotaKind::Tpm,
                retry_after: None
            }
            .code(),
            "LM-4003"
        );
        assert_eq!(GatewayError::Unauthorized.code(), "LM-4004");
        assert_eq!(GatewayError::Internal("boom".into()).code(), "LM-5001");
        // Resilience codes (M6).
        assert_eq!(
            GatewayError::UpstreamConnectTimeout {
                provider: "openai".into()
            }
            .code(),
            "LM-3012"
        );
        assert_eq!(
            GatewayError::UpstreamTotalTimeout {
                provider: "openai".into()
            }
            .code(),
            "LM-3013"
        );
        assert_eq!(
            GatewayError::CircuitOpen {
                provider: "openai".into(),
                retry_after: None
            }
            .code(),
            "LM-3020"
        );
        // Client-cancellation (issue #11): its own prefix, never LM-5001.
        assert_eq!(GatewayError::ClientCancelled.code(), "LM-6001");
    }

    #[test]
    fn resilience_statuses_and_types_follow_the_taxonomy() {
        // Connect and total timeouts are 504 upstream errors, never a 500/401.
        for err in [
            GatewayError::UpstreamConnectTimeout {
                provider: "p".into(),
            },
            GatewayError::UpstreamTotalTimeout {
                provider: "p".into(),
            },
        ] {
            assert_eq!(err.http_status(), 504);
            assert_eq!(err.error_type(), ErrorType::UpstreamError);
        }
        // An open circuit is a 503 that advertises when to retry.
        let open = GatewayError::CircuitOpen {
            provider: "p".into(),
            retry_after: Some(Duration::from_secs(30)),
        };
        assert_eq!(open.http_status(), 503);
        assert_eq!(open.error_type(), ErrorType::UpstreamError);
        assert_eq!(open.retry_after(), Some(Duration::from_secs(30)));
    }

    #[test]
    fn connect_timeout_maps_to_fg_3012_and_names_provider() {
        let ge = GatewayError::from_provider(
            "openai",
            ProviderError::ConnectTimeout {
                provider: String::new(),
            },
        );
        assert_eq!(ge.code(), "LM-3012");
        assert_eq!(ge.http_status(), 504);
        match ge {
            GatewayError::UpstreamConnectTimeout { provider } => assert_eq!(&provider, "openai"),
            other => panic!("expected UpstreamConnectTimeout, got {other:?}"),
        }
    }

    #[test]
    fn provider_error_retry_classification() {
        // Retryable and provider-fault: 5xx, timeouts, unreachable, 429.
        for err in [
            ProviderError::Upstream {
                provider: "p".into(),
                status: 503,
                retryable: true,
            },
            ProviderError::Timeout {
                provider: "p".into(),
            },
            ProviderError::ConnectTimeout {
                provider: "p".into(),
            },
            ProviderError::FirstTokenTimeout {
                provider: "p".into(),
            },
            ProviderError::Unavailable {
                provider: "p".into(),
            },
            ProviderError::RateLimited {
                provider: "p".into(),
                retry_after: None,
            },
        ] {
            assert!(err.is_retryable(), "{err:?} should be retryable");
            assert!(err.is_provider_fault(), "{err:?} should fault the breaker");
        }
        // Never retryable, never a provider-health signal.
        for err in [
            ProviderError::Upstream {
                provider: "p".into(),
                status: 400,
                retryable: false,
            },
            ProviderError::Translation("bad json".into()),
            ProviderError::ImageUrlNotSupported {
                provider: "p".into(),
            },
            ProviderError::Cancelled,
            ProviderError::UnsupportedField {
                provider: "p".into(),
                field: "dimensions".into(),
            },
            ProviderError::UnsupportedInput {
                provider: "p".into(),
                reason: "pre-tokenized input".into(),
            },
        ] {
            assert!(!err.is_retryable(), "{err:?} must not be retried");
            assert!(
                !err.is_provider_fault(),
                "{err:?} must not fault the breaker"
            );
        }
    }

    #[test]
    fn unsupported_field_maps_to_400_lm_1001_naming_the_field() {
        let err = GatewayError::from_provider(
            "ollama",
            ProviderError::UnsupportedField {
                provider: "ollama".into(),
                field: "dimensions".into(),
            },
        );
        assert_eq!(err.code(), "LM-1001");
        assert_eq!(err.error_type(), ErrorType::InvalidRequest);
        assert!(matches!(err, GatewayError::InvalidRequest(ref m) if m.contains("dimensions")));
    }

    #[test]
    fn unsupported_input_maps_to_400_lm_1001_naming_the_reason() {
        let err = GatewayError::from_provider(
            "cohere",
            ProviderError::UnsupportedInput {
                provider: "cohere".into(),
                reason: "pre-tokenized input (token id arrays)".into(),
            },
        );
        assert_eq!(err.code(), "LM-1001");
        assert_eq!(err.error_type(), ErrorType::InvalidRequest);
        assert!(
            matches!(err, GatewayError::InvalidRequest(ref m) if m.contains("pre-tokenized") && m.contains("cohere"))
        );
    }

    #[test]
    fn provider_rate_limit_retry_after_is_exposed() {
        let err = ProviderError::RateLimited {
            provider: "p".into(),
            retry_after: Some(Duration::from_secs(3)),
        };
        assert_eq!(err.retry_after(), Some(Duration::from_secs(3)));
        assert_eq!(
            ProviderError::Timeout {
                provider: "p".into()
            }
            .retry_after(),
            None
        );
    }

    #[test]
    fn empty_documents_is_a_400_client_error() {
        let err = GatewayError::EmptyDocuments;
        assert_eq!(err.http_status(), 400);
        assert_eq!(err.error_type(), ErrorType::InvalidRequest);
        assert_eq!(err.public_message(), "`documents` must not be empty");
    }

    #[test]
    fn status_and_type_are_consistent_with_taxonomy() {
        let client = GatewayError::InvalidRequest("bad".into());
        assert_eq!(client.http_status(), 400);
        assert_eq!(client.error_type(), ErrorType::InvalidRequest);

        let upstream = GatewayError::Upstream {
            provider: "openai".into(),
            status: 500,
        };
        assert_eq!(upstream.http_status(), 502);
        assert_eq!(upstream.error_type(), ErrorType::UpstreamError);

        let internal = GatewayError::Internal("db".into());
        assert_eq!(internal.http_status(), 500);
        assert_eq!(internal.error_type(), ErrorType::Internal);
    }

    #[test]
    fn envelope_json_matches_public_schema() {
        let err = GatewayError::ModelNotFound("gpt-9".into());
        let json = serde_json::to_value(err.to_envelope()).unwrap();
        assert_eq!(json["error"]["code"], "LM-2001");
        assert_eq!(json["error"]["type"], "invalid_request");
        assert_eq!(json["error"]["message"], "model 'gpt-9' not found");
    }

    #[test]
    fn internal_errors_do_not_leak_details_to_client() {
        let err = GatewayError::Internal("connection to sqlite at /var/secret failed".into());
        // Display (for logs) keeps the detail...
        assert!(err.to_string().contains("/var/secret"));
        // ...but the client-facing message is opaque.
        assert_eq!(err.public_message(), "internal error");
        let json = serde_json::to_value(err.to_envelope()).unwrap();
        assert_eq!(json["error"]["message"], "internal error");
    }

    #[test]
    fn provider_error_maps_to_upstream_and_names_provider() {
        let pe = ProviderError::Upstream {
            provider: "cohere".into(),
            status: 503,
            retryable: true,
        };
        let ge = GatewayError::from_provider("cohere", pe);
        assert_eq!(ge.error_type(), ErrorType::UpstreamError);
        match &ge {
            GatewayError::Upstream { provider, status } => {
                assert_eq!(provider, "cohere");
                assert_eq!(*status, 503);
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn provider_timeout_becomes_gateway_timeout_never_401() {
        let ge = GatewayError::from_provider(
            "openai",
            ProviderError::Timeout {
                provider: String::new(),
            },
        );
        assert_eq!(ge.http_status(), 504);
        assert_ne!(ge.http_status(), 401);
        match ge {
            GatewayError::UpstreamTimeout { provider } => assert_eq!(&provider, "openai"),
            other => panic!("expected UpstreamTimeout, got {other:?}"),
        }
    }

    #[test]
    fn malformed_upstream_response_is_502_never_500() {
        // A translation/parse failure is the upstream's fault, not ours.
        let ge = GatewayError::from_provider(
            "openai",
            ProviderError::Translation("unexpected end of JSON".into()),
        );
        assert_eq!(ge.code(), "LM-3002");
        assert_eq!(ge.http_status(), 502);
        assert_ne!(ge.http_status(), 500);
        assert_eq!(ge.error_type(), ErrorType::UpstreamError);
    }

    #[test]
    fn provider_image_url_not_supported_becomes_lm_2004_not_502() {
        // GH #13: when this failure surfaces from a fallback link deep in the
        // chain (not the LM-2004 pre-flight, which only inspects the primary),
        // it must still be the honest 4xx - never the generic 502 that a plain
        // `ProviderError::Translation` would produce.
        let ge = GatewayError::from_provider(
            "primary-router-name",
            ProviderError::ImageUrlNotSupported {
                provider: "gemini-fallback".into(),
            },
        );
        assert_eq!(ge.code(), "LM-2004");
        assert_eq!(ge.http_status(), 400);
        assert_eq!(ge.error_type(), ErrorType::InvalidRequest);
        match ge {
            GatewayError::ImageUrlNotSupported { provider } => {
                // The provider embedded in the error (the fallback that was
                // actually attempted) wins over the router-supplied name.
                assert_eq!(provider, "gemini-fallback");
            }
            other => panic!("expected ImageUrlNotSupported, got {other:?}"),
        }
    }

    #[test]
    fn rate_limited_carries_retry_after() {
        let ge = GatewayError::UpstreamRateLimited {
            provider: "openai".into(),
            retry_after: Some(Duration::from_secs(3)),
        };
        assert_eq!(ge.http_status(), 429);
        assert_eq!(ge.retry_after(), Some(Duration::from_secs(3)));
    }

    // Issue #11: a client-initiated cancel must not inflate `internal`
    // metrics/alerts (previously mapped to `GatewayError::Internal`, 500).
    #[test]
    fn cancelled_maps_to_a_dedicated_non_5xx_variant_not_internal() {
        let ge = GatewayError::from_provider("openai", ProviderError::Cancelled);
        match ge {
            GatewayError::ClientCancelled => {}
            other => panic!("expected ClientCancelled, got {other:?}"),
        }
    }

    #[test]
    fn client_cancelled_is_not_a_5xx_and_has_a_distinct_error_type() {
        let err = GatewayError::ClientCancelled;
        assert_eq!(err.code(), "LM-6001");
        assert_eq!(err.http_status(), 499);
        assert!(
            err.http_status() < 500,
            "client cancellation must not surface as a 5xx: {}",
            err.http_status()
        );
        assert_eq!(err.error_type(), ErrorType::ClientCancelled);
        assert_ne!(err.error_type(), ErrorType::Internal);
    }

    #[test]
    fn client_cancelled_envelope_carries_its_own_type() {
        let json = serde_json::to_value(GatewayError::ClientCancelled.to_envelope()).unwrap();
        assert_eq!(json["error"]["code"], "LM-6001");
        assert_eq!(json["error"]["type"], "client_cancelled");
    }
}
