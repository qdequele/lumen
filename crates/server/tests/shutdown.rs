//! Graceful shutdown: an in-flight request must finish after the shutdown
//! signal fires, and the server must then return cleanly (process exit 0).

use axum::{routing::get, Router};
use lumen_server::serve;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

#[tokio::test]
async fn inflight_request_completes_during_graceful_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // A slow route stands in for a long provider call. It is driven by
    // signals, never by sleeps: `entered` fires once the handler runs (the
    // request is genuinely in flight), and the handler only answers on
    // `release`. A timed guess raced under load: a connection whose request
    // was not read yet counts as idle and is closed by graceful shutdown.
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let app = {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        Router::new().route(
            "/slow",
            get(move || {
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                async move {
                    // `notify_one` stores a permit, so neither side can miss it.
                    entered.notify_one();
                    release.notified().await;
                    "done"
                }
            }),
        )
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        serve(listener, app, Duration::from_secs(30), async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    // Kick off the slow request and wait until it is inside the handler.
    let request = tokio::spawn(async move { reqwest::get(format!("http://{addr}/slow")).await });
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .expect("the request never reached the handler");

    // Trigger shutdown, and wait until it has taken effect (the listener is
    // closed, so a fresh connection is refused) while the request is still
    // held: the request below then really completes during the drain.
    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while TcpStream::connect(addr).await.is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the listener was not closed after the shutdown signal");
    assert!(
        !server.is_finished(),
        "server exited with a request in flight"
    );
    release.notify_one();

    // The in-flight request still completes successfully...
    let resp = request.await.unwrap().unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "done");

    // ...and the server drains and returns Ok (process would exit 0).
    let result = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("server did not finish draining")
        .unwrap();
    assert!(result.is_ok(), "serve returned an error: {result:?}");
}

#[tokio::test]
async fn server_stops_accepting_after_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route("/ping", get(|| async { "pong" }));

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        serve(listener, app, Duration::from_secs(5), async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    // Trigger shutdown with no in-flight requests; server should exit promptly.
    shutdown_tx.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server did not shut down within 5s")
        .unwrap();
    assert!(result.is_ok());

    // A new connection now fails to complete a request.
    let after = reqwest::get(format!("http://{addr}/ping")).await;
    assert!(
        after.is_err(),
        "expected connection to be refused after shutdown"
    );
}
