//! Provider implementations for Ferrogate.
//!
//! Each provider lives in its own module and implements one or more of the
//! capability traits from [`ferrogate_core`] ([`ChatProvider`], [`EmbeddingProvider`],
//! [`RerankProvider`]). Providers are added incrementally starting at milestone M2.
//!
//! [`ChatProvider`]: ferrogate_core::ChatProvider
//! [`EmbeddingProvider`]: ferrogate_core::EmbeddingProvider
//! [`RerankProvider`]: ferrogate_core::RerankProvider
