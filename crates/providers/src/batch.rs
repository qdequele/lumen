//! Automatic batching for embedding requests.
//!
//! When a request has more inputs than the provider's `max_batch_size`, it is
//! split into sub-batches run with bounded concurrency, then reassembled **in
//! the original order** with summed token usage. A single sub-batch failure
//! fails the whole request (no partial results in v1).

use futures::stream::{self, StreamExt, TryStreamExt};
use lumen_core::ProviderError;
use lumen_core::{
    EmbedData, EmbedInput, EmbedRequest, EmbedResponse, EmbedUsage, EmbeddingProvider,
};
use tokio_util::sync::CancellationToken;

/// Default number of sub-batches sent concurrently.
pub const DEFAULT_CONCURRENCY: usize = 4;

/// Embed `req` through `provider`, splitting into sub-batches when it exceeds
/// the provider's `max_batch_size`. Cancellation propagates to every in-flight
/// sub-batch.
pub async fn embed_batched(
    provider: &dyn EmbeddingProvider,
    req: EmbedRequest,
    cancel: &CancellationToken,
    concurrency: usize,
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
    // Each sub-batch verifies its own returned vector count against the count
    // it asked for, so a short upstream response is rejected (never silently
    // padded with an index gap) before reassembly (issue #89).
    let responses: Vec<EmbedResponse> = stream::iter(sub_requests)
        .map(|sub| {
            let expected = sub.input.len();
            let cancel = cancel.clone();
            async move {
                let resp = provider.embed(sub, cancel).await?;
                check_embedding_count(resp.data.len(), expected)?;
                Ok::<EmbedResponse, ProviderError>(resp)
            }
        })
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
