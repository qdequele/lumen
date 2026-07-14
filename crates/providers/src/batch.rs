//! Automatic batching for embedding requests.
//!
//! When a request has more inputs than the provider's `max_batch_size`, it is
//! split into sub-batches run with bounded concurrency, then reassembled **in
//! the original order** with summed token usage. A single sub-batch failure
//! fails the whole request (no partial results in v1).

use futures::stream::{self, StreamExt, TryStreamExt};
use lumen_core::ProviderError;
use lumen_core::{EmbedInput, EmbedRequest, EmbedResponse, EmbedUsage, EmbeddingProvider};
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
        return provider.embed(req, cancel.clone()).await;
    }

    // Consume the request, MOVING the input items into sub-batches rather
    // than cloning them (the payload can be large — pillar 1: no avoidable
    // copies on the request path). Request-level options are small and cloned.
    let EmbedRequest {
        model,
        input,
        encoding_format,
        dimensions,
        user,
    } = req;
    let total_inputs = input.len();

    // Split the input into `max_batch`-sized chunks, preserving the variant so
    // each sub-request keeps its shape (text batch stays a text batch, a
    // multimodal batch stays multimodal).
    let sub_inputs: Vec<EmbedInput> = match input {
        // A single item never reaches here (len == 1 <= max_batch fast path),
        // but keep the arm total and correct.
        EmbedInput::Single(s) => vec![EmbedInput::Single(s)],
        EmbedInput::Batch(v) => chunk_vec(v, max_batch).map(EmbedInput::Batch).collect(),
        EmbedInput::Multi(v) => chunk_vec(v, max_batch).map(EmbedInput::Multi).collect(),
    };

    let sub_requests: Vec<EmbedRequest> = sub_inputs
        .into_iter()
        .map(|input| EmbedRequest {
            model: model.clone(),
            input,
            encoding_format: encoding_format.clone(),
            dimensions,
            user: user.clone(),
        })
        .collect();

    let concurrency = concurrency.max(1).min(sub_requests.len());

    // `buffered` preserves stream order; `try_collect` short-circuits on the
    // first error, dropping (cancelling) the remaining in-flight sub-batches.
    let responses: Vec<EmbedResponse> = stream::iter(sub_requests)
        .map(|sub| provider.embed(sub, cancel.clone()))
        .buffered(concurrency)
        .try_collect()
        .await?;

    Ok(reassemble(responses, total_inputs, &model))
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
fn reassemble(
    responses: Vec<EmbedResponse>,
    total_inputs: usize,
    requested_model: &str,
) -> EmbedResponse {
    let mut data = Vec::with_capacity(total_inputs);
    let mut usage = EmbedUsage::default();
    let mut model = String::new();
    let mut next_index: u32 = 0;

    for resp in responses {
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

    if model.is_empty() {
        requested_model.clone_into(&mut model);
    }

    EmbedResponse {
        object: "list".to_owned(),
        data,
        model,
        usage,
    }
}
