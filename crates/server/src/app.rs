//! Assembly of the axum application and its middleware stack.

use crate::{admin, auth, chat, embeddings, health, models, rerank, routes, state::AppState};
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{
    middleware::{self, Next},
    routing::{get, patch, post, put},
    Router,
};
use lumen_core::GatewayError;
use tower::ServiceBuilder;
use tower_http::{
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};
use tracing::info_span;

use crate::error::ApiError;

/// Build the full application router with its middleware stack.
///
/// Middleware, outermost first:
/// 1. assign an `x-request-id` (uuid) if the client didn't send one;
/// 2. open a tracing span per request — carrying method, path and request id,
///    but never the body or query string (user data is never logged);
/// 3. propagate the request id onto the response;
/// 4. rewrite a bare `413` from the body-limit layer below into the `LM-1002`
///    envelope;
/// 5. reject bodies larger than `body_limit` bytes.
///
/// Route groups:
/// * `/health`, `/health/providers`, `/metrics` — operational, never
///   authenticated, no I/O (`/health` never depends on provider state);
/// * `/v1/*` — the API surface; virtual-key auth when enabled (M5);
/// * `/admin/*` — key management; mounted only when auth is enabled,
///   protected by the master key.
///
/// The body-size limit is read from `state.body_limit` — the single source of
/// truth also surfaced in the `LM-1002` message — rather than a second
/// parameter, so the enforced limit and the advertised one can never diverge.
pub fn build_app(state: AppState) -> Router {
    let body_limit = state.body_limit;
    let middleware_stack = ServiceBuilder::new()
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(TraceLayer::new_for_http().make_span_with(make_request_span))
        .layer(PropagateRequestIdLayer::x_request_id())
        // Conservative default security headers on every response (M7 §7.4).
        .layer(middleware::from_fn(security_headers))
        // `RequestBodyLimitLayer` short-circuits an over-limit body with a bare
        // `413 Payload Too Large` plain-text response *before* axum routing or
        // any handler runs (verified empirically: it fires on `Content-Length`
        // alone, so the chat handler's `JsonRejection` branch never sees it).
        // This middleware sits just outside that layer to rewrite the bare 413
        // into our `LM-1002` JSON envelope.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            map_body_limit_response,
        ))
        .layer(RequestBodyLimitLayer::new(body_limit));

    let api = Router::new()
        .route("/v1/models", get(models::models))
        .route("/v1/chat/completions", post(chat::chat))
        .route("/v1/embeddings", post(embeddings::embeddings))
        .route("/v1/rerank", post(rerank::rerank_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_virtual_key,
        ));

    let mut app = Router::new()
        .route("/health", get(routes::health))
        // Separate from /health (which never depends on provider state): the
        // observability view of background health checks (M6 §6.5).
        .route("/health/providers", get(health::providers_health))
        .route("/metrics", get(routes::metrics))
        .merge(api);

    if state.auth.is_some() {
        let admin_routes = Router::new()
            .route("/admin/keys", post(admin::create_key).get(admin::list_keys))
            .route("/admin/keys/{id}", patch(admin::patch_key))
            .route("/admin/provider-keys/{name}", put(admin::put_provider_key))
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                auth::require_master_key,
            ));
        app = app.merge(admin_routes);
    }

    app.with_state(state).layer(middleware_stack)
}

/// Rewrite a bare `413` from [`RequestBodyLimitLayer`] into the `LM-1002`
/// envelope.
///
/// `RequestBodyLimitLayer` returns its own plain-text `413` directly — it
/// never constructs a [`GatewayError`], so the response otherwise carries no
/// stable error code. This middleware wraps that layer and swaps any `413` it
/// produces for [`GatewayError::PayloadTooLarge`], keeping the `body_limit`
/// this gateway was configured with in the message.
///
/// This rewrites *every* `413`, trusting that only `RequestBodyLimitLayer`
/// (immediately inside this middleware in the stack below) ever produces one.
/// No handler in this crate returns 413 for any other reason today; if one
/// ever needs to, route it around this layer or it will be relabelled as a
/// body-size rejection.
async fn map_body_limit_response(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let response = next.run(request).await;
    if response.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return ApiError::from(GatewayError::PayloadTooLarge {
            limit: state.body_limit,
        })
        .into_response();
    }
    response
}

/// Conservative default security headers for every response (M7 §7.4).
///
/// LUMEN is a JSON/SSE API, never a browser-rendered app, so the strictest
/// values are safe: deny framing and sniffing, send no referrer, and lock the
/// CSP to `default-src 'none'`. HSTS is deliberately *not* set — it depends on
/// the deployment terminating TLS, so it is left to the operator's proxy.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    for (name, value) in [
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "no-referrer"),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; frame-ancestors 'none'",
        ),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
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
