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
use crate::mapping::{classify_error, parse_retry_after};

/// The OAuth scope Vertex AI requires.
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// Refresh a cached token this long before its stated expiry, so a token is
/// never presented to the upstream in the last seconds of its life.
const EXPIRY_SKEW: Duration = Duration::from_secs(60);

/// How long a token whose lifetime the endpoint did not state (`expires_in`
/// missing or zero) is cached. Without a floor such a token is "expired" the
/// moment it is cached and every request re-mints one, putting the token
/// exchange back on the per-request path.
const UNKNOWN_LIFETIME_TTL: Duration = Duration::from_secs(30);

/// How long to cache a token that states `expires_in` seconds of life: the
/// stated lifetime less [`EXPIRY_SKEW`]; a token shorter-lived than the skew
/// is cached for half its life rather than not at all; an unstated lifetime
/// gets [`UNKNOWN_LIFETIME_TTL`]. Never zero.
fn cache_ttl(expires_in: Option<u64>) -> Duration {
    match expires_in {
        None | Some(0) => UNKNOWN_LIFETIME_TTL,
        Some(secs) => {
            let lifetime = Duration::from_secs(secs);
            if lifetime > EXPIRY_SKEW {
                lifetime.checked_sub(EXPIRY_SKEW).unwrap_or(lifetime / 2)
            } else {
                lifetime / 2
            }
        }
    }
}

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
    /// Token lifetime in seconds. Google always states it; `None` (or zero)
    /// falls back to a short floor, see [`cache_ttl`].
    #[serde(default)]
    expires_in: Option<u64>,
}

/// A cached access token and the instant it should be refreshed by.
struct CachedToken {
    value: String,
    /// Absolute deadline (already discounted, see [`cache_ttl`]).
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
    /// Waiting for the lock honours `cancel` like the exchange itself: a caller
    /// queued behind a slow exchange is released as soon as its client leaves.
    ///
    /// # Errors
    /// A signing, transport, non-2xx or malformed-response failure at the token
    /// endpoint becomes a provider-named upstream error (never a client 401).
    pub(crate) async fn token(&self, cancel: &CancellationToken) -> Result<String, ProviderError> {
        let mut guard = with_cancel(cancel, async { Ok(self.cache.lock().await) }).await?;
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

    /// Mint a brand-new token, bypassing (and not filling) the cache: a key
    /// check must prove the service account works NOW, not that a token
    /// minted before a key revocation is still inside its hour of validity.
    pub(crate) async fn mint_fresh(&self, cancel: &CancellationToken) -> Result<(), ProviderError> {
        self.fetch(cancel).await.map(|_| ())
    }

    /// Whether the private key can sign an assertion at all. A malformed key
    /// is only detected here (parsing is lazy), and it is a local credential
    /// failure: no request needs to be sent to know it will be rejected.
    pub(crate) fn can_sign(&self) -> bool {
        self.signed_assertion().is_ok()
    }

    /// The OAuth token endpoint (public, secret-free).
    pub(crate) fn token_uri(&self) -> &str {
        &self.token_uri
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
                let code = status.as_u16();
                let body = crate::http::error_body_prefix(response).await;
                return Err(classify_error(provider, code, retry_after, &body));
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
                    retry_after: None,
                }
            })?;
            Ok(parsed)
        };

        let parsed = with_cancel(cancel, call).await?;
        if parsed.expires_in.is_none_or(|secs| secs == 0) {
            tracing::warn!(
                provider,
                "token endpoint stated no token lifetime (expires_in); \
                 caching the token for a short default"
            );
        }
        let refresh_at = Instant::now() + cache_ttl(parsed.expires_in);
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
            retry_after: None,
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
                ..
            } => {
                assert_eq!(provider, "vertex");
                assert_ne!(status, 401, "must not masquerade as a client auth error");
                assert!(!retryable);
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }
    #[test]
    fn cache_ttl_is_never_zero() {
        // Google's usual hour: the stated lifetime less the skew.
        assert_eq!(cache_ttl(Some(3600)), Duration::from_secs(3540));
        // Missing or zero: a short floor, not "expired on arrival".
        assert_eq!(cache_ttl(None), UNKNOWN_LIFETIME_TTL);
        assert_eq!(cache_ttl(Some(0)), UNKNOWN_LIFETIME_TTL);
        // Shorter-lived than the skew: half its life, still cached.
        assert_eq!(cache_ttl(Some(40)), Duration::from_secs(20));
        assert_eq!(cache_ttl(Some(60)), Duration::from_secs(30));
        assert!(cache_ttl(Some(1)) > Duration::ZERO);
    }

    async fn mock_token_endpoint(body: serde_json::Value) -> (wiremock::MockServer, TokenSource) {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(&mock)
            .await;
        let mut key = sample_key();
        key.token_uri = format!("{}/token", mock.uri());
        let source = TokenSource::new(reqwest::Client::new(), "vertex", key);
        (mock, source)
    }

    /// A token response without `expires_in` used to be cached with a zero
    /// lifetime, so every request re-minted a token.
    #[tokio::test]
    async fn token_without_expires_in_is_cached_not_reminted() {
        let (mock, source) =
            mock_token_endpoint(serde_json::json!({ "access_token": "ya29.x" })).await;
        let cancel = CancellationToken::new();
        for _ in 0..3 {
            assert_eq!(source.token(&cancel).await.expect("token"), "ya29.x");
        }
        assert_eq!(mock.received_requests().await.expect("recorded").len(), 1);
    }

    /// Once the refresh deadline passes, the next call mints a new token.
    #[tokio::test]
    async fn token_past_its_refresh_deadline_is_reminted() {
        let (mock, source) = mock_token_endpoint(
            serde_json::json!({ "access_token": "ya29.x", "expires_in": 3600 }),
        )
        .await;
        let cancel = CancellationToken::new();
        source.token(&cancel).await.expect("token");
        if let Some(cached) = source.cache.lock().await.as_mut() {
            cached.refresh_at = Instant::now();
        }
        source.token(&cancel).await.expect("token");
        assert_eq!(mock.received_requests().await.expect("recorded").len(), 2);
    }

    /// A caller queued behind an in-flight exchange (which holds the cache
    /// lock) is released by its own cancellation instead of waiting it out.
    #[tokio::test]
    async fn waiting_for_the_cache_lock_is_cancellable() {
        let source = TokenSource::new(reqwest::Client::new(), "vertex", sample_key());
        let _held = source.cache.lock().await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = tokio::time::timeout(Duration::from_secs(5), source.token(&cancel))
            .await
            .expect("must not wait for the lock")
            .expect_err("cancelled");
        assert!(matches!(err, ProviderError::Cancelled), "{err:?}");
    }
}
