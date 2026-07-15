//! Provider implementations for LUMEN.
//!
//! Each provider lives in its own module and implements one or more of the
//! capability traits from [`lumen_core`]. The [`registry`] builds concrete
//! instances from config-derived specs and resolves `(capability, model)` to a
//! provider; [`batch`] splits oversized embedding requests across sub-batches.
//!
//! OpenAI ([`openai`]) is the canonical reference; new providers follow its
//! shape and must pass the shared conformance suite (see the crate's tests).

#![forbid(unsafe_code)]

pub mod anthropic;
pub mod azure;
pub mod batch;
pub mod chat;
pub mod cloudflare;
pub mod cohere;
pub mod google;
pub mod http;
pub mod image_fetch;
pub mod jina;
pub mod kind;
pub mod mapping;
pub mod mistral;
pub mod mixedbread;
pub mod nvidia;
pub mod ollama;
pub mod openai;
pub mod pinecone;
pub mod registry;
pub mod rerank;
pub mod sse;
pub mod tei;
pub mod together;
pub mod voyage;

pub use anthropic::AnthropicProvider;
pub use azure::AzureProvider;
pub use batch::embed_batched;
pub use cloudflare::CloudflareRerankProvider;
pub use cohere::CohereProvider;
pub use google::vertex::VertexProvider;
pub use google::GoogleProvider;
pub use jina::JinaProvider;
pub use kind::ProviderKind;
pub use mistral::MistralProvider;
pub use mixedbread::MixedbreadProvider;
pub use nvidia::NvidiaProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenAiProvider;
pub use pinecone::PineconeProvider;
pub use registry::{
    ChatRoute, EmbeddingRoute, LoadedModelSummary, ModelSpec, ProviderSpec, Registry,
    RegistryError, RerankRoute,
};
pub use tei::TeiProvider;
pub use together::TogetherRerankProvider;
pub use voyage::VoyageProvider;
