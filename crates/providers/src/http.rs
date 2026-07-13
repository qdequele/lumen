//! The shared HTTP client used by every provider.
//!
//! One [`reqwest::Client`] is created for the whole process and cloned into
//! each provider (a clone is a cheap `Arc` bump), so connections are pooled
//! across providers. `reqwest::Client` uses rustls, never OpenSSL.

use bytes::Bytes;
use lumen_core::ProviderError;
use futures::stream::{BoxStream, StreamExt};
use serde::Serialize;
use std::future::Future;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::mapping::{classify_status, parse_retry_after};

/// Build the process-wide HTTP client with default timeouts (10 s connect,
/// 300 s overall). Prefer [`build_client_with`] to honour the operator's
/// resilience config (M6).
#[must_use]
pub fn build_client() -> reqwest::Client {
    build_client_with(Duration::from_secs(10), Duration::from_secs(300))
}

/// Build the process-wide HTTP client with an explicit connect timeout
/// (LM-3012, client-wide — M6 §6.4) and an overall backstop.
///
/// The `overall` cap is a safety net so a wedged upstream cannot pin a
/// connection forever; the executor's total timeout (and cancellation on client
/// disconnect) normally fire first. One pooled client is shared across all
/// providers, so the connect timeout is necessarily process-wide.
#[must_use]
pub fn build_client_with(connect: Duration, overall: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect)
        .timeout(overall)
        .user_agent(concat!("lumen/", env!("CARGO_PKG_VERSION")))
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

/// Open a streaming POST: send `body` as JSON (optional bearer auth), and on a
/// success status return the response body as a `Bytes` stream, mapping
/// transport errors to [`ProviderError`]. The initial send honours `cancel`; the
/// returned stream is aborted by being dropped (the server holds the cancel drop
/// guard inside the response body — see ADR 004). A non-success status is
/// classified and returned as `Err` before any streaming begins.
pub async fn open_stream<B>(
    client: &reqwest::Client,
    url: &str,
    body: &B,
    api_key: Option<&str>,
    provider: &str,
    cancel: &CancellationToken,
) -> Result<BoxStream<'static, Result<Bytes, ProviderError>>, ProviderError>
where
    B: Serialize + ?Sized,
{
    let mut builder = client.post(url).json(body);
    if let Some(key) = api_key {
        builder = builder.bearer_auth(key);
    }
    open(builder, provider, cancel).await
}

/// Like [`open_stream`], but applies arbitrary request headers instead of
/// bearer auth (Anthropic's `x-api-key`, Google's `x-goog-api-key`). Header
/// values must never be logged.
pub async fn open_stream_with_headers<B>(
    client: &reqwest::Client,
    url: &str,
    body: &B,
    headers: &[(&str, &str)],
    provider: &str,
    cancel: &CancellationToken,
) -> Result<BoxStream<'static, Result<Bytes, ProviderError>>, ProviderError>
where
    B: Serialize + ?Sized,
{
    let mut builder = client.post(url).json(body);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    open(builder, provider, cancel).await
}

/// Send a prepared streaming request and return the response body as a `Bytes`
/// stream (shared core of the two `open_stream*` variants).
async fn open(
    builder: reqwest::RequestBuilder,
    provider: &str,
    cancel: &CancellationToken,
) -> Result<BoxStream<'static, Result<Bytes, ProviderError>>, ProviderError> {
    let call = async {
        let response = builder
            .send()
            .await
            .map_err(|e| map_transport(provider, &e))?;
        let status = response.status();
        if status.is_success() {
            Ok(response)
        } else {
            let retry_after = parse_retry_after(response.headers());
            Err(classify_status(provider, status.as_u16(), retry_after))
        }
    };

    let response = with_cancel(cancel, call).await?;
    let provider = provider.to_owned();
    Ok(response
        .bytes_stream()
        .map(move |item| item.map_err(|e| map_transport(&provider, &e)))
        .boxed())
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
pub(crate) fn map_transport(provider: &str, err: &reqwest::Error) -> ProviderError {
    if err.is_timeout() {
        // A timeout during connection establishment is distinct from a read
        // timeout (LM-3012 vs LM-3005) so operators can tell a dead host from
        // a slow one (M6 §6.4).
        if err.is_connect() {
            ProviderError::ConnectTimeout {
                provider: provider.to_owned(),
            }
        } else {
            ProviderError::Timeout {
                provider: provider.to_owned(),
            }
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
