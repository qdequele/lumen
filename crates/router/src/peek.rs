//! First-frame peek for streaming commitment (ADR 005, 2026-07-15 amendment).
//!
//! Opening an upstream byte stream (2xx status + headers) is not yet a promise
//! of content: the upstream can return 200 and then error, or close, before a
//! single frame reaches the client. [`peek_first_frame`] closes that gap. It
//! awaits the **first** stream item and decides the commitment point:
//!
//! * first item is a content frame (`Ok`) - commit: the frame is re-attached
//!   ahead of the untouched remainder and the reconstructed stream is returned
//!   for the caller's frame guards to own (no further retry);
//! * first item is an error (`Err`) - a pre-commit failure: the error is
//!   returned so the executor retries / falls back and penalises the breaker,
//!   exactly as if the *open* had failed;
//! * the stream ends before any frame (`None`) - the same pre-commit failure,
//!   surfaced as [`ProviderError::EmptyStream`].
//!
//! Buffering is bounded to a single frame (ADR 004 forbids buffering the whole
//! stream). The peek races the [`CancellationToken`]: a client disconnect
//! during the window returns [`ProviderError::Cancelled`] and drops the stream,
//! aborting the upstream. The caller wraps the peek in the per-attempt
//! `first_token` timeout, so a silent upstream (headers, then no bytes) also
//! fails over instead of hanging.

use futures::stream::{BoxStream, StreamExt};
use lumen_core::ProviderError;
use tokio_util::sync::CancellationToken;

/// Peek the first item of an opened upstream stream to decide commitment.
///
/// Returns the same stream with the first frame re-attached once a content
/// frame arrives, or a pre-commit [`ProviderError`] (which the executor treats
/// like an open failure: retry, fallback, breaker penalty).
///
/// Buffers at most one frame. The `provider` name only labels an
/// [`ProviderError::EmptyStream`]; it is otherwise untouched.
///
/// # Errors
///
/// - the first item's own error, verbatim, when the upstream errors before any
///   content frame;
/// - [`ProviderError::EmptyStream`] when the stream ends before any frame;
/// - [`ProviderError::Cancelled`] when `cancel` fires during the peek window.
pub async fn peek_first_frame<T>(
    mut stream: BoxStream<'static, Result<T, ProviderError>>,
    provider: &str,
    cancel: &CancellationToken,
) -> Result<BoxStream<'static, Result<T, ProviderError>>, ProviderError>
where
    T: Send + 'static,
{
    let first = tokio::select! {
        biased;
        // A disconnect mid-peek aborts before we commit; dropping `stream` on
        // return releases the upstream handle.
        () = cancel.cancelled() => return Err(ProviderError::Cancelled),
        item = stream.next() => item,
    };

    match first {
        // Commit. Re-attach the buffered frame ahead of the untouched tail so
        // the forwarded body is byte-identical to the upstream (zero-copy: the
        // frame is moved, never copied).
        Some(Ok(frame)) => {
            let head = futures::stream::once(std::future::ready(Ok(frame)));
            Ok(head.chain(stream).boxed())
        }
        Some(Err(error)) => Err(error),
        None => Err(ProviderError::EmptyStream {
            provider: provider.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn frames(
        items: Vec<Result<&'static str, ProviderError>>,
    ) -> BoxStream<'static, Result<&'static str, ProviderError>> {
        futures::stream::iter(items).boxed()
    }

    async fn collect_ok(
        stream: BoxStream<'static, Result<&'static str, ProviderError>>,
    ) -> Vec<&'static str> {
        stream
            .map(|item| item.expect("no error expected in the committed tail"))
            .collect()
            .await
    }

    /// Extract the pre-commit error (the `Ok` arm is a `BoxStream`, not `Debug`).
    fn expect_precommit_error(
        result: Result<BoxStream<'static, Result<&'static str, ProviderError>>, ProviderError>,
    ) -> ProviderError {
        match result {
            Ok(_) => panic!("expected a pre-commit error, got a committed stream"),
            Err(error) => error,
        }
    }

    #[tokio::test]
    async fn first_content_frame_commits_and_reemits_the_whole_stream() {
        let cancel = CancellationToken::new();
        let committed = peek_first_frame(frames(vec![Ok("a"), Ok("b")]), "p", &cancel)
            .await
            .expect("a content frame commits");
        // The peeked frame is re-attached, not swallowed.
        assert_eq!(collect_ok(committed).await, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn first_error_frame_is_a_pre_commit_failure() {
        let cancel = CancellationToken::new();
        let err = expect_precommit_error(
            peek_first_frame(
                frames(vec![Err(ProviderError::Upstream {
                    provider: "p".to_owned(),
                    status: 500,
                    retryable: true,
                })]),
                "p",
                &cancel,
            )
            .await,
        );
        assert!(err.is_retryable() && err.is_provider_fault());
    }

    #[tokio::test]
    async fn stream_ending_before_any_frame_yields_empty_stream() {
        let cancel = CancellationToken::new();
        let err =
            expect_precommit_error(peek_first_frame(frames(vec![]), "primary", &cancel).await);
        assert!(
            matches!(err, ProviderError::EmptyStream { ref provider } if provider == "primary"),
            "got: {err:?}"
        );
        // And it is treated as a retryable provider fault (fallback + breaker).
        assert!(ProviderError::EmptyStream {
            provider: "primary".to_owned()
        }
        .is_provider_fault());
    }

    /// A stream whose state flips a flag when dropped, standing in for the
    /// upstream connection handle: if the peek drops it, the upstream is aborted.
    fn pending_stream_with_drop_flag(
        flag: Arc<AtomicBool>,
    ) -> BoxStream<'static, Result<&'static str, ProviderError>> {
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        futures::stream::unfold(DropFlag(flag), |guard| async move {
            // Never yields; keeps `guard` alive until the stream is dropped.
            std::future::pending::<()>().await;
            Some((Ok("unreachable"), guard))
        })
        .boxed()
    }

    #[tokio::test]
    async fn cancel_during_peek_aborts_and_drops_the_upstream() {
        let dropped = Arc::new(AtomicBool::new(false));
        let stream = pending_stream_with_drop_flag(dropped.clone());
        let cancel = CancellationToken::new();
        cancel.cancel(); // client already gone when the peek runs

        let err = expect_precommit_error(peek_first_frame(stream, "p", &cancel).await);
        assert!(matches!(err, ProviderError::Cancelled));
        // Cancellation is never a fallback trigger nor a breaker penalty.
        assert!(!err.is_retryable() && !err.is_provider_fault());
        // The stream (upstream handle) was dropped: the upstream is aborted.
        assert!(
            dropped.load(Ordering::SeqCst),
            "upstream stream not dropped"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn peek_is_bounded_by_the_callers_timeout() {
        // The executor wraps the peek in the per-attempt `first_token` timeout;
        // a silent upstream (headers, then no bytes) must therefore time out
        // rather than hang forever.
        let dropped = Arc::new(AtomicBool::new(false));
        let stream = pending_stream_with_drop_flag(dropped.clone());
        let cancel = CancellationToken::new();
        let peek = peek_first_frame(stream, "p", &cancel);
        let timed_out = tokio::time::timeout(Duration::from_millis(50), peek).await;
        assert!(
            timed_out.is_err(),
            "peek should not resolve on a silent upstream"
        );
        // Timing out drops the peek future, which drops the stream → upstream aborted.
        assert!(
            dropped.load(Ordering::SeqCst),
            "upstream stream not dropped on timeout"
        );
    }
}
