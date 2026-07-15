//! Google Vertex AI chat provider (regional endpoints, GCP OAuth).
//!
//! Vertex AI serves the same `GenerateContent` wire schema as the public Gemini
//! Developer API, so this provider reuses the request/response and streaming
//! translation from the parent [`google`](super) module verbatim. The delta is
//! entirely in the transport layer:
//!
//! * endpoints are regional and path-scoped to a GCP project:
//!   `https://{location}-aiplatform.googleapis.com/v1/projects/{project}/`
//!   `locations/{location}/publishers/google/models/{model}:generateContent`
//!   (and `:streamGenerateContent?alt=sse` for SSE);
//! * auth is an OAuth2 `Bearer` token minted from a service-account key, not a
//!   static API key (see [`auth`]). The token is cached in memory and refreshed
//!   before expiry, so it never sits on the per-request hot path.
//!
//! A provider whose credentials env var is unset still *builds* (matching how
//! every other provider boots without its key) - each request then fails with
//! a provider-named upstream error, never a misleading gateway 401.
//!
//! Like Gemini, Vertex accepts only inline base64 image bytes, so
//! [`accepts_remote_image_url`](ChatProvider::accepts_remote_image_url) is
//! `false`.

mod auth;

use std::fmt;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lumen_core::{ChatChunk, ChatProvider, ChatRequest, ChatResponse, ProviderError};
use tokio_util::sync::CancellationToken;

use self::auth::{ServiceAccountKey, TokenSource};
use super::stream::GoogleTranslator;
use super::{translate_request, translate_response, GeminiResponse};
use crate::chat::{items_to_chunks, items_to_sse_bytes, translate_sse_stream, StreamItem};
use crate::http::{open_stream, post_json};

/// A Google Vertex AI chat provider.
pub struct VertexProvider {
    client: reqwest::Client,
    provider_name: String,
    /// `None` when no credentials were configured: the provider builds (so the
    /// gateway still boots, like every other provider missing its key) but all
    /// requests fail with a provider-named upstream error.
    state: Option<Ready>,
}

/// The fully-configured state: resolved endpoints plus a token source.
struct Ready {
    /// GCP project id that scopes the model path.
    project_id: String,
    /// GCP region (e.g. `us-central1`) that scopes both the host and the path.
    location: String,
    /// The aiplatform endpoint base, e.g.
    /// `https://us-central1-aiplatform.googleapis.com`.
    endpoint_base: String,
    /// Mints and caches OAuth2 access tokens (private key redacted inside).
    auth: TokenSource,
}

impl Ready {
    /// Build the regional, project-scoped model URL for the given method.
    fn model_url(&self, model: &str, streaming: bool) -> String {
        let method = if streaming {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        };
        format!(
            "{base}/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:{method}",
            base = self.endpoint_base,
            project = self.project_id,
            location = self.location,
        )
    }
}

impl fmt::Debug for VertexProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("VertexProvider");
        s.field("provider_name", &self.provider_name);
        match &self.state {
            Some(ready) => {
                s.field("project_id", &ready.project_id)
                    .field("location", &ready.location)
                    .field("endpoint_base", &ready.endpoint_base)
                    // TokenSource's own Debug redacts the private key.
                    .field("auth", &ready.auth);
            }
            None => {
                s.field("state", &"<no credentials configured>");
            }
        }
        s.finish_non_exhaustive()
    }
}

/// Failure while building a [`VertexProvider`] from configuration.
///
/// Only *deterministically wrong* configuration is a build error (garbage
/// credentials JSON, no region, no project id). A merely *absent* credentials
/// env var is not: the provider builds unconfigured and fails per request.
#[derive(Debug, thiserror::Error)]
pub enum VertexConfigError {
    /// The GCP region was not configured.
    #[error("Vertex AI provider '{name}' requires a GCP region (location)")]
    MissingLocation {
        /// The offending provider's name.
        name: String,
    },

    /// No GCP project id was configured or present in the credentials.
    #[error("Vertex AI provider '{name}' requires a GCP project id")]
    MissingProject {
        /// The offending provider's name.
        name: String,
    },

    /// The service-account credentials JSON could not be parsed. The message
    /// never echoes the input, so key material cannot leak through it.
    #[error("Vertex AI provider '{name}' has invalid service-account credentials")]
    InvalidCredentials {
        /// The offending provider's name.
        name: String,
    },
}

