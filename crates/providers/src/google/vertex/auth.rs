//! GCP service-account OAuth2 for Vertex AI.
//!
//! Vertex AI authenticates with a short-lived OAuth2 access token, not a static
//! API key. This module implements the server-to-server JWT-bearer flow:
//!
//! 1. build a JWT whose claims name the service account (`iss`), the requested
//!    scope (`cloud-platform`) and the token endpoint (`aud`), then RS256-sign
//!    it with the account's private key;
//! 2. exchange that assertion at the token endpoint
//!    (`urn:ietf:params:oauth:grant-type:jwt-bearer`) for an `access_token`;
//! 3. cache the token in memory and reuse it until shortly before it expires,
//!    so the token fetch stays off the per-request hot path.
//!
//! The private key never appears in `Debug` output, logs or errors. The token
//! endpoint URL is taken from the credentials (`token_uri`), so tests point it
//! at a wiremock server and never reach Google.

use std::fmt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use lumen_core::ProviderError;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::http::{map_transport, with_cancel};
use crate::mapping::{classify_status, parse_retry_after};

/// The OAuth scope Vertex AI requires.
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// Refresh a cached token this long before its stated expiry, so a token is
/// never presented to the upstream in the last seconds of its life.
const EXPIRY_SKEW: Duration = Duration::from_secs(60);

/// Requested lifetime of the signed assertion (Google caps this at 1 hour).
const ASSERTION_TTL_SECS: u64 = 3600;

/// A redacted secret string: never rendered by `Debug` and never in errors.
struct Redacted(String);

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The subset of a GCP service-account key file this flow needs.
///
/// Deserialized directly from the standard service-account JSON, so an operator
/// can paste the file's contents verbatim.
#[derive(Deserialize)]
pub(crate) struct ServiceAccountKey {
    /// The service account's email, used as the JWT issuer and subject.
    pub client_email: String,
    /// The RS256 private key in PEM form. Redacted everywhere.
    pub private_key: String,
    /// The OAuth token endpoint. Standard files carry
    /// `https://oauth2.googleapis.com/token`; tests override it to a mock.
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
    /// The GCP project id embedded in the key file (used when the operator did
    /// not configure one explicitly).
    #[serde(default)]
    pub project_id: Option<String>,
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_owned()
}

impl ServiceAccountKey {
    /// Parse a service-account key from its JSON representation.
    ///
    /// # Errors
    /// Returns [`ProviderError::Translation`] if the JSON is not a well-formed
    /// service-account key. The error text never echoes the input, so a private
    /// key cannot leak through a parse error.
    pub(crate) fn from_json(raw: &str) -> Result<Self, ProviderError> {
        serde_json::from_str(raw).map_err(|_| {
            ProviderError::Translation("invalid Google service-account credentials JSON".to_owned())
        })
    }
}

/// JWT claims for the assertion (RFC 7523).
#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

/// The token endpoint's success payload.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: u64,
}

/// A cached access token and the instant it should be refreshed by.
struct CachedToken {
    value: String,
    /// Absolute deadline (already discounted by [`EXPIRY_SKEW`]).
    refresh_at: Instant,
}

/// Mints and caches OAuth2 access tokens for one service account.
///
/// Cloning is cheap conceptually but not needed; the source is held behind an
/// `Arc` inside the provider. The private key and any live token are redacted
/// from `Debug`.
pub(crate) struct TokenSource {
    client: reqwest::Client,
    provider_name: String,
    client_email: String,
    /// PEM private key. Never logged, never in `Debug`, never in an error.
    private_key: Redacted,
    token_uri: String,
    cache: Mutex<Option<CachedToken>>,
}

impl fmt::Debug for TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenSource")
            .field("provider_name", &self.provider_name)
            .field("client_email", &self.client_email)
            .field("token_uri", &self.token_uri)
            .field("private_key", &self.private_key)
            .finish_non_exhaustive()
    }
}

impl TokenSource {
    /// Build a token source from an already-parsed service-account key.
    pub(crate) fn new(
        client: reqwest::Client,
        provider_name: impl Into<String>,
        key: ServiceAccountKey,
    ) -> Self {
        Self {
            client,
            provider_name: provider_name.into(),
            client_email: key.client_email,
            private_key: Redacted(key.private_key),
            token_uri: key.token_uri,
            cache: Mutex::new(None),
        }
    }

    /// Return a valid access token, minting a fresh one only when the cache is
    /// empty or the current token is within [`EXPIRY_SKEW`] of expiry.
    ///
    /// The lock is held across the token fetch, so concurrent callers coalesce
    /// onto a single upstream exchange rather than stampeding it. The valid-cache
    /// path only clones a `String`, so it stays off the network hot path.
    ///
    /// # Errors
    /// A signing, transport, non-2xx or malformed-response failure at the token
    /// endpoint becomes a provider-named upstream error (never a client 401).
    pub(crate) async fn token(&self, cancel: &CancellationToken) -> Result<String, ProviderError> {
        let mut guard = self.cache.lock().await;
        if let Some(cached) = guard.as_ref() {
            if Instant::now() < cached.refresh_at {
                return Ok(cached.value.clone());
            }
        }
        let fetched = self.fetch(cancel).await?;
        let value = fetched.value.clone();
        *guard = Some(fetched);
        Ok(value)
    }

