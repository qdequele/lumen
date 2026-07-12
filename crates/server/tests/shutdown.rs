//! Graceful shutdown: an in-flight request must finish after the shutdown
//! signal fires, and the server must then return cleanly (process exit 0).

use axum::{routing::get, Router};
use ferrogate_server::serve;
use std::time::Duration;
use tokio::net::TcpListener;

#[tokio::test]
async fn inflight_request_completes_during_graceful_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // A deliberately slow route stands in for a long provider call.
    let app = Router::new().route(
        "/slow",
        get(|| async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            "done"
        }),
    );

    // The shutdown "signal" is a oneshot we fire mid-request.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        serve(listener, app, Duration::from_secs(30), async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    // Kick off the slow request.
    let request = tokio::spawn(async move { reqwest::get(format!("http://{addr}/slow")).await });

    // Let the request reach the handler, then trigger shutdown.
    tokio::time::sleep(Duration::from_millis(50)).await;
    shutdown_tx.send(()).unwrap();

    // The in-flight request still completes successfully...
    let resp = request.await.unwrap().unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "done");

    // ...and the server drains and returns Ok (process would exit 0).
    let result = server.await.unwrap();
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
