//! Server lifecycle: serving with a bounded graceful shutdown.

use axum::serve::{Listener, ListenerExt};
use axum::Router;
use std::future::Future;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Wrap the accept loop so every accepted client socket gets `TCP_NODELAY`.
///
/// axum does not set it, and the kernel default is Nagle ON: without this,
/// each small SSE frame the gateway writes can sit in the send buffer waiting
/// for the previous packet's delayed ACK (~40-200 ms per frame on a real
/// network). A streaming completion is hundreds of small frames, so Nagle
/// multiplies total streaming time severalfold - the direct-to-provider
/// baseline (and every hosted gateway) disables it. The upstream leg already
/// has `TCP_NODELAY` via reqwest's default. Failure to set the flag is logged
/// and ignored: a slow socket beats a dropped connection.
fn with_nodelay(
    listener: TcpListener,
) -> impl Listener<Io = tokio::net::TcpStream, Addr = std::net::SocketAddr> {
    listener.tap_io(|stream| {
        if let Err(err) = stream.set_nodelay(true) {
            tracing::debug!(error = %err, "failed to set TCP_NODELAY on accepted connection");
        }
    })
}

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
    let server = axum::serve(with_nodelay(listener), app).with_graceful_shutdown(async move {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every connection accepted through the serving path must have
    /// `TCP_NODELAY` set, or Nagle stalls each small SSE frame behind the
    /// previous packet's delayed ACK and streaming latency blows up.
    #[tokio::test]
    async fn accepted_connections_have_nodelay_set() {
        let raw = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = raw.local_addr().expect("local addr");
        let mut listener = with_nodelay(raw);

        let client =
            tokio::spawn(
                async move { tokio::net::TcpStream::connect(addr).await.expect("connect") },
            );
        let (accepted, _peer) = listener.accept().await;
        assert!(
            accepted.nodelay().expect("read TCP_NODELAY"),
            "accepted socket must have TCP_NODELAY enabled"
        );
        drop(client);
    }
}
