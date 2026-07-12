//! Bridging [`GatewayError`] to an axum HTTP response.
//!
//! `GatewayError` lives in `core` and is deliberately web-framework-agnostic;
//! this newtype adds the axum [`IntoResponse`] impl (status, JSON envelope and
//! `Retry-After` header) here in the server layer.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use ferrogate_core::GatewayError;

/// A [`GatewayError`] that can be returned directly from a handler.
pub struct ApiError(pub GatewayError);

impl From<GatewayError> for ApiError {
    fn from(err: GatewayError) -> Self {
        ApiError(err)
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