impl VertexProvider {
    /// Construct a Vertex AI provider.
    ///
    /// * `credentials_json` is the service-account key file contents (inline
    ///   JSON). Its `project_id` is used when `project_id` is not given, and its
    ///   `token_uri` is the OAuth endpoint (a wiremock URL in tests). `None`
    ///   builds an unconfigured provider whose requests fail with a
    ///   provider-named upstream error (the gateway still boots).
    /// * `location` is the GCP region (e.g. `us-central1`).
    /// * `endpoint_base` overrides the derived aiplatform host; when `None` the
    ///   standard `https://{location}-aiplatform.googleapis.com` is used.
    ///
    /// # Errors
    /// Returns a [`VertexConfigError`] if supplied credentials are unparseable,
    /// or the project id or region cannot be determined.
    pub fn new(
        client: reqwest::Client,
        provider_name: impl Into<String>,
        credentials_json: Option<&str>,
        project_id: Option<String>,
        location: Option<String>,
        endpoint_base: Option<String>,
    ) -> Result<Self, VertexConfigError> {
        let provider_name = provider_name.into();
        let Some(raw) = credentials_json else {
            return Ok(Self {
                client,
                provider_name,
                state: None,
            });
        };
        let key = ServiceAccountKey::from_json(raw).map_err(|_| {
            VertexConfigError::InvalidCredentials {
                name: provider_name.clone(),
            }
        })?;

        let project_id = project_id
            .or_else(|| key.project_id.clone())
            .ok_or_else(|| VertexConfigError::MissingProject {
                name: provider_name.clone(),
            })?;

        let location = location.ok_or_else(|| VertexConfigError::MissingLocation {
            name: provider_name.clone(),
        })?;

        let endpoint_base = endpoint_base
            .unwrap_or_else(|| format!("https://{location}-aiplatform.googleapis.com"))
            .trim_end_matches('/')
            .to_owned();

        let auth = TokenSource::new(client.clone(), provider_name.clone(), key);

        Ok(Self {
            client,
            provider_name,
            state: Some(Ready {
                project_id,
                location,
                endpoint_base,
                auth,
            }),
        })
    }

    /// The ready state, or the request-time error for an unconfigured provider:
    /// an upstream auth failure attributed to this provider (surfaces as a 502
    /// naming the provider, never a misleading gateway 401).
    fn ready(&self) -> Result<&Ready, ProviderError> {
        self.state.as_ref().ok_or_else(|| ProviderError::Upstream {
            provider: self.provider_name.clone(),
            status: 401,
            retryable: false,
        })
    }

