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
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
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

    /// Perform a streaming chat completion, yielding typed chunks as they arrive.
    ///
    /// The returned stream is `'static` so it can be handed directly to the SSE
    /// responder without borrowing the provider. This is the path translating
    /// providers (Anthropic, Gemini) implement; passthrough providers instead
    /// override [`chat_stream_bytes`](ChatProvider::chat_stream_bytes).
    async fn chat_stream(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<ChatChunk, ProviderError>>, ProviderError>;

    /// Stream the complete SSE response body as raw bytes: `data: {json}\n\n`
    /// frames including the terminal `data: [DONE]\n\n`.
    ///
    /// The default adapts [`chat_stream`](ChatProvider::chat_stream) by
    /// serializing each typed chunk — correct for translating providers.
    /// Passthrough providers (whose upstream already speaks OpenAI SSE) override
    /// this to forward upstream bytes verbatim with no per-chunk `serde` round
    /// trip (zero-copy; see ADR 004). Errors before the first frame surface as
    /// `Err`; a mid-stream failure is emitted as an SSE error frame by the
    /// server. See ADR 004 for the design.
    async fn chat_stream_bytes(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<Bytes, ProviderError>>, ProviderError> {
        let chunks = self.chat_stream(req, cancel).await?;
        let framed = chunks.map(|item| {
            item.map(|chunk| {
                let json = serde_json::to_string(&chunk).unwrap_or_else(|_| "{}".to_owned());
                Bytes::from(format!("data: {json}\n\n"))
            })
        });
        let done = stream::once(async { Ok(Bytes::from_static(b"data: [DONE]\n\n")) });
        Ok(framed.chain(done).boxed())
    }

    /// Whether this provider can accept a remote (`http(s)`) image URL in a
    /// content part. Providers that only accept inline base64 image bytes
    /// (Gemini) return `false`, so the gateway rejects a remote URL with
    /// `LM-2004` rather than forwarding one the upstream cannot fetch.
    fn accepts_remote_image_url(&self) -> bool {
        true
    }
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
