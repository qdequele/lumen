//! Automatic batching for embedding requests.
//!
//! When a request has more inputs than the provider's `max_batch_size`, it is
//! split into sub-batches run with bounded concurrency, then reassembled **in
//! the original order** with summed token usage. A sub-batch that still fails
//! after its own retries fails the whole request (no partial results in v1).
//!
//! # Sub-batch retry (ADR 005, 2026-11-12 amendment)
//!
//! A sub-batch that fails with a retryable error is retried on its own, so
//! the sub-batches that already completed (and were billed) are kept instead
//! of being thrown away for the retry above to start again from input 0. This
//! matters most for the single-input providers (Bedrock, Vertex), where N
//! inputs are N upstream calls. See [`SubBatchRetry`] for the bounds.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use futures::stream::{self, StreamExt, TryStreamExt};
use lumen_core::ProviderError;
use lumen_core::{
    EmbedData, EmbedInput, EmbedRequest, EmbedResponse, EmbedUsage, EmbeddingProvider,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Default number of sub-batches sent concurrently.
pub const DEFAULT_CONCURRENCY: usize = 4;

/// Bounds of the retry applied to ONE failed sub-batch of a split request.
///
/// The classification and the backoff maths are the ones the request-level
/// retry uses (`crates/router/src/retry.rs`): only
/// [`ProviderError::is_retryable`] errors are retried (429, 5xx, timeouts,
/// unreachable; never a client fault), the delay is exponential with equal
/// jitter, and an upstream `Retry-After` is a floor.
///
/// Three bounds keep a retry from amplifying a failing upstream:
///
/// - `max_attempts` caps the calls for one sub-batch;
/// - `max_total_backoff` caps the time one request spends waiting, summed
///   over all its sub-batches. A wait that does not fit (a long
///   `Retry-After` included) is not started and the error is returned;
/// - a wait pauses the whole request, not only the failed sub-batch: no new
///   upstream call starts before it ends, and the retry runs in the
///   concurrency slot of the sub-batch that failed, so the number of calls
///   in flight never exceeds the configured concurrency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubBatchRetry {
    /// Upstream calls allowed for one sub-batch, the first included (`1`
    /// disables the sub-batch retry).
    pub max_attempts: u32,
    /// Base backoff delay (the wait after the first failure, before jitter).
    pub base: Duration,
    /// Ceiling on the exponential term, before jitter and `Retry-After`.
    pub max: Duration,
    /// Longest time one request may spend waiting between sub-batch
    /// attempts, all sub-batches together.
    pub max_total_backoff: Duration,
}

impl Default for SubBatchRetry {
    /// 3 attempts, 200 ms base and 5 s cap (the request-level defaults), and
    /// at most 10 s of waiting per request: well inside the default
    /// first-token timeout (30 s) that bounds the whole call.
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base: Duration::from_millis(200),
            max: Duration::from_secs(5),
            max_total_backoff: Duration::from_secs(10),
        }
    }
}

impl SubBatchRetry {
    /// A policy that never retries a sub-batch.
    #[must_use]
    pub const fn never() -> Self {
        Self {
            max_attempts: 1,
            base: Duration::ZERO,
            max: Duration::ZERO,
            max_total_backoff: Duration::ZERO,
        }
    }
}

/// The delay before the retry that follows `failed_attempt` (0-based).
///
/// Same formula as the router's `backoff_delay`: `exp = min(base * 2^n, max)`,
/// equal jitter `exp/2 + (exp/2) * rand01`, floored at `retry_after`.
fn backoff_delay(
    failed_attempt: u32,
    policy: &SubBatchRetry,
    retry_after: Option<Duration>,
    rand01: f64,
) -> Duration {
    let base_ms = u64::try_from(policy.base.as_millis()).unwrap_or(u64::MAX);
    let max_ms = u64::try_from(policy.max.as_millis()).unwrap_or(u64::MAX);
    let factor = 1u64.checked_shl(failed_attempt).unwrap_or(u64::MAX);
    let exp_ms = base_ms.saturating_mul(factor).min(max_ms);

    let half = exp_ms / 2;
    let r = rand01.clamp(0.0, 1.0);
    // `half` is at most `max_ms` (a few thousand), well within f64's exact
    // integer range; truncating the product toward zero is the intended floor.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let jitter = (half as f64 * r) as u64;
    let mut delay_ms = half + jitter;

    if let Some(after) = retry_after {
        delay_ms = delay_ms.max(u64::try_from(after.as_millis()).unwrap_or(u64::MAX));
    }
    Duration::from_millis(delay_ms)
}