    /// Open the upstream SSE stream (shared by both streaming trait methods).
    async fn open_translated_stream(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamItem, ProviderError>>, ProviderError> {
        let ready = self.ready()?;
        let token = ready.auth.token(&cancel).await?;
        let url = ready.model_url(&req.model, true);
        let body = translate_request(&req, &self.provider_name)?;
        let bytes = open_stream(
            &self.client,
            &url,
            &body,
            Some(&token),
            &self.provider_name,
            &cancel,
        )
        .await?;
        Ok(translate_sse_stream(
            bytes,
            GoogleTranslator::new(&req.model),
        ))
    }
}

#[async_trait]
impl ChatProvider for VertexProvider {
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, ProviderError> {
        let ready = self.ready()?;
        let token = ready.auth.token(&cancel).await?;
        let url = ready.model_url(&req.model, false);
        let body = translate_request(&req, &self.provider_name)?;
        let bytes = post_json(
            &self.client,
            &url,
            &body,
            Some(&token),
            &self.provider_name,
            &cancel,
        )
        .await?;
        let parsed: GeminiResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("google vertex response: {e}")))?;
        Ok(translate_response(parsed, &req.model))
    }

    async fn chat_stream(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<ChatChunk, ProviderError>>, ProviderError> {
        let items = self.open_translated_stream(req, cancel).await?;
        Ok(items_to_chunks(items))
    }

    async fn chat_stream_bytes(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<Bytes, ProviderError>>, ProviderError> {
        let items = self.open_translated_stream(req, cancel).await?;
        Ok(items_to_sse_bytes(items))
    }

    /// Vertex, like Gemini, accepts only inline base64 image bytes.
    fn accepts_remote_image_url(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_core::{ChatMessage, MessageContent};

    const TEST_KEY: &str = include_str!("testdata/test_private_key.pem");

    fn creds() -> String {
        serde_json::json!({
            "type": "service_account",
            "project_id": "key-project",
            "client_email": "svc@key-project.iam.gserviceaccount.com",
            "private_key": TEST_KEY,
            "token_uri": "https://oauth2.googleapis.com/token",
        })
        .to_string()
    }

    fn ready(p: &VertexProvider) -> &Ready {
        p.state.as_ref().expect("provider is configured")
    }

    #[test]
    fn url_is_regional_and_project_scoped() {
        let p = VertexProvider::new(
            reqwest::Client::new(),
            "vertex",
            Some(&creds()),
            Some("my-project".to_owned()),
            Some("us-central1".to_owned()),
            None,
        )
        .expect("builds");
        assert_eq!(
            ready(&p).model_url("gemini-2.0-flash", false),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/\
             locations/us-central1/publishers/google/models/gemini-2.0-flash:generateContent"
        );
        assert_eq!(
            ready(&p).model_url("gemini-2.0-flash", true),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/\
             locations/us-central1/publishers/google/models/gemini-2.0-flash:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn project_id_falls_back_to_the_credentials() {
        let p = VertexProvider::new(
            reqwest::Client::new(),
            "vertex",
            Some(&creds()),
            None,
            Some("europe-west1".to_owned()),
            None,
        )
        .expect("builds");
        assert_eq!(ready(&p).project_id, "key-project");
        assert!(ready(&p)
            .model_url("m", false)
            .contains("/projects/key-project/locations/europe-west1/"));
    }

    #[test]
    fn endpoint_base_override_wins() {
        let p = VertexProvider::new(
            reqwest::Client::new(),
            "vertex",
            Some(&creds()),
            Some("proj".to_owned()),
            Some("us-central1".to_owned()),
            Some("http://127.0.0.1:9999/".to_owned()),
        )
        .expect("builds");
        assert!(ready(&p)
            .model_url("m", false)
            .starts_with("http://127.0.0.1:9999/v1/"));
    }

    #[tokio::test]
    async fn missing_credentials_builds_but_fails_requests_naming_provider() {
        // No creds env var: the gateway must still boot (provider builds), and
        // the failure is a provider-named upstream error at request time.
        let p = VertexProvider::new(
            reqwest::Client::new(),
            "vertex",
            None,
            Some("proj".to_owned()),
            Some("us-central1".to_owned()),
            None,
        )
        .expect("unconfigured provider still builds");
        let req = ChatRequest {
            model: "gemini-2.0-flash".to_owned(),
            messages: vec![ChatMessage {
                role: "user".to_owned(),
                content: Some(MessageContent::Text("hi".to_owned())),
                name: None,
                extra: serde_json::Map::new(),
            }],
            temperature: None,
            top_p: None,
            max_tokens: None,
            n: None,
            stop: None,
            stream: false,
            extra: serde_json::Map::new(),
        };
        let err = p
            .chat(req, CancellationToken::new())
            .await
            .expect_err("unconfigured provider fails requests");
        match err {
            ProviderError::Upstream {
                provider,
                retryable,
                ..
            } => {
                assert_eq!(provider, "vertex");
                assert!(!retryable);
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn invalid_credentials_json_is_a_config_error() {
        let err = VertexProvider::new(
            reqwest::Client::new(),
            "vertex",
            Some("{ not json"),
            Some("proj".to_owned()),
            Some("us-central1".to_owned()),
            None,
        )
        .expect_err("garbage creds fail the build");
        assert!(matches!(err, VertexConfigError::InvalidCredentials { .. }));
    }

    #[test]
    fn missing_location_is_a_config_error() {
        let err = VertexProvider::new(
            reqwest::Client::new(),
            "vertex",
            Some(&creds()),
            Some("proj".to_owned()),
            None,
            None,
        )
        .expect_err("no location");
        assert!(matches!(err, VertexConfigError::MissingLocation { .. }));
    }

    #[test]
    fn debug_never_leaks_the_private_key() {
        let p = VertexProvider::new(
            reqwest::Client::new(),
            "vertex",
            Some(&creds()),
            Some("proj".to_owned()),
            Some("us-central1".to_owned()),
            None,
        )
        .expect("builds");
        let rendered = format!("{p:?}");
        assert!(!rendered.contains("PRIVATE KEY"), "leaked: {rendered}");
        assert!(!rendered.contains("MIIE"), "leaked: {rendered}");
    }
}
