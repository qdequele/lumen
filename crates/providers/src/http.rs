//! The shared HTTP client used by every provider.
//!
//! One [`reqwest::Client`] is created for the whole process and cloned into
//! each provider (a clone is a cheap `Arc` bump), so connections are pooled
//! across providers. `reqwest::Client` uses rustls, never OpenSSL.

use ferrogate_core::ProviderError;
use std::future::Future;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Build the process-wide HTTP client.
///
/// Timeouts here are coarse startup defaults; fine-grained per-phase timeouts
/// (connect / first-token / total) arrive in M6.
#[must_use]
pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        // A generous overall cap so a wedged upstream cannot pin a connection
        // forever; cancellation (client disconnect) aborts sooner than this.
        .timeout(Duration::from_secs(300))
        .user_agent(concat!("ferrogate/", env!("CARGO_PKG_VERSION")))
        .build()
        // Falls back to the default client if the builder somehow fails; the
        // default is always constructible, so this cannot panic in practice.
        .unwrap_or_default()
}

/// Run an upstream call, aborting it if `cancel` fires first.
///
/// When the downstream client disconnects the token fires, `fut` is dropped,
/// and the underlying reqwest future is cancelled — closing the connection so
/// the upstream stops working. `biased` makes cancellation win a tie.
pub async fn with_cancel<F, T>(cancel: &CancellationToken, fut: F) -> Result<T, ProviderError>
where
    F: Future<Output = Result<T, ProviderError>>,
{
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(ProviderError::Cancelled),
        result = fut => result,
    }
}
