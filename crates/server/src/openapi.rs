//! The gateway's own HTTP contract (`docs/openapi.yaml`), served as JSON at
//! `GET /openapi.json` so a control plane can vendor it. Master-key gated
//! like the rest of the admin surface; the YAML is compiled in and parsed
//! once.

use crate::error::ApiError;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use lumen_core::GatewayError;
use std::sync::LazyLock;

/// `docs/openapi.yaml`, the source of truth, compiled in.
pub const OPENAPI_YAML: &str = include_str!("../../../docs/openapi.yaml");

/// The document as compact JSON, or why it could not be parsed (a build
/// defect: the tests below and `app::routes_match_openapi` fail first).
/// Served as a `&'static str` body, so a request copies nothing.
static OPENAPI_JSON: LazyLock<Result<String, String>> = LazyLock::new(|| {
    let value: serde_json::Value =
        serde_yaml_ng::from_str(OPENAPI_YAML).map_err(|e| e.to_string())?;
    serde_json::to_string(&value).map_err(|e| e.to_string())
});

/// `GET /openapi.json`.
///
/// # Errors
/// `LM-5001` if the compiled-in YAML does not parse (never in a tested build).
#[allow(clippy::unused_async)] // axum handlers are async; this one just reads a static.
pub async fn openapi_json() -> Result<Response, ApiError> {
    match &*OPENAPI_JSON {
        Ok(json) => {
            Ok(([(header::CONTENT_TYPE, "application/json")], json.as_str()).into_response())
        }
        Err(error) => {
            tracing::error!(%error, "docs/openapi.yaml does not parse");
            Err(GatewayError::Internal("the OpenAPI document is unavailable".to_owned()).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The error branch of [`openapi_json`] is unreachable in a build that
    /// passes this test.
    #[test]
    fn the_embedded_document_parses_to_json() {
        let json = OPENAPI_JSON.as_ref().unwrap();
        let doc: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(doc["openapi"], "3.1.0");
        assert_eq!(doc["info"]["version"], env!("CARGO_PKG_VERSION"));
    }
}
