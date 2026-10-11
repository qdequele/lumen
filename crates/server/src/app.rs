//! Assembly of the axum application and its middleware stack.

use crate::{
    admin, admin_scope, auth, chat, decisions, embeddings, health, models, openapi, rerank, routes,
    state::AppState,
};
use axum::extract::{MatchedPath, Request, State};
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
/// 2. open a tracing span per request - carrying method, path and request id,
///    but never the body or query string (user data is never logged);
/// 3. propagate the request id onto the response;
/// 4. measure per-request latency: one histogram sample
///    (`lumen_http_request_duration_seconds`) and one log event per request,
///    for EVERY endpoint including `/health`, `/metrics` and `/admin/*`;
/// 5. rewrite a bare `413` from the body-limit layer below into the `LM-1002`
///    envelope;
/// 6. reject bodies larger than `body_limit` bytes.
///
/// Route groups:
/// * `/health`, `/health/providers`, `/metrics` - operational, never
///   authenticated, no I/O (`/health` never depends on provider state);
/// * `/v1/*` - the API surface; virtual-key auth when enabled (M5);
/// * `/admin/*` and `/openapi.json` - key management, budget webhooks, usage
///   reporting and the gateway's own OpenAPI document; mounted only when auth
///   is enabled, protected by the master key.
///
/// The body-size limit is read from `state.body_limit` - the single source of
/// truth also surfaced in the `LM-1002` message - rather than a second
/// parameter, so the enforced limit and the advertised one can never diverge.
pub fn build_app(state: AppState) -> Router {
    let body_limit = state.body_limit;
    let middleware_stack = ServiceBuilder::new()
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(TraceLayer::new_for_http().make_span_with(make_request_span))
        .layer(PropagateRequestIdLayer::x_request_id())
        // Latency observability: every request - whatever the route - produces
        // a histogram sample and a completion log event carrying latency_ms.
        .layer(middleware::from_fn_with_state(state.clone(), track_latency))
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
        // Wildcard, not a single-segment `{id}`: model ids may legally
        // contain slashes (HF-style, e.g. "mistralai/Mistral-7B"). A
        // single-segment param would leave such paths unrouted - a bare 404
        // outside the LM envelope AND outside the auth layer below. The
        // wildcard captures the whole remainder (slashes included, one or
        // more segments) into the same handler; the literal `/v1/models`
        // above still takes priority, so the list route is not shadowed.
        // (matchit rejects registering `{id}` and `{*id}` side by side.)
        .route("/v1/models/{*id}", get(models::model))
        .route("/v1/chat/completions", post(chat::chat))
        .route("/v1/embeddings", post(embeddings::embeddings))
        .route("/v1/rerank", post(rerank::rerank_handler))
        .route("/v1/decisions", post(decisions::decisions_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_virtual_key,
        ));

    // The operational views that describe the whole platform: the provider
    // health view of background checks (M6 §6.5, separate from /health,
    // which never depends on provider state) and the Prometheus metrics
    // (provider, model and key labels across every account). An
    // account-scoped call (`X-Lumen-Account-Ref`, platform contract v2
    // section 8.3) is refused with 403 `LM-4005` like the other
    // platform-only routes; without the header both stay open as before.
    let operational = Router::new()
        .route("/health/providers", get(health::providers_health))
        .route("/metrics", get(routes::metrics))
        .route_layer(middleware::from_fn(admin_scope::platform_only));

    let mut app = Router::new()
        .route("/health", get(routes::health))
        .merge(operational)
        .merge(api);

    if state.auth.is_some() {
        app = app.merge(admin_routes(&state));
    }

    // Every request that matches no route above lands here (trailing-slash,
    // extra-segment and other near-miss paths) instead of returning a bare,
    // empty-body 404. The fallback sits OUTSIDE the `/v1` virtual-key auth
    // layer on purpose: `route_layer` only runs for routes that matched, and
    // an unmatched path is answered before any auth check (issue #88). A
    // `LM-1003` route-not-found envelope leaks no more than the bare 404 it
    // replaces: it names no path and discloses no route. A MATCHED `/v1` route
    // without a key still returns 401 `LM-4004`, since that route's existence
    // is not the secret - only its auth state is.
    app.fallback(route_not_found)
        .with_state(state)
        .layer(middleware_stack)
}

/// Router fallback for any request matching no route: the standard `LM-1003`
/// route-not-found envelope (issue #88), never a bare 404. It runs outside the
/// virtual-key auth layer, so it needs no state and touches no I/O.
async fn route_not_found() -> Response {
    ApiError::from(GatewayError::RouteNotFound).into_response()
}

/// Rewrite a bare `413` from [`RequestBodyLimitLayer`] into the `LM-1002`
/// envelope.
///
/// `RequestBodyLimitLayer` returns its own plain-text `413` directly - it
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

