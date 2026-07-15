//! `GatewayError` → HTTP response bridge tests, focused on the
//! client-cancellation variant (issue #11).
//!
//! A client-initiated cancel (`ProviderError::Cancelled`) used to map to
//! `GatewayError::Internal`, surfacing as a 500 and inflating the same
//! `internal`/5xx metrics and alerts as a genuine gateway malfunction. It now
//! gets its own `LM-6001` code, a non-5xx status, and a distinct `type` in the
//! envelope, so it is never counted or alerted on as an internal error.

use axum::response::IntoResponse;
use lumen_core::GatewayError;
use lumen_server::error::ApiError;

#[test]
fn client_cancelled_is_not_a_5xx() {
    let response = ApiError::from(GatewayError::ClientCancelled).into_response();
    assert_eq!(response.status().as_u16(), 499);
    assert!(
        !response.status().is_server_error(),
        "a client cancel must not classify as a server error (status {})",
        response.status()
    );
}

#[tokio::test]
async fn client_cancelled_envelope_carries_lm_6001_and_its_own_type() {
    let response = ApiError::from(GatewayError::ClientCancelled).into_response();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON envelope");
    assert_eq!(json["error"]["code"], "LM-6001");
    assert_eq!(json["error"]["type"], "client_cancelled");
}
