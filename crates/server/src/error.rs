//! Bridging [`GatewayError`] to an axum HTTP response.
//!
//! `GatewayError` lives in `core` and is deliberately web-framework-agnostic;
//! this newtype adds the axum [`IntoResponse`] impl (status, JSON envelope and
//! `Retry-After` header) here in the server layer.

use axum::extract::rejection::{BytesRejection, JsonRejection};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use lumen_core::GatewayError;

/// A [`GatewayError`] that can be returned directly from a handler.
pub struct ApiError(pub GatewayError);

impl From<GatewayError> for ApiError {
    fn from(err: GatewayError) -> Self {
        ApiError(err)
    }
}

/// A body-extractor rejection ([`JsonRejection`] or [`BytesRejection`]): its
/// HTTP status and its plain-text description.
pub trait BodyRejection {
    /// The status axum would answer with.
    fn status(&self) -> StatusCode;
    /// The description of what was wrong with the body.
    fn body_text(&self) -> String;
}

impl BodyRejection for JsonRejection {
    fn status(&self) -> StatusCode {
        JsonRejection::status(self)
    }
    fn body_text(&self) -> String {
        JsonRejection::body_text(self)
    }
}

impl BodyRejection for BytesRejection {
    fn status(&self) -> StatusCode {
        BytesRejection::status(self)
    }
    fn body_text(&self) -> String {
        BytesRejection::body_text(self)
    }
}

/// Map a body-extractor rejection to the gateway's error envelope.
///
/// A `413` means the body ran over `body_limit` while it was being read (a
/// chunked body with no `Content-Length`, which the body-limit layer cannot
/// refuse up front): `LM-1002`, quoting the configured limit. Anything else
/// (malformed JSON, wrong content type, a broken body) is `LM-1001`.
#[must_use]
pub fn body_rejection(rejection: &impl BodyRejection, body_limit: usize) -> GatewayError {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        GatewayError::PayloadTooLarge { limit: body_limit }
    } else {
        GatewayError::InvalidRequest(rejection.body_text())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let err = self.0;
        let status =
            StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        // Server-side and upstream faults are logged (with their safe Display
        // detail); client rejections are only logged at debug. The client body
        // never carries internal detail (see `GatewayError::public_message`).
        if status.is_server_error() {
            tracing::error!(code = err.code(), error = %err, "request failed");
        } else {
            tracing::debug!(
                code = err.code(),
                status = status.as_u16(),
                "request rejected"
            );
        }

        let mut response = (status, Json(err.to_envelope())).into_response();

        if let Some(retry) = err.retry_after() {
            if let Ok(value) = HeaderValue::from_str(&retry.as_secs().to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }

        response
    }
}
