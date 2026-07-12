//! Shared helpers for server integration tests.

use ferrogate_server::{build_app, serve, AppState};
use ferrogate_telemetry::Metrics;
use std::time::Duration;
use tokio::net::TcpListener;

/// Spawn the real app on an ephemeral port with the given body limit and return
/// its base URL (e.g. `http://127.0.0.1:54321`).
pub async fn spawn_with_limit(body_limit: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("read local addr");
    let app = build_app(AppState::new(Metrics::new()), body_limit);

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

/// Spawn the app with the default 10 MiB body limit.
pub async fn spawn() -> String {
    spawn_with_limit(10 * 1024 * 1024).await
}