/// A lock-free pseudo-random fraction in `[0, 1)` (splitmix64), as in the
/// router's retry: no dependency, no blocking, no clock read. Jitter only.
fn jitter01() -> f64 {
    const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
    static STATE: AtomicU64 = AtomicU64::new(GAMMA);
    let seed = STATE
        .fetch_add(GAMMA, Ordering::Relaxed)
        .wrapping_add(GAMMA);
    let mut z = seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // Top 53 bits scaled into [0, 1): exact, no precision loss for this range.
    #[allow(clippy::cast_precision_loss)]
    {
        (z >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// The pause shared by every sub-batch of one request.
///
/// A sub-batch that must wait before its retry extends the pause; every
/// sub-batch waits for it to end before it sends anything. The time added to
/// the pause is charged to the request's backoff budget, so the budget is
/// wall-clock time: overlapping waits of concurrent sub-batches count once.
struct Pause {
    policy: SubBatchRetry,
    /// Never held across an `.await`.
    state: Mutex<PauseState>,
}

#[derive(Default)]
struct PauseState {
    /// No upstream call starts before this instant.
    until: Option<Instant>,
    /// Pause time granted so far.
    spent: Duration,
}

impl Pause {
    fn new(policy: SubBatchRetry) -> Self {
        Self {
            policy,
            state: Mutex::new(PauseState::default()),
        }
    }

    /// Ask for the request to pause for `delay` from now. `false` when the
    /// extra wait does not fit in what is left of the backoff budget: the
    /// caller then gives up and returns its error.
    fn extend(&self, delay: Duration) -> bool {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let from = state.until.filter(|until| *until > now).unwrap_or(now);
        let Some(wanted) = now.checked_add(delay) else {
            return false;
        };
        let until = wanted.max(from);
        let spent = state.spent.saturating_add(until - from);
        if spent > self.policy.max_total_backoff {
            return false;
        }
        state.spent = spent;
        state.until = Some(until);
        true
    }

    /// Wait until the pause is over (it may be extended meanwhile).
    ///
    /// # Errors
    /// [`ProviderError::Cancelled`] when the request is cancelled first.
    async fn wait(&self, cancel: &CancellationToken) -> Result<(), ProviderError> {
        loop {
            let until = self
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .until
                .filter(|until| *until > Instant::now());
            let Some(until) = until else {
                return Ok(());
            };
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(ProviderError::Cancelled),
                () = tokio::time::sleep_until(until) => {}
            }
        }
    }
}

/// Run one sub-batch: send it, and retry it alone on a retryable failure
/// within the bounds of [`SubBatchRetry`]. Verifies the returned vector count
/// against the count asked for, so a short upstream response is rejected
/// (never silently padded with an index gap) before reassembly (issue #89).
async fn embed_sub_batch(
    provider: &dyn EmbeddingProvider,
    sub: EmbedRequest,
    cancel: &CancellationToken,
    pause: &Pause,
) -> Result<EmbedResponse, ProviderError> {
    let expected = sub.input.len();
    let max_attempts = pause.policy.max_attempts.max(1);
    // The request is moved into the last attempt allowed and cloned for the
    // ones before it: the copy is what a retry needs to send again.
    let mut sub = Some(sub);
    let mut failed = 0u32;
    loop {
        pause.wait(cancel).await?;
        let attempt = if failed + 1 >= max_attempts {
            sub.take()
        } else {
            sub.clone()
        };
        let Some(attempt) = attempt else {
            // Unreachable: the request is taken only by the last attempt,
            // which always returns. Kept total instead of panicking.
            return Err(ProviderError::Translation(
                "embedding sub-batch retry lost its request".to_owned(),
            ));
        };
        let error = match provider.embed(attempt, cancel.clone()).await {
            Ok(resp) => {
                check_embedding_count(resp.data.len(), expected)?;
                return Ok(resp);
            }
            Err(error) => error,
        };
        failed += 1;
        if !error.is_retryable() || failed >= max_attempts || cancel.is_cancelled() {
            return Err(error);
        }
        let delay = backoff_delay(failed - 1, &pause.policy, error.retry_after(), jitter01());
        if !pause.extend(delay) {
            tracing::debug!(%error, ?delay, "sub-batch backoff budget exhausted; not retrying");
            return Err(error);
        }
        tracing::debug!(%error, ?delay, attempt = failed + 1, "retrying embedding sub-batch");
    }
}

/// Embed `req` through `provider`, splitting into sub-batches when it exceeds
/// the provider's `max_batch_size`. Cancellation propagates to every in-flight
/// sub-batch. A failed sub-batch is retried on its own with the default
/// [`SubBatchRetry`].
///
/// # Errors
/// The provider's error for the first sub-batch that fails for good, or
/// [`ProviderError::Translation`] when a response does not match its inputs.
pub async fn embed_batched(
    provider: &dyn EmbeddingProvider,
    req: EmbedRequest,
    cancel: &CancellationToken,
    concurrency: usize,
) -> Result<EmbedResponse, ProviderError> {
    embed_batched_with(provider, req, cancel, concurrency, SubBatchRetry::default()).await
}

/// [`embed_batched`] with an explicit sub-batch retry policy.
///
/// A request that fits in one upstream call is never retried here: nothing
/// would be saved, and the request-level retry already covers it.
///
/// # Errors
/// See [`embed_batched`].
pub async fn embed_batched_with(
    provider: &dyn EmbeddingProvider,
    req: EmbedRequest,
    cancel: &CancellationToken,
    concurrency: usize,
    retry: SubBatchRetry,
) -> Result<EmbedResponse, ProviderError> {
    let max_batch = provider.max_batch_size().max(1);

    // Fast path: within a single upstream call, keep the original input shape.
    if req.input.len() <= max_batch {
        let expected = req.input.len();
        let resp = provider.embed(req, cancel.clone()).await?;
        check_embedding_count(resp.data.len(), expected)?;
        return Ok(resp);
    }

    // Consume the request, MOVING the input items into sub-batches rather
    // than cloning them (the payload can be large - pillar 1: no avoidable
    // copies on the request path). Request-level options are small and cloned.
    let EmbedRequest {
        model,
        input,
        encoding_format,
        dimensions,
        user,
        extra,
    } = req;
    let total_inputs = input.len();

    // Split the input into `max_batch`-sized chunks, preserving the variant so
    // each sub-request keeps its shape (text batch stays a text batch, a
    // multimodal batch stays multimodal).
    let sub_inputs: Vec<EmbedInput> = match input {
        // Single-item shapes never reach here (len == 1 <= max_batch fast path),
        // but keep the arms total and correct.
        EmbedInput::Single(s) => vec![EmbedInput::Single(s)],
        EmbedInput::Tokens(ids) => vec![EmbedInput::Tokens(ids)],
        EmbedInput::Batch(v) => chunk_vec(v, max_batch).map(EmbedInput::Batch).collect(),
        EmbedInput::TokenBatch(v) => chunk_vec(v, max_batch)
            .map(EmbedInput::TokenBatch)
            .collect(),
        EmbedInput::Multi(v) => chunk_vec(v, max_batch).map(EmbedInput::Multi).collect(),
    };

    // `extra` (e.g. Cohere's `input_type` override) must reach every sub-batch
    // identically, or a caller's override would silently apply to only the
    // first chunk of a request that spills past `max_batch_size`.
    let sub_requests: Vec<EmbedRequest> = sub_inputs
        .into_iter()
        .map(|input| EmbedRequest {
            model: model.clone(),
            input,
            encoding_format: encoding_format.clone(),
            dimensions,
            user: user.clone(),
            extra: extra.clone(),
        })
        .collect();

    let concurrency = concurrency.max(1).min(sub_requests.len());

    // `buffered` preserves stream order; `try_collect` short-circuits on the
    // first error, dropping (cancelling) the remaining in-flight sub-batches.
    // A sub-batch retries inside its own future, so it keeps its slot while
    // it waits: retries never add to the `concurrency` calls in flight.
    let pause = Pause::new(retry);
    let pause = &pause;
    let responses: Vec<EmbedResponse> = stream::iter(sub_requests)
        .map(|sub| embed_sub_batch(provider, sub, cancel, pause))
        .buffered(concurrency)
        .try_collect()
        .await?;

    reassemble(responses, total_inputs, &model)
}

/// Put one sub-response's vectors in input order using the upstream `index`.
///
/// The upstream `index` (not the position in `data`) says which input a vector
/// belongs to, so a sub-response is sorted by it before the global re-index.
/// The indices must then be exactly `0..n`: a duplicate or a gap means the
/// vectors cannot be matched to their inputs, which is the upstream's fault
/// (a 502-class [`ProviderError::Translation`], LM-3002). The common case
/// (already in order) is a single pass with no sort.
fn order_by_upstream_index(data: &mut [EmbedData]) -> Result<(), ProviderError> {
    let in_order = |data: &[EmbedData]| {
        data.iter()
            .enumerate()
            .all(|(position, item)| item.index as usize == position)
    };
    if in_order(data) {
        return Ok(());
    }
    data.sort_unstable_by_key(|item| item.index);
    if in_order(data) {
        Ok(())
    } else {
        Err(ProviderError::Translation(format!(
            "embedding index values are not a permutation of 0..{}",
            data.len()
        )))
    }
}

/// Reject a response whose embedding count does not match the number of inputs
/// it was asked to embed. A short (or long) response is the upstream's fault,
/// so it maps to a 502-class [`ProviderError::Translation`] (LM-3002), never a
/// silent short result (issue #89).
fn check_embedding_count(returned: usize, expected: usize) -> Result<(), ProviderError> {
    if returned != expected {
        return Err(ProviderError::Translation(format!(
            "embedding count {returned} != input count {expected}"
        )));
    }
    Ok(())
}

/// Split a vector into consecutive chunks of at most `size` items, moving the
/// elements (no clone). `size` is assumed non-zero (callers pass `max(1)`).
fn chunk_vec<T>(items: Vec<T>, size: usize) -> impl Iterator<Item = Vec<T>> {
    let mut iter = items.into_iter();
    std::iter::from_fn(move || {
        let chunk: Vec<T> = iter.by_ref().take(size).collect();
        (!chunk.is_empty()).then_some(chunk)
    })
}

/// Concatenate sub-responses in order, re-index globally, and sum usage.
/// Within a sub-response the upstream `index` decides the order (see
/// [`order_by_upstream_index`]), never the arrival position.
///
/// # Errors
/// [`ProviderError::Translation`] when a sub-response's indices are not a
/// permutation of `0..n`.
///
/// Usage is summed ONLY when every sub-batch reported upstream usage. If any
/// sub-batch reported none (`prompt_tokens == 0`, e.g. Gemini's optional
/// `usageMetadata` absent on one chunk), the summed total would be a silent
/// undercount that the server then treats as exact. Instead the whole sum is
/// zeroed, so the server falls back to the flagged local estimate for the full
/// input, per ADR 003 (issue #89).
fn reassemble(
    responses: Vec<EmbedResponse>,
    total_inputs: usize,
    requested_model: &str,
) -> Result<EmbedResponse, ProviderError> {
    let mut data = Vec::with_capacity(total_inputs);
    let mut usage = EmbedUsage::default();
    let mut model = String::new();
    let mut next_index: u32 = 0;

    // Every sub-batch must have reported usage for the sum to be exact.
    let all_reported = responses.iter().all(|r| r.usage.prompt_tokens > 0);

    for mut resp in responses {
        order_by_upstream_index(&mut resp.data)?;
        if model.is_empty() {
            model = resp.model;
        }
        usage.prompt_tokens = usage.prompt_tokens.saturating_add(resp.usage.prompt_tokens);
        usage.total_tokens = usage.total_tokens.saturating_add(resp.usage.total_tokens);
        for mut item in resp.data {
            item.index = next_index;
            next_index = next_index.saturating_add(1);
            data.push(item);
        }
    }

    if !all_reported {
        // Partial or absent upstream usage: drop it so the server's
        // `prompt_tokens > 0` check fails and the flagged estimate wins.
        usage.prompt_tokens = 0;
        usage.total_tokens = 0;
    }

    if model.is_empty() {
        requested_model.clone_into(&mut model);
    }

    Ok(EmbedResponse {
        object: "list".to_owned(),
        data,
        model,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lumen_core::EmbeddingEncoding;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A configurable embedding provider that never touches the network.
    struct MockProvider {
        max_batch: usize,
        /// Return one FEWER vector than asked (a short upstream response).
        short: bool,
        /// `usage.prompt_tokens` to report per call, in call order; `0` means
        /// "reported no usage". Missing entries default to `0`.
        usage: Vec<u32>,
        /// How each response's `data` is laid out relative to its inputs.
        layout: Layout,
        calls: AtomicUsize,
    }

    /// Shape of the mock's `data` array. Each vector is `[input as f32]`, so a
    /// test can tell which input a returned vector belongs to.
    #[derive(Clone, Copy)]
    enum Layout {
        /// In input order, `index` == position.
        InOrder,
        /// Correct `index` on every entry, but the array is reversed.
        Reversed,
        /// Every entry claims `index: 0`.
        AllZero,
    }

    impl MockProvider {
        fn new(max_batch: usize, short: bool, usage: Vec<u32>) -> Self {
            Self {
                max_batch,
                short,
                usage,
                layout: Layout::InOrder,
                calls: AtomicUsize::new(0),
            }
        }

        fn with_layout(mut self, layout: Layout) -> Self {
            self.layout = layout;
            self
        }
    }

    #[async_trait]
    impl EmbeddingProvider for MockProvider {
        async fn embed(
            &self,
            req: EmbedRequest,
            _cancel: CancellationToken,
        ) -> Result<EmbedResponse, ProviderError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let asked = req.input.len();
            let n = if self.short {
                asked.saturating_sub(1)
            } else {
                asked
            };
            let mut data: Vec<EmbedData> = req
                .input
                .iter()
                .take(n)
                .enumerate()
                .map(|(i, text)| EmbedData {
                    object: "embedding".to_owned(),
                    index: match self.layout {
                        Layout::AllZero => 0,
                        Layout::InOrder | Layout::Reversed => u32::try_from(i).unwrap_or(u32::MAX),
                    },
                    embedding: vec![text.parse::<f32>().unwrap_or(f32::NAN)],
                    encoding: EmbeddingEncoding::default(),
                })
                .collect();
            if matches!(self.layout, Layout::Reversed) {
                data.reverse();
            }
            let prompt = self.usage.get(call).copied().unwrap_or(0);
            Ok(EmbedResponse {
                object: "list".to_owned(),
                data,
                model: req.model,
                usage: EmbedUsage {
                    prompt_tokens: prompt,
                    total_tokens: prompt,
                    estimated: None,
                },
            })
        }

        fn max_batch_size(&self) -> usize {
            self.max_batch
        }
    }

    fn batch_of(n: usize) -> EmbedRequest {
        EmbedRequest {
            model: "m".to_owned(),
            input: EmbedInput::Batch((0..n).map(|i| i.to_string()).collect()),
            encoding_format: None,
            dimensions: None,
            user: None,
            extra: serde_json::Map::new(),
        }
    }

    /// A single-input provider (like Bedrock or Vertex) whose calls fail as
    /// scripted per input text, and which records what it was asked.
    struct FlakyProvider {
        /// Input text -> errors to return, in order, before succeeding.
        failures: Mutex<std::collections::HashMap<String, Vec<ProviderError>>>,
        /// Input text of every call, in call order.
        calls: Mutex<Vec<String>>,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
    }

    impl FlakyProvider {
        fn new(failures: Vec<(&str, Vec<ProviderError>)>) -> Self {
            Self {
                failures: Mutex::new(
                    failures
                        .into_iter()
                        .map(|(input, errors)| (input.to_owned(), errors))
                        .collect(),
                ),
                calls: Mutex::new(Vec::new()),
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
            }
        }

        fn calls_for(&self, input: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| *c == input)
                .count()
        }

        fn total_calls(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl EmbeddingProvider for FlakyProvider {
        async fn embed(
            &self,
            req: EmbedRequest,
            _cancel: CancellationToken,
        ) -> Result<EmbedResponse, ProviderError> {
            let text = req.input.iter().next().unwrap_or_default().to_owned();
            self.calls.lock().unwrap().push(text.clone());
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            // Stay in flight long enough for concurrent calls to overlap.
            tokio::time::sleep(Duration::from_millis(2)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);

            let scripted = {
                let mut failures = self.failures.lock().unwrap();
                match failures.get_mut(&text) {
                    Some(errors) if !errors.is_empty() => Some(errors.remove(0)),
                    _ => None,
                }
            };
            if let Some(error) = scripted {
                return Err(error);
            }
            Ok(EmbedResponse {
                object: "list".to_owned(),
                data: vec![EmbedData {
                    object: "embedding".to_owned(),
                    index: 0,
                    embedding: vec![text.parse::<f32>().unwrap_or(f32::NAN)],
                    encoding: EmbeddingEncoding::default(),
                }],
                model: req.model,
                usage: EmbedUsage {
                    prompt_tokens: 1,
                    total_tokens: 1,
                    estimated: None,
                },
            })
        }

        fn max_batch_size(&self) -> usize {
            1
        }
    }

    fn rate_limited(retry_after: Option<Duration>) -> ProviderError {
        ProviderError::RateLimited {
            provider: "p".to_owned(),
            retry_after,
        }
    }

    fn upstream(status: u16, retryable: bool) -> ProviderError {
        ProviderError::Upstream {
            provider: "p".to_owned(),
            status,
            retryable,
            retry_after: None,
        }
    }

    /// Millisecond-scale policy so the tests run on the real clock.
    fn fast_retry() -> SubBatchRetry {
        SubBatchRetry {
            max_attempts: 3,
            base: Duration::from_millis(2),
            max: Duration::from_millis(20),
            max_total_backoff: Duration::from_millis(500),
        }
    }

    #[tokio::test]
    #[allow(clippy::float_cmp)] // the mock echoes small integers exactly
    async fn rate_limited_sub_batch_is_retried_alone() {
        // 10 single-input calls; input 7 is rate limited once. Only that call
        // is sent again: the nine completed (billed) results are kept.
        let provider = FlakyProvider::new(vec![("7", vec![rate_limited(None)])]);
        let resp = embed_batched_with(
            &provider,
            batch_of(10),
            &CancellationToken::new(),
            4,
            fast_retry(),
        )
        .await
        .unwrap();

        assert_eq!(provider.total_calls(), 11);
        assert_eq!(provider.calls_for("7"), 2);
        for other in ["0", "1", "2", "3", "4", "5", "6", "8", "9"] {
            assert_eq!(provider.calls_for(other), 1, "input {other} was re-sent");
        }
        let values: Vec<f32> = resp.data.iter().map(|d| d.embedding[0]).collect();
        assert_eq!(values, (0u8..10).map(f32::from).collect::<Vec<_>>());
        assert_eq!(resp.usage.prompt_tokens, 10);
    }

    #[tokio::test]
    async fn retryable_upstream_error_is_retried_up_to_the_attempt_cap() {
        // Two 503s then success: three calls, the cap, and the request passes.
        let provider =
            FlakyProvider::new(vec![("1", vec![upstream(503, true), upstream(503, true)])]);
        let resp = embed_batched_with(
            &provider,
            batch_of(3),
            &CancellationToken::new(),
            4,
            fast_retry(),
        )
        .await
        .unwrap();
        assert_eq!(resp.data.len(), 3);
        assert_eq!(provider.calls_for("1"), 3);

        // Three failures: the third is returned, with no fourth call.
        let provider = FlakyProvider::new(vec![(
            "1",
            vec![
                upstream(503, true),
                upstream(503, true),
                upstream(502, true),
                upstream(500, true),
            ],
        )]);
        let err = embed_batched_with(
            &provider,
            batch_of(3),
            &CancellationToken::new(),
            4,
            fast_retry(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ProviderError::Upstream { status: 502, .. }),
            "{err:?}"
        );
        assert_eq!(provider.calls_for("1"), 3);
    }

    #[tokio::test]
    async fn client_errors_are_never_retried() {
        for error in [
            upstream(400, false),
            ProviderError::Translation("bad".to_owned()),
            ProviderError::ContextLengthExceeded {
                provider: "p".to_owned(),
                status: 400,
            },
        ] {
            let provider = FlakyProvider::new(vec![("1", vec![error])]);
            let err = embed_batched_with(
                &provider,
                batch_of(3),
                &CancellationToken::new(),
                1,
                fast_retry(),
            )
            .await
            .unwrap_err();
            assert!(!err.is_retryable(), "{err:?}");
            assert_eq!(provider.calls_for("1"), 1, "{err:?} was retried");
        }
    }

    #[tokio::test]
    async fn retry_after_hint_is_honoured() {
        let hint = Duration::from_millis(60);
        let provider = FlakyProvider::new(vec![("1", vec![rate_limited(Some(hint))])]);
        let started = std::time::Instant::now();
        embed_batched_with(
            &provider,
            batch_of(3),
            &CancellationToken::new(),
            4,
            fast_retry(),
        )
        .await
        .unwrap();
        assert!(started.elapsed() >= hint, "{:?}", started.elapsed());
        assert_eq!(provider.calls_for("1"), 2);
    }

    #[tokio::test]
    async fn a_wait_above_the_backoff_budget_is_not_started() {
        // The upstream asks for 5 s; the request may wait 500 ms in total.
        // The error comes back at once, hint intact, for the layer above.
        let hint = Duration::from_secs(5);
        let provider = FlakyProvider::new(vec![("1", vec![rate_limited(Some(hint))])]);
        let started = std::time::Instant::now();
        let err = embed_batched_with(
            &provider,
            batch_of(3),
            &CancellationToken::new(),
            4,
            fast_retry(),
        )
        .await
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(err.retry_after(), Some(hint));
        assert_eq!(provider.calls_for("1"), 1);
    }

    #[tokio::test]
    async fn total_backoff_is_capped_across_sub_batches() {
        // Every input is rate limited with a 40 ms hint, one call at a time,
        // and the request may wait 100 ms in total: two waits fit, the third
        // does not, so the third rate-limited input fails the request.
        let inputs = ["0", "1", "2", "3", "4", "5"];
        let provider = FlakyProvider::new(
            inputs
                .iter()
                .map(|i| (*i, vec![rate_limited(Some(Duration::from_millis(40)))]))
                .collect(),
        );
        let policy = SubBatchRetry {
            max_total_backoff: Duration::from_millis(100),
            ..fast_retry()
        };
        let started = std::time::Instant::now();
        let err = embed_batched_with(
            &provider,
            batch_of(inputs.len()),
            &CancellationToken::new(),
            1,
            policy,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ProviderError::RateLimited { .. }), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(1));
        // Inputs 0 and 1: failed then retried. Input 2: failed, not retried.
        assert_eq!(provider.total_calls(), 5);
    }

    #[tokio::test]
    async fn a_retry_storm_never_exceeds_the_concurrency() {
        // Every one of 12 sub-batches fails once: 24 calls, never more than
        // 3 in flight.
        let inputs: Vec<String> = (0..12).map(|i| i.to_string()).collect();
        let provider = FlakyProvider::new(
            inputs
                .iter()
                .map(|i| (i.as_str(), vec![rate_limited(None)]))
                .collect(),
        );
        let resp = embed_batched_with(
            &provider,
            batch_of(12),
            &CancellationToken::new(),
            3,
            fast_retry(),
        )
        .await
        .unwrap();
        assert_eq!(resp.data.len(), 12);
        assert_eq!(provider.total_calls(), 24);
        assert!(
            provider.max_in_flight.load(Ordering::SeqCst) <= 3,
            "in flight: {}",
            provider.max_in_flight.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn cancellation_interrupts_the_backoff() {
        let policy = SubBatchRetry {
            max_total_backoff: Duration::from_secs(30),
            ..fast_retry()
        };
        let provider = FlakyProvider::new(vec![(
            "0",
            vec![rate_limited(Some(Duration::from_secs(20)))],
        )]);
        let cancel = CancellationToken::new();
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            canceller.cancel();
        });
        let started = std::time::Instant::now();
        let err = embed_batched_with(&provider, batch_of(3), &cancel, 1, policy)
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Cancelled), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
        // Nothing was sent after the cancellation.
        assert_eq!(provider.total_calls(), 1);
    }

    #[tokio::test]
    async fn never_policy_keeps_the_single_attempt_behaviour() {
        let provider = FlakyProvider::new(vec![("1", vec![rate_limited(None)])]);
        let err = embed_batched_with(
            &provider,
            batch_of(3),
            &CancellationToken::new(),
            1,
            SubBatchRetry::never(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ProviderError::RateLimited { .. }), "{err:?}");
        assert_eq!(provider.calls_for("1"), 1);
    }

    #[test]
    fn backoff_matches_the_request_level_formula() {
        let p = SubBatchRetry::default();
        // Equal jitter: [exp/2, exp], exponential, capped at `max`.
        assert_eq!(backoff_delay(0, &p, None, 0.0), Duration::from_millis(100));
        assert_eq!(backoff_delay(0, &p, None, 1.0), Duration::from_millis(200));
        assert_eq!(backoff_delay(1, &p, None, 0.0), Duration::from_millis(200));
        assert_eq!(backoff_delay(20, &p, None, 1.0), Duration::from_secs(5));
        // `Retry-After` is a floor.
        assert_eq!(
            backoff_delay(0, &p, Some(Duration::from_secs(3)), 0.0),
            Duration::from_secs(3)
        );
    }

    #[tokio::test]
    async fn single_call_length_mismatch_is_a_translation_error() {
        // 3 inputs <= max_batch 10: the fast path. A short response must not be
        // accepted silently (issue #89).
        let provider = MockProvider::new(10, true, vec![1]);
        let err = embed_batched(&provider, batch_of(3), &CancellationToken::new(), 4)
            .await
            .unwrap_err();
        match err {
            ProviderError::Translation(msg) => {
                assert!(
                    msg.contains("embedding count 2 != input count 3"),
                    "got: {msg}"
                );
            }
            other => panic!("expected Translation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reassembled_length_mismatch_is_a_translation_error() {
        // 5 inputs, max_batch 2 => 3 sub-batches; a short sub-batch fails the
        // whole request rather than reassembling with an index gap (issue #89).
        let provider = MockProvider::new(2, true, vec![1, 1, 1]);
        let err = embed_batched(&provider, batch_of(5), &CancellationToken::new(), 4)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::Translation(_)),
            "expected Translation, got {err:?}"
        );
    }

    #[tokio::test]
    async fn mixed_usage_zeroes_the_sum_for_the_estimate_fallback() {
        // 3 inputs, max_batch 1 => 3 sub-batches; the middle reports no usage.
        // The partial sum must be dropped so the server falls back to the
        // flagged local estimate (issue #89 / ADR 003).
        let provider = MockProvider::new(1, false, vec![5, 0, 7]);
        let resp = embed_batched(&provider, batch_of(3), &CancellationToken::new(), 4)
            .await
            .unwrap();
        assert_eq!(resp.data.len(), 3);
        assert_eq!(resp.usage.prompt_tokens, 0);
        assert_eq!(resp.usage.total_tokens, 0);
        // reassemble never flags the estimate itself; the server does.
        assert_eq!(resp.usage.estimated, None);
    }

    #[tokio::test]
    #[allow(clippy::float_cmp)] // the mock echoes small integers exactly
    async fn sub_batches_are_reassembled_by_upstream_index_not_arrival_order() {
        // 5 inputs, max_batch 2 => [0,1] [2,3] [4]; every sub-response lists
        // its vectors in reverse with the right `index`. The vector at global
        // position i must be the one for input i.
        let provider = MockProvider::new(2, false, vec![1, 1, 1]).with_layout(Layout::Reversed);
        let resp = embed_batched(&provider, batch_of(5), &CancellationToken::new(), 4)
            .await
            .unwrap();
        let indices: Vec<u32> = resp.data.iter().map(|d| d.index).collect();
        assert_eq!(indices, vec![0, 1, 2, 3, 4]);
        let values: Vec<f32> = resp.data.iter().map(|d| d.embedding[0]).collect();
        assert_eq!(values, vec![0.0, 1.0, 2.0, 3.0, 4.0]);
    }

    #[tokio::test]
    async fn sub_batch_indices_that_are_not_a_permutation_are_a_translation_error() {
        // Each two-item sub-response claims `index: 0` twice: there is no way
        // to tell which vector belongs to which input.
        let provider = MockProvider::new(2, false, vec![1, 1]).with_layout(Layout::AllZero);
        let err = embed_batched(&provider, batch_of(4), &CancellationToken::new(), 4)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::Translation(ref m) if m.contains("index")),
            "expected Translation, got {err:?}"
        );
    }

    #[tokio::test]
    async fn full_usage_is_summed_when_every_sub_batch_reports() {
        let provider = MockProvider::new(1, false, vec![1, 2, 3]);
        let resp = embed_batched(&provider, batch_of(3), &CancellationToken::new(), 4)
            .await
            .unwrap();
        assert_eq!(resp.data.len(), 3);
        assert_eq!(resp.usage.prompt_tokens, 6);
        assert_eq!(resp.usage.total_tokens, 6);
    }
}
