//! TypeSafe provider - SystemOne typed decisions (Jev), ADR 013.
//!
//! TypeSafe exposes a single evaluation endpoint, `POST {base}/v1/systemone`,
//! whose request and response shapes are exactly LUMEN's public
//! `/v1/systemone` shape, so this provider forwards the request as-is (with
//! the model rewritten to the upstream id) and parses the response only far
//! enough to read `model` and `usage`; `state`, questions and answers stay
//! raw JSON end to end.
//!
//! Auth is a bearer key. Status handling is the shared classifier: `401` /
//! `422` are non-retryable upstream errors, `429` is rate limited (honouring
//! `Retry-After`), and TypeSafe's `529 Overloaded` is a retryable 5xx, so the
//! executor retries and falls back as it would for any overloaded upstream.

pub mod rerank;

use async_trait::async_trait;
use lumen_core::{ProviderError, SystemOneProvider, SystemOneRequest, SystemOneResponse};
use std::fmt;
use tokio_util::sync::CancellationToken;

use crate::http::post_json;

/// Default TypeSafe API base (no version in the path).
pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";

/// A TypeSafe provider serving SystemOne evaluations.
pub struct TypesafeProvider {
    client: reqwest::Client,
    provider_name: String,
    /// `{base}/v1/systemone`, precomputed once.
    url: String,
    /// The TypeSafe API key, sent as a bearer token. Redacted from `Debug`;
    /// never logged.
    api_key: Option<String>,
}

impl TypesafeProvider {
    /// Construct a provider. `base_url` defaults to the public TypeSafe API;
    /// an override is the API root (e.g. `https://api.typesafe.ai`), with or
    /// without a trailing `/v1`.
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        provider_name: impl Into<String>,
        base_url: Option<String>,
        api_key: Option<String>,
    ) -> Self {
        let base = base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        let base = base.trim_end_matches('/');
        let base = base.strip_suffix("/v1").unwrap_or(base);
        Self {
            client,
            provider_name: provider_name.into(),
            url: format!("{base}/v1/systemone"),
            api_key,
        }
    }
}

/// Redacted so the API key can never reach a log line via `{:?}`.
impl fmt::Debug for TypesafeProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TypesafeProvider")
            .field("provider_name", &self.provider_name)
            .field("url", &self.url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl SystemOneProvider for TypesafeProvider {
    async fn evaluate(
        &self,
        req: SystemOneRequest,
        cancel: CancellationToken,
    ) -> Result<SystemOneResponse, ProviderError> {
        let bytes = post_json(
            &self.client,
            &self.url,
            &req,
            self.api_key.as_deref(),
            &self.provider_name,
            &cancel,
        )
        .await?;

        serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("typesafe systemone response: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_leaks_the_api_key() {
        let provider = TypesafeProvider::new(
            reqwest::Client::new(),
            "typesafe",
            None,
            Some("ts-super-secret".to_owned()),
        );
        let debug = format!("{provider:?}");
        assert!(!debug.contains("ts-super-secret"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn base_url_is_normalized_to_the_systemone_endpoint() {
        for (base, expected) in [
            (None, "https://api.typesafe.ai/v1/systemone"),
            (
                Some("https://api.typesafe.ai/"),
                "https://api.typesafe.ai/v1/systemone",
            ),
            (
                Some("https://proxy.local/v1"),
                "https://proxy.local/v1/systemone",
            ),
            (
                Some("https://proxy.local/v1/"),
                "https://proxy.local/v1/systemone",
            ),
        ] {
            let provider =
                TypesafeProvider::new(reqwest::Client::new(), "t", base.map(str::to_owned), None);
            assert_eq!(provider.url, expected, "{base:?}");
        }
    }
}
