//! Provider implementations for Ferrogate.
//!
//! Each provider lives in its own module and implements one or more of the
//! capability traits from [`ferrogate_core`]. The [`registry`] builds concrete
//! instances from config-derived specs and resolves `(capability, model)` to a
//! provider; [`batch`] splits oversized embedding requests across sub-batches.
//!
//! OpenAI ([`openai`]) is the canonical reference; new providers follow its
//! shape and must pass the shared conformance suite (see the crate's tests).

#![forbid(unsafe_code)]

pub mod batch;
pub mod http;
pub mod kind;
pub mod mapping;
pub mod ollama;
pub mod openai;
pub mod registry;

pub use batch::embed_batched;
pub use kind::ProviderKind;
pub use ollama::OllamaProvider;
pub use openai::OpenAiProvider;
pub use registry::{EmbeddingRoute, ModelSpec, ProviderSpec, Registry, RegistryError};
