//! The shared HTTP client used by every provider.
//!
//! One [`reqwest::Client`] is created for the whole process and cloned into
//! each provider (a clone is a cheap `Arc` bump), so connections are pooled
//! across providers. `reqwest::Client` uses rustls, never OpenSSL.

use bytes::Bytes;
use ferrogate_core::ProviderError;
use serde::Serialize;
use std::future::Future;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::mapping::{classify_status, parse_retry_after};

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

/// POST `body` as JSON to `url`, with optional bearer auth, honouring `cancel`.
///
/// The whole request/response is subject to cancellation (see [`with_cancel`]),
/// so a client disconnect aborts the in-flight upstream call. On a success
/// status the raw response body is returned for the provider to translate; on a
/// non-success status the shared [`classify_status`] policy applies (429 → rate
/// limited, 5xx → retryable upstream, other → fatal upstream). Transport
/// failures map to [`ProviderError::Timeout`] or [`ProviderError::Unavailable`].
///
/// Every provider shares this path so transport handling and error
/// classification are identical across them; only body translation differs.
pub async fn post_json<B>(
    client: &reqwest::Client,
    url: &str,
    body: &B,
    api_key: Option<&str>,
    provider: &str,
    cancel: &CancellationToken,
) -> Result<Bytes, ProviderError>
where
    B: Serialize + ?Sized,
{
    let mut builder = client.post(url).json(body);
    if let Some(key) = api_key {
        builder = builder.bearer_auth(key);
    }
    send(builder, provider, cancel).await
}

/// Like [`post_json`], but applies arbitrary request headers instead of bearer
/// auth. Used by providers whose auth is not a bearer token (e.g. Anthropic's
/// `x-api-key` + `anthropic-version`). Header values must never be logged.
pub async fn post_json_with_headers<B>(
    client: &reqwest::Client,
    url: &str,
    body: &B,
    headers: &[(&str, &str)],
    provider: &str,
    cancel: &CancellationToken,
) -> Result<Bytes, ProviderError>
where
    B: Serialize + ?Sized,
{
    let mut builder = client.post(url).json(body);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    send(builder, provider, cancel).await
}

/// Send a prepared request, honouring `cancel`, and classify the outcome. On a
/// success status the raw body is returned; otherwise the shared
/// [`classify_status`] policy applies.
async fn send(
    builder: reqwest::RequestBuilder,
    provider: &str,
    cancel: &CancellationToken,
) -> Result<Bytes, ProviderError> {
    let call = async {
        let response = builder
            .send()
            .await
            .map_err(|e| map_transport(provider, &e))?;
        let status = response.status();

        if status.is_success() {
            response
                .bytes()
                .await
                .map_err(|e| map_transport(provider, &e))
        } else {
            let retry_after = parse_retry_after(response.headers());
            Err(classify_status(provider, status.as_u16(), retry_after))
        }
    };

    with_cancel(cancel, call).await
}

/// Map a reqwest transport error to a provider error, distinguishing a timeout
/// from an unreachable upstream. Never embeds the URL or any request detail.
fn map_transport(provider: &str, err: &reqwest::Error) -> ProviderError {
    if err.is_timeout() {
        ProviderError::Timeout {
            provider: provider.to_owned(),
        }
    } else {
        ProviderError::Unavailable {
            provider: provider.to_owned(),
        }
    }
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