/// Measure and publish the latency of every request.
///
/// Emits, per request:
/// * one `lumen_http_request_duration_seconds` sample, labelled with the
///   method, the MATCHED route template (`/v1/chat/completions`, never the raw
///   URI - bounded cardinality, and user data never reaches a label) and the
///   response status;
/// * one `lumen::http` log event carrying `latency_ms`, inside the request
///   span (so it also carries the request id, method and path).
///
/// Requests that match no route are labelled `"unmatched"` rather than their
/// raw path. For streaming responses this measures time-to-response-headers;
/// the full-stream latency of API calls lands in
/// `lumen_request_duration_seconds` when accounting closes. A client that
/// disconnects before response headers drops this future - such a request
/// produces no sample, consistent with cancellation propagation (rule 3).
async fn track_latency(State(state): State<AppState>, request: Request, next: Next) -> Response {
    // `Router::layer` middleware runs after route matching, so the matched
    // route template is available on the request extensions. `MatchedPath` is
    // `Arc<str>`-backed: cloning it keeps the template alive past `next.run`
    // without allocating on the hot path.
    let path = request.extensions().get::<MatchedPath>().cloned();
    let method = method_label(request.method());
    let started = std::time::Instant::now();

    let response = next.run(request).await;

    let elapsed = started.elapsed();
    let status = response.status().as_u16();
    state.latency.observe_http(
        method,
        path.as_ref().map_or("unmatched", MatchedPath::as_str),
        status,
        elapsed.as_secs_f64(),
    );
    tracing::debug!(
        target: "lumen::http",
        status,
        latency_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        "request completed"
    );
    response
}

/// Normalise the HTTP method to a closed label set. Hyper accepts arbitrary
/// extension methods, so labelling the raw string would hand unauthenticated
/// clients an unbounded-cardinality lever; anything non-standard is `"other"`.
fn method_label(method: &axum::http::Method) -> &'static str {
    match *method {
        axum::http::Method::GET => "GET",
        axum::http::Method::POST => "POST",
        axum::http::Method::PUT => "PUT",
        axum::http::Method::PATCH => "PATCH",
        axum::http::Method::DELETE => "DELETE",
        axum::http::Method::HEAD => "HEAD",
        axum::http::Method::OPTIONS => "OPTIONS",
        _ => "other",
    }
}

/// Conservative default security headers for every response (M7 §7.4).
///
/// LUMEN is a JSON/SSE API, never a browser-rendered app, so the strictest
/// values are safe: deny framing and sniffing, send no referrer, and lock the
/// CSP to `default-src 'none'`. HSTS is deliberately *not* set - it depends on
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

/// Build the per-request tracing span. Only metadata - never the body or query.
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

/// The master-key-protected `/admin` surface (mounted only when auth is
/// enabled). Split out of [`build_app`] to keep that function readable.
///
/// Two halves: the account routes (keys, groups, usage), which a control
/// plane may scope to one account with `X-Lumen-Account-Ref`, and the
/// platform routes (provider keys and checks, webhooks, config,
/// `/openapi.json`), which
/// refuse a scoped call with `403 LM-4005` (platform contract v2 section
/// 8.3, see [`admin_scope`]). The master key is checked first on both.
fn admin_routes(state: &AppState) -> Router<AppState> {
    let account = Router::new()
        .route("/admin/keys", post(admin::create_key).get(admin::list_keys))
        .route(
            "/admin/keys/{id}",
            patch(admin::patch_key).delete(admin::delete_key),
        )
        .route("/admin/keys/{id}/rotate", post(admin::rotate_key))
        .route("/admin/keys/{id}/grant", post(admin::grant_key))
        .route(
            "/admin/groups",
            post(admin::create_group).get(admin::list_groups),
        )
        .route(
            "/admin/groups/{id}",
            get(admin::get_group)
                .patch(admin::patch_group)
                .delete(admin::delete_group),
        )
        .route("/admin/groups/{id}/grant", post(admin::grant_group))
        .route("/admin/usage", get(admin::usage_report))
        .route("/admin/usage/export", get(admin::usage_export));
    let platform = Router::new()
        .route("/admin/provider-keys/{name}", put(admin::put_provider_key))
        .route(
            "/admin/providers/{name}/check",
            post(admin::check_provider_key),
        )
        .route(
            "/admin/webhooks",
            get(admin::get_webhooks)
                .put(admin::put_webhooks)
                .delete(admin::delete_webhooks),
        )
        .route(
            "/admin/webhooks/signing-key",
            put(admin::put_webhook_signing_key).delete(admin::delete_webhook_signing_key),
        )
        .route(
            "/admin/config",
            get(admin::get_config).put(admin::put_config),
        )
        // Granular config endpoints (ADR 012 Task 8). The `providers`
        // routes are registered BEFORE the `{section}` catch-all below:
        // matchit (axum's router) always prefers a static segment
        // ("providers") over a same-position parameter ("{section}"), so
        // this ordering is not load-bearing for correctness, but keeps
        // the more specific routes visually adjacent to the resource
        // they specialize.
        .route("/admin/config/providers", get(admin::list_providers))
        .route(
            "/admin/config/providers/{name}",
            get(admin::get_provider)
                .put(admin::put_provider)
                .delete(admin::delete_provider),
        )
        .route(
            "/admin/config/virtual_models",
            get(admin::list_virtual_models),
        )
        .route(
            "/admin/config/virtual_models/{id}",
            get(admin::get_virtual_model)
                .put(admin::put_virtual_model)
                .delete(admin::delete_virtual_model),
        )
        .route(
            "/admin/config/virtual_models/{id}/plan",
            get(admin::get_virtual_model_plan),
        )
        .route(
            "/admin/config/{section}",
            get(admin::get_config_section).put(admin::put_config_section),
        )
        // The gateway's own contract (docs/openapi.yaml as JSON). Not under
        // `/admin`, but platform-only and master-key gated like it.
        .route("/openapi.json", get(openapi::openapi_json))
        .route_layer(middleware::from_fn(admin_scope::platform_only));
    account
        .merge(platform)
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_master_key,
        ))
}

