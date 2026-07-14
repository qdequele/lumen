//! Server lifecycle: serving with a bounded graceful shutdown.

use axum::Router;
use std::future::Future;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Serve `app` on `listener` until `shutdown` resolves, then drain in-flight
/// requests for at most `drain_timeout` before forcing exit.
///
/// When `shutdown` fires the server stops accepting new connections and waits
/// for in-flight requests to finish. If draining exceeds `drain_timeout`, the
/// function returns anyway (the process then exits) rather than hanging forever.
pub async fn serve<F>(
    listener: TcpListener,
    app: Router,
    drain_timeout: Duration,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let token = CancellationToken::new();

    // Translate the external shutdown future into a cancellation.
    let signal_token = token.clone();
    tokio::spawn(async move {
        shutdown.await;
        signal_token.cancel();
    });

    let graceful_token = token.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        graceful_token.cancelled().await;
    });

    // Hard deadline: `drain_timeout` after shutdown begins, give up draining.
    let hard_deadline = async move {
        token.cancelled().await;
        tokio::time::sleep(drain_timeout).await;
    };

    tokio::select! {
        result = server => result,
        () = hard_deadline => {
            tracing::warn!(
                timeout_secs = drain_timeout.as_secs(),
                "graceful shutdown exceeded drain timeout; forcing exit"
            );
            Ok(())
        }
    }
}

/// Resolve when the process receives SIGINT (Ctrl-C) or SIGTERM.
///
/// If a signal handler cannot be installed, that branch simply never fires
/// (we never panic here) - the other signal still works.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }

    tracing::info!("shutdown signal received; draining");
}
