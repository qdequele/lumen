//! Billable bandwidth metering for the `/v1` routes.
//!
//! [`count_bandwidth`] runs inside the virtual-key auth layer. It wraps the
//! request body (bytes the handler actually reads) and the response body
//! (bytes actually yielded to the client) in counting bodies, and credits the
//! authenticated key through [`KeyEntry::add_bytes`](lumen_auth::state::KeyEntry::add_bytes)
//! exactly once: when the response body ends or is dropped, whichever comes
//! first.
//!
//! Only requests that reached at least one upstream attempt are billed. A
//! handler calls [`UpstreamMark::mark`] at the start of every attempt, before
//! the provider call; a request refused by the gateway (invalid body, budget,
//! rate limit) never marks and so is never billed. Requests without an
//! authenticated key are not counted.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use http_body::{Body as HttpBody, Frame, SizeHint};
use lumen_auth::state::KeyEntry;

use crate::auth::AuthedKey;

/// Set by a handler once the request reaches (or is about to reach) an
/// upstream provider. Shared between the handler and the metering layer via
/// request extensions.
#[derive(Clone, Default, Debug)]
pub struct UpstreamMark(Arc<AtomicBool>);

impl UpstreamMark {
    /// Record that an upstream attempt is starting.
    pub fn mark(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether any upstream attempt started.
    #[must_use]
    pub fn is_marked(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Request-body wrapper counting the data bytes the handler reads.
struct CountingRequest {
    inner: Body,
    read: Arc<AtomicU64>,
}

impl HttpBody for CountingRequest {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let poll = Pin::new(&mut self.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &poll {
            if let Some(data) = frame.data_ref() {
                self.read.fetch_add(data.len() as u64, Ordering::Relaxed);
            }
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// What to credit, and to whom, when the response body finishes.
struct Credit {
    entry: Arc<KeyEntry>,
    mark: UpstreamMark,
    bytes_in: Arc<AtomicU64>,
}

/// Response-body wrapper counting the data bytes yielded to the client.
///
/// The credit is an `Option` taken exactly once, by end of stream or by
/// `Drop` (client disconnect, handler error), so bytes are never counted
/// twice.
struct CountingResponse {
    inner: Body,
    written: u64,
    credit: Option<Credit>,
}

impl CountingResponse {
    fn settle(&mut self) {
        if let Some(credit) = self.credit.take() {
            if credit.mark.is_marked() {
                credit
                    .entry
                    .add_bytes(credit.bytes_in.load(Ordering::Relaxed), self.written);
            }
        }
    }
}

impl HttpBody for CountingResponse {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let poll = Pin::new(&mut self.inner).poll_frame(cx);
        match &poll {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    self.written += data.len() as u64;
                }
            }
            Poll::Ready(None | Some(Err(_))) => self.settle(),
            Poll::Pending => {}
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for CountingResponse {
    fn drop(&mut self) {
        self.settle();
    }
}

/// `/v1` middleware: meter request and response bytes for the authenticated
/// key. Must sit inside [`require_virtual_key`](crate::auth::require_virtual_key)
/// so the [`AuthedKey`] extension is present.
pub async fn count_bandwidth(mut req: Request, next: Next) -> Response {
    let mark = UpstreamMark::default();
    req.extensions_mut().insert(mark.clone());
    let Some(AuthedKey(entry)) = req.extensions().get::<AuthedKey>().cloned() else {
        return next.run(req).await;
    };
    let bytes_in = Arc::new(AtomicU64::new(0));
    let (parts, body) = req.into_parts();
    let counted = Body::new(CountingRequest {
        inner: body,
        read: Arc::clone(&bytes_in),
    });
    let response = next.run(Request::from_parts(parts, counted)).await;
    let (parts, body) = response.into_parts();
    let body = Body::new(CountingResponse {
        inner: body,
        written: 0,
        credit: Some(Credit {
            entry,
            mark,
            bytes_in,
        }),
    });
    Response::from_parts(parts, body)
}
