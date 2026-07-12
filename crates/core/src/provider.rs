//! Capability traits implemented by providers.
//!
//! Every method that performs an upstream call takes a [`CancellationToken`].
//! When the token fires (because the downstream client disconnected), the
//! provider MUST abort its in-flight HTTP request so the upstream model stops
//! generating — dropping the response body is not enough (lesson: LiteLLM
//! issue #22805).

use crate::chat::{ChatChunk, ChatRequest, ChatResponse};
use crate::embed::{EmbedRequest, EmbedResponse};
use crate::error::ProviderError;
use crate::rerank::{RerankRequest, RerankResponse};
use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

/// A provider that can serve chat completions.
#[async_trait]
pub trait ChatProvider: Send + Sync {
    /// Perform a non-streaming chat completion.
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, ProviderError>;

    /// Perform a streaming chat completion, yielding chunks as they arrive.
    ///
    /// The returned stream is `'static` so it can be handed directly to the SSE
    /// responder without borrowing the provider.
    async fn chat_stream(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<ChatChunk, ProviderError>>, ProviderError>;
}

/// A provider that can produce text embeddings.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed one or more inputs. Batches larger than [`max_batch_size`] are
    /// split and reassembled in order by the router, not the provider.
    ///
    /// [`max_batch_size`]: EmbeddingProvider::max_batch_size
    async fn embed(
        &self,
        req: EmbedRequest,
        cancel: CancellationToken,
    ) -> Result<EmbedResponse, ProviderError>;

    /// Maximum number of inputs the upstream accepts in a single request.
    fn max_batch_size(&self) -> usize;
}

/// A provider that can rerank documents against a query.
#[async_trait]
pub trait RerankProvider: Send + Sync {
    /// Score and order `documents` by relevance to `query`.
    async fn rerank(
        &self,
        req: RerankRequest,
        cancel: CancellationToken,
    ) -> Result<RerankResponse, ProviderError>;
}
