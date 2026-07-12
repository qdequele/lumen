//! Error taxonomy.
//!
//! Two layers:
//!
//! * [`ProviderError`] — what a provider returns. Never contains secrets.
//! * [`GatewayError`] — what the gateway returns to the client. Carries a
//!   stable `FG-XXXX` code (documented in `docs/errors.md`), an HTTP status,
//!   and a coarse [`ErrorType`].
//!
//! The taxonomy always distinguishes three situations (see `CLAUDE.md` rule 8):
//! a client error (4xx / `invalid_request`), an upstream provider error
//! (502/503/504 / `upstream_error`, always naming the provider), and an
//! internal gateway error (500 / `internal`). A gateway malfunction must never
//! be reported as a misleading 401 (lesson: OpenRouter outages).

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

    /// The upstream did not respond within the configured timeout.
    #[error("provider '{provider}' timed out")]
    Timeout { provider: String },

    /// The downstream client disconnected; the upstream call was aborted.
    #[error("request cancelled")]
    Cancelled,

    /// A request or response could not be translated to/from the upstream schema.
    #[error("translation error: {0}")]
    Translation(String),

    /// The upstream signalled rate limiting (HTTP 429).
    #[error("provider '{provider}' rate limited the request")]
    RateLimited {
        provider: String,
        /// The upstream `Retry-After`, if provided.
        retry_after: Option<Duration>,
    },
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
}

/// An error the gateway returns to the client.
#[derive(Debug, Clone, thiserror::Error)]
pub enum GatewayError {
    // ---- Client errors (FG-1xxx) --------------------------------------------
    /// Malformed or invalid request body / parameters.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// No model matched the requested id.
    #[error("model '{0}' not found")]
    ModelNotFound(String),

    /// The model exists but does not serve the requested capability.
    #[error("model '{model}' does not support capability '{capability}'")]
    UnsupportedCapability {
        model: String,
        capability: Capability,
    },

    /// The request body exceeded the configured size limit.
    #[error("payload too large (limit {limit} bytes)")]
    PayloadTooLarge { limit: usize },

    /// Missing or invalid virtual key.
    #[error("authentication required")]
    Unauthorized,

    /// The virtual key's hard budget is exhausted.
    #[error("budget exceeded for this key")]
    BudgetExceeded,

    /// A gateway-side quota (RPM/TPM) was exceeded.
    #[error("rate limit exceeded")]
    RateLimited { retry_after: Option<Duration> },

    // ---- Upstream errors (FG-2xxx) ------------------------------------------
    /// An upstream provider returned an error status.
    #[error("upstream provider '{provider}' returned an error (HTTP {status})")]
    Upstream { provider: String, status: u16 },

    /// No healthy upstream was available (circuit open / all fallbacks failed).
    #[error("upstream provider '{provider}' is unavailable")]
    UpstreamUnavailable { provider: String },

    /// An upstream provider timed out.
    #[error("upstream provider '{provider}' timed out")]
    UpstreamTimeout { provider: String },

    /// An upstream provider rate limited the request.
    #[error("upstream provider '{provider}' rate limited the request")]
    UpstreamRateLimited {
        provider: String,
        retry_after: Option<Duration>,
    },

    // ---- Internal errors (FG-5xxx) ------------------------------------------
    /// An internal gateway malfunction. The detail is logged, never returned.
    #[error("internal error: {0}")]
    Internal(String),
}

impl GatewayError {
    /// The stable `FG-XXXX` code for this error. Documented in `docs/errors.md`.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            GatewayError::InvalidRequest(_) => "FG-1001",
            GatewayError::ModelNotFound(_) => "FG-1002",
            GatewayError::UnsupportedCapability { .. } => "FG-1003",
            GatewayError::PayloadTooLarge { .. } => "FG-1004",
            GatewayError::Unauthorized => "FG-1005",
            GatewayError::BudgetExceeded => "FG-1006",
            GatewayError::RateLimited { .. } => "FG-1007",
            GatewayError::Upstream { .. } => "FG-2001",
            GatewayError::UpstreamUnavailable { .. } => "FG-2002",
            GatewayError::UpstreamTimeout { .. } => "FG-2003",
            GatewayError::UpstreamRateLimited { .. } => "FG-2004",
            GatewayError::Internal(_) => "FG-5001",
        }
    }

    /// The HTTP status code to return for this error.
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        match self {
            GatewayError::InvalidRequest(_) | GatewayError::UnsupportedCapability { .. } => 400,
            GatewayError::Unauthorized => 401,
            GatewayError::BudgetExceeded => 402,
            GatewayError::ModelNotFound(_) => 404,
            GatewayError::PayloadTooLarge { .. } => 413,
            GatewayError::RateLimited { .. } | GatewayError::UpstreamRateLimited { .. } => 429,
            GatewayError::Internal(_) => 500,
            GatewayError::Upstream { .. } => 502,
            GatewayError::UpstreamUnavailable { .. } => 503,
            GatewayError::UpstreamTimeout { .. } => 504,
        }
    }

    /// The coarse category for the `type` field.
    #[must_use]
    pub const fn error_type(&self) -> ErrorType {
        match self {
            GatewayError::InvalidRequest(_)
            | GatewayError::ModelNotFound(_)
            | GatewayError::UnsupportedCapability { .. }
            | GatewayError::PayloadTooLarge { .. }
            | GatewayError::Unauthorized
            | GatewayError::BudgetExceeded
            | GatewayError::RateLimited { .. } => ErrorType::InvalidRequest,
            GatewayError::Upstream { .. }
            | GatewayError::UpstreamUnavailable { .. }
            | GatewayError::UpstreamTimeout { .. }
            | GatewayError::UpstreamRateLimited { .. } => ErrorType::UpstreamError,
            GatewayError::Internal(_) => ErrorType::Internal,
        }
    }

    /// The `Retry-After` hint (seconds) to advertise, if any.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            GatewayError::RateLimited { retry_after }
            | GatewayError::UpstreamRateLimited { retry_after, .. } => *retry_after,
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
            ProviderError::RateLimited {
                provider: p,
                retry_after,
            } => GatewayError::UpstreamRateLimited {
                provider: p_or(provider, p),
                retry_after,
            },
            // A malformed upstream response is the upstream's fault → 503.
            ProviderError::Translation(_) => GatewayError::UpstreamUnavailable {
                provider: provider.to_owned(),
            },
            // Cancellation is normally handled before a body is produced; if it
            // does surface, treat it as an internal condition rather than a
            // misleading client/upstream error.
            ProviderError::Cancelled => GatewayError::Internal("request cancelled".to_owned()),
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
    /// Stable `FG-XXXX` code.
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
    fn error_codes_are_stable() {
        assert_eq!(GatewayError::InvalidRequest("x".into()).code(), "FG-1001");
        assert_eq!(GatewayError::ModelNotFound("x".into()).code(), "FG-1002");
        assert_eq!(GatewayError::Unauthorized.code(), "FG-1005");
        assert_eq!(
            GatewayError::Upstream {
                provider: "openai".into(),
                status: 500
            }
            .code(),
            "FG-2001"
        );
        assert_eq!(GatewayError::Internal("boom".into()).code(), "FG-5001");
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
        assert_eq!(json["error"]["code"], "FG-1002");
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
    fn rate_limited_carries_retry_after() {
        let ge = GatewayError::UpstreamRateLimited {
            provider: "openai".into(),
            retry_after: Some(Duration::from_secs(3)),
        };
        assert_eq!(ge.http_status(), 429);
        assert_eq!(ge.retry_after(), Some(Duration::from_secs(3)));
    }
}