#[cfg(test)]
mod routes_match_openapi {
    use std::collections::BTreeSet;

    /// Every `(METHOD, path)` this file mounts, parsed from its own source
    /// (up to this test module): each `.route(<path>, <chain>)` call and
    /// every `get(` / `post(` / `put(` / `patch(` / `delete(` inside that
    /// call's parentheses. axum does not expose a route listing, and the
    /// source is the truth. `<path>` must be a string literal; a `.route(`
    /// call the parser cannot read fails the test rather than going
    /// unchecked.
    fn mounted() -> BTreeSet<(String, String)> {
        let full = include_str!("app.rs");
        let source = full
            .split("mod routes_match_openapi {")
            .next()
            .unwrap_or(full);
        let route = regex::Regex::new(r#"\.route\(\s*"([^"]+)"\s*,"#).unwrap();
        let method = regex::Regex::new(r"\b(get|post|put|patch|delete)\(").unwrap();
        let calls = source.matches(".route(").count();
        let mut parsed = 0;
        let mut out = BTreeSet::new();
        for found in route.captures_iter(source) {
            parsed += 1;
            let path = found[1].to_owned();
            let rest = &source[found.get(0).unwrap().end()..];
            let mut depth = 1_i32;
            let mut end = rest.len();
            for (i, c) in rest.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            for m in method.captures_iter(&rest[..end]) {
                out.insert((m[1].to_uppercase(), normalize(&path)));
            }
        }
        assert_eq!(
            parsed, calls,
            "a `.route(` call has a path the parser cannot read"
        );
        out
    }

    /// `{*id}` (axum wildcard) and `{id}` document the same parameter.
    fn normalize(path: &str) -> String {
        path.replace("{*", "{")
    }

    fn spec() -> serde_json::Value {
        serde_yaml_ng::from_str(include_str!("../../../docs/openapi.yaml")).unwrap()
    }

    fn documented() -> BTreeSet<(String, String)> {
        let spec = spec();
        let mut out = BTreeSet::new();
        for (path, item) in spec["paths"].as_object().unwrap() {
            for (method, _) in item.as_object().unwrap() {
                if ["get", "post", "put", "patch", "delete"].contains(&method.as_str()) {
                    out.insert((method.to_uppercase(), normalize(path)));
                }
            }
        }
        out
    }

    #[test]
    fn every_mounted_route_is_documented_and_nothing_else() {
        let mounted = mounted();
        assert!(
            mounted.len() >= 30,
            "the parser found too few routes: {mounted:?}"
        );
        assert_eq!(
            mounted,
            documented(),
            "app.rs and docs/openapi.yaml disagree"
        );
    }

    #[test]
    fn the_spec_version_is_the_crate_version() {
        let spec = spec();
        assert_eq!(spec["openapi"], "3.1.0");
        assert_eq!(spec["info"]["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn admin_routes_require_the_master_key_in_the_spec() {
        let spec = spec();
        for (path, item) in spec["paths"].as_object().unwrap() {
            for (method, op) in item.as_object().unwrap() {
                if !["get", "post", "put", "patch", "delete"].contains(&method.as_str()) {
                    continue;
                }
                let schemes: Vec<&str> = op["security"]
                    .as_array()
                    .map(|reqs| {
                        reqs.iter()
                            .flat_map(|r| r.as_object().unwrap().keys())
                            .map(String::as_str)
                            .collect()
                    })
                    .unwrap_or_default();
                let expected: &[&str] = if path.starts_with("/admin/") || path == "/openapi.json" {
                    &["masterKey"]
                } else if path.starts_with("/v1/") {
                    &["virtualKey"]
                } else {
                    &[]
                };
                assert_eq!(schemes, expected, "{method} {path}");
            }
        }
    }
}
