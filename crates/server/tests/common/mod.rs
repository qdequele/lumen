//! Shared helpers for server integration tests.
//!
//! Each integration-test binary includes this module and uses a different
//! subset of helpers, so unused-in-one-binary warnings are expected here.
#![allow(dead_code)]

use lumen_providers::{http, Registry};
use lumen_server::resilience::ResilienceRuntime;
use lumen_server::{build_app, serve, AppState, StreamGuards};
use lumen_telemetry::{Metrics, TokenMetrics};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

/// Build a fresh AppState with default guards and no auth.
pub fn base_state(registry: Arc<Registry>) -> AppState {
    let metrics = Metrics::new();
    let tokens = TokenMetrics::register(&metrics, &[]).expect("register token metrics");
    AppState::new(metrics, registry, tokens)
}

/// Spawn the app with a given registry and body limit; returns its base URL.
pub async fn spawn_with(registry: Arc<Registry>, body_limit: usize) -> String {
    spawn_with_guards(registry, body_limit, StreamGuards::default()).await
}

/// Spawn the app with explicit stream guard timings (first-token timeout,
/// heartbeat interval) — for the LM-3011 tests, which need a short window.
pub async fn spawn_with_guards(
    registry: Arc<Registry>,
    body_limit: usize,
    guards: StreamGuards,
) -> String {
    // Align the executor's first-token timeout with the guard the test set, so
    // the LM-3011 tests still exercise their short window (the executor now owns
    // the first-token deadline, M6).
    let resilience =
        Arc::new(ResilienceRuntime::defaults().with_first_token(guards.first_token_timeout));
    let state = base_state(registry)
        .with_guards(guards)
        .with_resilience(resilience);
    spawn_state(state, body_limit).await
}

/// Spawn the app from a fully-built state (auth/pricing/usage attached by the
/// caller); returns its base URL.
///
/// `body_limit` is applied onto `state` here (the single place it's threaded
/// through to `build_app`), so `AppState.body_limit` — surfaced in the
/// `LM-1002` message — can never drift from the limit `build_app` actually
/// enforces.
pub async fn spawn_state(state: AppState, body_limit: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("read local addr");
    let app = build_app(state.with_body_limit(body_limit));

    // `pending()` shutdown = never shut down for the lifetime of the test.
    tokio::spawn(async move {
        let _ = serve(
            listener,
            app,
            Duration::from_secs(5),
            std::future::pending(),
        )
        .await;
    });

    format!("http://{addr}")
}

/// An empty registry (no providers) — for tests that don't hit `/v1/*`.
#[must_use]
pub fn empty_registry() -> Arc<Registry> {
    Arc::new(Registry::build(Vec::new(), http::build_client()).expect("empty registry builds"))
}

/// Spawn with the given body limit and an empty registry.
pub async fn spawn_with_limit(body_limit: usize) -> String {
    spawn_with(empty_registry(), body_limit).await
}

/// Spawn with the default 10 MiB body limit and an empty registry.
pub async fn spawn() -> String {
    spawn_with_limit(10 * 1024 * 1024).await
}