    /// Perform the JWT-bearer exchange once, honouring cancellation.
    async fn fetch(&self, cancel: &CancellationToken) -> Result<CachedToken, ProviderError> {
        let assertion = self.signed_assertion()?;
        let params = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
        ];

        let provider = self.provider_name.as_str();
        let call = async {
            let response = self
                .client
                .post(&self.token_uri)
                .form(&params)
                .send()
                .await
                .map_err(|e| map_transport(provider, &e))?;
            let status = response.status();
            if !status.is_success() {
                // A token-exchange rejection is the upstream's fault, attributed
                // to this provider - never surfaced as a misleading client 401.
                let retry_after = parse_retry_after(response.headers());
                return Err(classify_status(provider, status.as_u16(), retry_after));
            }
            let bytes = response
                .bytes()
                .await
                .map_err(|e| map_transport(provider, &e))?;
            let parsed: TokenResponse = serde_json::from_slice(&bytes).map_err(|_| {
                // Malformed token payload: the upstream's fault, so 502-class.
                ProviderError::Upstream {
                    provider: provider.to_owned(),
                    status: 502,
                    retryable: false,
                }
            })?;
            Ok(parsed)
        };

        let parsed = with_cancel(cancel, call).await?;
        let lifetime = Duration::from_secs(parsed.expires_in);
        let refresh_at = Instant::now() + lifetime.saturating_sub(EXPIRY_SKEW);
        Ok(CachedToken {
            value: parsed.access_token,
            refresh_at,
        })
    }

    /// Build and RS256-sign the JWT assertion. The private key is used but never
    /// echoed: a signing failure maps to a generic provider-named upstream error
    /// so no key material can leak through an error string.
    fn signed_assertion(&self) -> Result<String, ProviderError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let claims = Claims {
            iss: &self.client_email,
            scope: SCOPE,
            aud: &self.token_uri,
            iat: now,
            exp: now + ASSERTION_TTL_SECS,
        };
        let key = EncodingKey::from_rsa_pem(self.private_key.0.as_bytes())
            .map_err(|_| self.auth_error())?;
        encode(&Header::new(Algorithm::RS256), &claims, &key).map_err(|_| self.auth_error())
    }

    /// A provider-named, secret-free error for any credential/signing failure.
    fn auth_error(&self) -> ProviderError {
        ProviderError::Upstream {
            provider: self.provider_name.clone(),
            status: 502,
            retryable: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A throwaway 2048-bit RSA key generated only for these tests. It signs no
    // real Google traffic: the token endpoint is always a wiremock mock.
    const TEST_KEY: &str = include_str!("testdata/test_private_key.pem");

    fn sample_key() -> ServiceAccountKey {
        ServiceAccountKey {
            client_email: "svc@proj.iam.gserviceaccount.com".to_owned(),
            private_key: TEST_KEY.to_owned(),
            token_uri: "https://oauth2.googleapis.com/token".to_owned(),
            project_id: Some("proj".to_owned()),
        }
    }

    #[test]
    fn service_account_json_parses_and_defaults_token_uri() {
        let raw = format!(
            r#"{{"type":"service_account","project_id":"p","client_email":"a@b.iam","private_key":{}}}"#,
            serde_json::to_string(TEST_KEY).unwrap()
        );
        let key = ServiceAccountKey::from_json(&raw).expect("parses");
        assert_eq!(key.client_email, "a@b.iam");
        assert_eq!(key.token_uri, "https://oauth2.googleapis.com/token");
        assert_eq!(key.project_id.as_deref(), Some("p"));
    }

    #[test]
    fn malformed_credentials_json_is_translation_error_without_echo() {
        match ServiceAccountKey::from_json("{ not json") {
            Err(ProviderError::Translation(msg)) => {
                assert!(!msg.contains("not json"), "must not echo input: {msg}");
            }
            Ok(_) => panic!("malformed JSON must not parse"),
            Err(other) => panic!("expected Translation, got {other:?}"),
        }
    }

    #[test]
    fn signing_produces_three_segment_jwt() {
        let source = TokenSource::new(reqwest::Client::new(), "vertex", sample_key());
        let jwt = source.signed_assertion().expect("signs");
        assert_eq!(
            jwt.split('.').count(),
            3,
            "a JWT has header.payload.signature"
        );
    }

    #[test]
    fn redacted_debug_never_shows_the_private_key() {
        let source = TokenSource::new(reqwest::Client::new(), "vertex", sample_key());
        let rendered = format!("{source:?}");
        assert!(!rendered.contains("PRIVATE KEY"), "leaked key: {rendered}");
        assert!(!rendered.contains("MIIE"), "leaked key body: {rendered}");
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn bad_private_key_maps_to_provider_named_upstream_never_401() {
        let mut key = sample_key();
        key.private_key = "-----BEGIN PRIVATE KEY-----\nnope\n-----END PRIVATE KEY-----".to_owned();
        let source = TokenSource::new(reqwest::Client::new(), "vertex", key);
        let err = source.signed_assertion().expect_err("bad key fails");
        match err {
            ProviderError::Upstream {
                provider,
                status,
                retryable,
            } => {
                assert_eq!(provider, "vertex");
                assert_ne!(status, 401, "must not masquerade as a client auth error");
                assert!(!retryable);
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }
}
