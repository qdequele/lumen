//! Assembly of the axum application and its middleware stack.

use crate::{embeddings, routes, state::AppState};
use axum::{
    routing::{get, post},
    Router,
};
use tower::ServiceBuilder;
use tower_http::{
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};
use tracing::info_span;

/// Build the full application router with its middleware stack.
///
/// Middleware, outermost first:
/// 1. assign an `x-request-id` (uuid) if the client didn't send one;
/// 2. open a tracing span per request — carrying method, path and request id,
///    but never the body or query string (user data is never logged);
/// 3. propagate the request id onto the response;
/// 4. reject bodies larger than `body_limit` bytes.
pub fn build_app(state: AppState, body_limit: usize) -> Router {
    let middleware = ServiceBuilder::new()
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(TraceLayer::new_for_http().make_span_with(make_request_span))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(RequestBodyLimitLayer::new(body_limit));

    Router::new()
        .route("/health", get(routes::health))
        .route("/metrics", get(routes::metrics))
        .route("/v1/embeddings", post(embeddings::embeddings))
        .with_state(state)
        .layer(middleware)
}

/// Build the per-request tracing span. Only metadata — never the body or query.
fn make_request_span<B>(request: &axum::http::Request<B>) -> tracing::Span {
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    info_span!(
        "request",
        method = %request.method(),
        // `.path()` deliberately excludes the query string: user data never
        // appears in logs.
        path = %request.uri().path(),
        request_id = %request_id,
    )
}
