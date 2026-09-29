//! Provider API-key checks that spend no tokens.
//!
//! Each provider kind that exposes a free, authenticated endpoint (a model
//! list, a key-introspection route, an OAuth token mint, a control-plane
//! listing) is probed there, never on an inference route. The outcome is a
//! [`KeyCheck`] whose `key_valid` is a tri-state: `Some(true)` (the upstream
//! accepted the credentials), `Some(false)` (it rejected them, or none are
//! configured), `None` (this check cannot tell: a keyless kind, a kind with
//! no known free endpoint, a rate limit, a 5xx, an unreachable host).
//!
//! A passing check proves the credentials *authenticate*, not that inference
//! will succeed: an exhausted quota, a model the key is not entitled to, or a
//! missing Bedrock model-access grant only surface on a real request.
//!
//! The key is sent upstream and nowhere else: it is never part of the
//! outcome, its `endpoint` label, its `detail`, a log line, or a `Debug`.

use std::fmt;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::bedrock::BedrockProvider;
use crate::google::vertex::VertexProvider;
use crate::kind::ProviderKind;

/// Upper bound on one check, so a black-holed host cannot pin an admin call.
pub(crate) const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// The outcome of one provider key check. Secret-free by construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeyCheck {
    /// `true` accepted, `false` rejected or not configured, `null` unknown.
    pub key_valid: Option<bool>,
    /// Whether the check endpoint answered; `null` when no request was made.
    pub reachable: Option<bool>,
    /// The upstream HTTP status, when a response was received.
    pub http_status: Option<u16>,
    /// Round-trip time of the check request, when one was made.
    pub latency_ms: Option<u64>,
    /// What was probed, e.g. `GET https://api.openai.com/v1/models`. Never
    /// carries a credential (keys travel in headers only).
    pub endpoint: Option<String>,
    /// A short human-readable explanation. Never echoes upstream bodies.
    pub detail: String,
}

impl KeyCheck {
    /// An outcome decided without any network call.
    pub(crate) fn without_request(key_valid: Option<bool>, detail: impl Into<String>) -> Self {
        Self {
            key_valid,
            reachable: None,
            http_status: None,
            latency_ms: None,
            endpoint: None,
            detail: detail.into(),
        }
    }

    /// The configured credential is absent: rejected without asking upstream.
    pub(crate) fn missing_key(what: &str) -> Self {
        Self::without_request(
            Some(false),
            format!("no {what} configured for this provider"),
        )
    }

    pub(crate) fn cancelled(endpoint: String) -> Self {
        Self {
            endpoint: Some(endpoint),
            ..Self::without_request(None, "check cancelled")
        }
    }
}

/// How a response to a check request maps to a verdict (beyond the shared
/// 2xx / 401 / 403 / 429 / 404 / 5xx rules).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// 2xx accepted, 401/403 rejected.
    Standard,
    /// Cohere `check-api-key`: a 2xx carries `{"valid": bool}`.
    CohereValidFlag,
    /// Gemini answers an invalid key with a 400 whose body names
    /// `API_KEY_INVALID`.
    GoogleApiKeyInvalid,
    /// AWS: a 403 `AccessDeniedException` means the credentials authenticated
    /// but lack IAM permission for the probe action.
    AwsAccessDenied,
}

/// Send a prepared check request, honouring `cancel` and [`CHECK_TIMEOUT`],
/// and classify the response. `endpoint` is the secret-free label reported
/// back (method + URL).
pub(crate) async fn run(
    builder: reqwest::RequestBuilder,
    endpoint: String,
    verdict: Verdict,
    cancel: &CancellationToken,
) -> KeyCheck {
    let started = Instant::now();
    let call = async {
        let response = builder.timeout(CHECK_TIMEOUT).send().await?;
        let status = response.status().as_u16();
        let error_type = response
            .headers()
            .get("x-amzn-errortype")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = if needs_body(verdict, status) {
            Some(response.bytes().await?)
        } else {
            None
        };
        Ok::<_, reqwest::Error>((status, error_type, body))
    };

    let outcome = tokio::select! {
        biased;
        () = cancel.cancelled() => return KeyCheck::cancelled(endpoint),
        outcome = call => outcome,
    };
    let latency_ms = Some(elapsed_ms(started));

    match outcome {
        Ok((status, error_type, body)) => {
            let (key_valid, detail) =
                interpret(verdict, status, error_type.as_deref(), body.as_deref());
            KeyCheck {
                key_valid,
                reachable: Some(true),
                http_status: Some(status),
                latency_ms,
                endpoint: Some(endpoint),
                detail,
            }
        }
        Err(error) => KeyCheck {
            key_valid: None,
            reachable: Some(false),
            http_status: None,
            latency_ms,
            endpoint: Some(endpoint),
            detail: transport_detail(&error).to_owned(),
        },
    }
}

/// The reported `METHOD URL` label, with any `user:password@` userinfo a
/// configured `base_url` may carry removed (it is a credential too). An
/// unparseable URL is not echoed at all.
pub(crate) fn endpoint_label(method: &reqwest::Method, url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut parsed) => {
            // Both only fail for URLs that cannot carry userinfo at all.
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            format!("{method} {parsed}")
        }
        Err(_) => format!("{method} <unparseable URL>"),
    }
}

/// A secret-free description of a transport failure (never the URL or the
/// underlying error text).
pub(crate) fn transport_detail(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "check endpoint timed out"
    } else {
        "check endpoint unreachable"
    }
}

/// Milliseconds since `started`, saturating.
pub(crate) fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Whether classifying this response needs its body.
fn needs_body(verdict: Verdict, status: u16) -> bool {
    match verdict {
        Verdict::CohereValidFlag => (200..300).contains(&status),
        Verdict::GoogleApiKeyInvalid => status == 400,
        Verdict::Standard | Verdict::AwsAccessDenied => false,
    }
}

/// Map a check response to `(key_valid, detail)`.
fn interpret(
    verdict: Verdict,
    status: u16,
    error_type: Option<&str>,
    body: Option<&[u8]>,
) -> (Option<bool>, String) {
    match status {
        200..=299 => {
            if verdict == Verdict::CohereValidFlag {
                return match body.and_then(cohere_valid_flag) {
                    Some(true) => (Some(true), "key accepted".to_owned()),
                    Some(false) => (Some(false), "key rejected".to_owned()),
                    None => (None, "unexpected check response body".to_owned()),
                };
            }
            (Some(true), "key accepted".to_owned())
        }
        403 if verdict == Verdict::AwsAccessDenied
            && error_type.is_some_and(|t| t.starts_with("AccessDenied")) =>
        {
            (
                Some(true),
                "credentials authenticated, but lack the bedrock:ListFoundationModels \
                 permission this check uses; inference permissions are not verified"
                    .to_owned(),
            )
        }
        401 | 403 => (Some(false), format!("key rejected (HTTP {status})")),
        400 if verdict == Verdict::GoogleApiKeyInvalid
            && body.is_some_and(|b| contains(b, b"API_KEY_INVALID")) =>
        {
            (
                Some(false),
                "key rejected (HTTP 400, API_KEY_INVALID)".to_owned(),
            )
        }
        429 => (
            None,
            "rate limited (HTTP 429); the key was probably accepted, but this check \
             cannot confirm it"
                .to_owned(),
        ),
        404 => (
            None,
            "check endpoint not found (HTTP 404); base_url may point at a server \
             that does not serve it"
                .to_owned(),
        ),
        300..=399 => (
            None,
            format!(
                "redirected (HTTP {status}); redirects are never followed, so \
                 base_url may be stale or point at a proxy"
            ),
        ),
        500..=599 => (None, format!("upstream error (HTTP {status})")),
        _ => (None, format!("unexpected HTTP {status}")),
    }
}

/// Cohere's `{"valid": bool}` flag, if the body carries one.
fn cohere_valid_flag(body: &[u8]) -> Option<bool> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("valid")?
        .as_bool()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// A per-provider key checker, held by the registry and rebuilt with it on
/// every hot reload, so a check always uses the live credentials.
pub(crate) enum KeyProber {
    /// Kinds checked with one plain HTTP request built from the spec.
    Http(HttpProber),
    /// Vertex AI: mint an OAuth token from the service account.
    Vertex(VertexProvider),
    /// Bedrock: a SigV4-signed control-plane listing.
    Bedrock(BedrockProvider),
}

impl KeyProber {
    /// Run the check.
    pub(crate) async fn check(&self, cancel: &CancellationToken) -> KeyCheck {
        match self {
            Self::Http(prober) => prober.check(cancel).await,
            Self::Vertex(provider) => provider.check_key(cancel).await,
            Self::Bedrock(provider) => provider.check_key(cancel).await,
        }
    }
}

impl fmt::Debug for KeyProber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(prober) => prober.fmt(f),
            Self::Vertex(provider) => provider.fmt(f),
            Self::Bedrock(provider) => provider.fmt(f),
        }
    }
}

/// The spec fields a plain-HTTP key check needs.
pub(crate) struct HttpProber {
    pub(crate) client: reqwest::Client,
    pub(crate) kind: ProviderKind,
    pub(crate) base_url: Option<String>,
    /// Resolved API key. Redacted from `Debug`; never logged.
    pub(crate) api_key: Option<String>,
    pub(crate) api_version: Option<String>,
}

impl fmt::Debug for HttpProber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpProber")
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("api_version", &self.api_version)
            .finish_non_exhaustive()
    }
}

/// Where and how to send a plain-HTTP check.
struct Probe {
    method: reqwest::Method,
    url: String,
    auth: Auth,
    verdict: Verdict,
}

/// How the key is attached to a check request.
enum Auth {
    Bearer,
    /// The key in the named header, plus fixed extra headers.
    Header(&'static str, &'static [(&'static str, &'static str)]),
}

impl HttpProber {
    async fn check(&self, cancel: &CancellationToken) -> KeyCheck {
        let key = self.api_key.as_deref().filter(|k| !k.is_empty());
        let Some(key) = key else {
            return if self.kind.requires_api_key() {
                KeyCheck::missing_key("API key")
            } else {
                KeyCheck::without_request(
                    None,
                    "keyless provider (no API key configured); nothing to check",
                )
            };
        };
        let Some(probe) = self.probe() else {
            return KeyCheck::without_request(
                None,
                format!(
                    "no free key-check endpoint known for kind '{}'; only a real \
                     request can verify this key",
                    self.kind.as_str()
                ),
            );
        };

        let endpoint = endpoint_label(&probe.method, &probe.url);
        let mut builder = self.client.request(probe.method, &probe.url);
        builder = match probe.auth {
            Auth::Bearer => builder.bearer_auth(key),
            Auth::Header(name, extra) => {
                let mut b = builder.header(name, key);
                for (h, v) in extra {
                    b = b.header(*h, *v);
                }
                b
            }
        };
        run(builder, endpoint, probe.verdict, cancel).await
    }

    /// The check request for this kind, or `None` when the kind has no free,
    /// authenticated endpoint worth trusting.
    // A flat per-kind table (like `registry::build_providers`): its length
    // scales with the number of kinds, and splitting it would only scatter
    // the mapping.
    #[allow(clippy::too_many_lines)]
    fn probe(&self) -> Option<Probe> {
        let base = |default: &str| {
            self.base_url
                .as_deref()
                .unwrap_or(default)
                .trim_end_matches('/')
                .to_owned()
        };
        let get = |url: String, auth: Auth| Probe {
            method: reqwest::Method::GET,
            url,
            auth,
            verdict: Verdict::Standard,
        };
        match self.kind {
            ProviderKind::Openai => Some(get(
                format!("{}/models", base(crate::openai::DEFAULT_BASE_URL)),
                Auth::Bearer,
            )),
            ProviderKind::Mistral => Some(get(
                format!("{}/models", base(crate::mistral::DEFAULT_BASE_URL)),
                Auth::Bearer,
            )),
            ProviderKind::Groq
            | ProviderKind::Together
            | ProviderKind::Fireworks
            | ProviderKind::Deepseek
            | ProviderKind::Xai
            | ProviderKind::Deepinfra
            | ProviderKind::Vllm => {
                let root = base(self.kind.default_base_url().unwrap_or_default());
                Some(get(format!("{root}/models"), Auth::Bearer))
            }
            // OpenRouter's model list is public, so it would pass any key; its
            // key-introspection route is authenticated and free.
            ProviderKind::Openrouter => {
                let root = base(self.kind.default_base_url().unwrap_or_default());
                Some(get(format!("{root}/key"), Auth::Bearer))
            }
            // The router's model list is public (it would pass any key), so
            // the router, whether defaulted or set explicitly, is checked with
            // the authenticated `whoami`. Any other base_url (a dedicated
            // endpoint) serves its own authenticated `/models`.
            ProviderKind::Huggingface => {
                let router = self.kind.default_base_url().unwrap_or_default();
                let root = base(router);
                let url = if root == router {
                    "https://huggingface.co/api/whoami-v2".to_owned()
                } else {
                    format!("{root}/models")
                };
                Some(get(url, Auth::Bearer))
            }
            // Model search is account-scoped and authenticated, and accepts
            // both user- and account-owned API tokens.
            ProviderKind::Cloudflare => {
                let configured = base("");
                let root = configured
                    .strip_suffix("/ai/v1")
                    .or_else(|| configured.strip_suffix("/v1"))
                    .unwrap_or(&configured);
                Some(get(
                    format!("{root}/ai/models/search?per_page=1"),
                    Auth::Bearer,
                ))
            }
            ProviderKind::Anthropic => Some(get(
                format!(
                    "{}/v1/models?limit=1",
                    base(crate::anthropic::DEFAULT_BASE_URL)
                ),
                Auth::Header(
                    "x-api-key",
                    &[("anthropic-version", crate::anthropic::ANTHROPIC_VERSION)],
                ),
            )),
            ProviderKind::Cohere => Some(Probe {
                method: reqwest::Method::POST,
                url: format!("{}/v1/check-api-key", base(crate::cohere::DEFAULT_BASE_URL)),
                auth: Auth::Bearer,
                verdict: Verdict::CohereValidFlag,
            }),
            ProviderKind::Google => Some(Probe {
                verdict: Verdict::GoogleApiKeyInvalid,
                ..get(
                    format!(
                        "{}/v1beta/models?pageSize=1",
                        base(crate::google::DEFAULT_BASE_URL)
                    ),
                    Auth::Header("x-goog-api-key", &[]),
                )
            }),
            ProviderKind::Azure => {
                let (endpoint, url_version) = crate::azure::split_endpoint_and_version(&base(""));
                let version = self
                    .api_version
                    .clone()
                    .or(url_version)
                    .unwrap_or_else(|| crate::azure::DEFAULT_API_VERSION.to_owned());
                Some(get(
                    format!(
                        "{endpoint}/openai/models?api-version={}",
                        crate::azure::percent_encode(&version)
                    ),
                    Auth::Header("api-key", &[]),
                ))
            }
            ProviderKind::Pinecone => Some(get(
                format!("{}/indexes", base(crate::pinecone::DEFAULT_BASE_URL)),
                Auth::Header(
                    "Api-Key",
                    &[("X-Pinecone-API-Version", crate::pinecone::API_VERSION)],
                ),
            )),
            // No documented endpoint that is both free and authenticated (or,
            // for NVIDIA's hosted catalog, one that actually checks the key).
            // Vertex and Bedrock never reach here: they have their own probers.
            ProviderKind::Jina
            | ProviderKind::Voyage
            | ProviderKind::Mixedbread
            | ProviderKind::Typesafe
            | ProviderKind::Perplexity
            | ProviderKind::Nvidia
            | ProviderKind::Tei
            | ProviderKind::Ollama
            | ProviderKind::VertexAi
            | ProviderKind::Bedrock => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_verdicts() {
        let v = |s| interpret(Verdict::Standard, s, None, None).0;
        assert_eq!(v(200), Some(true));
        assert_eq!(v(204), Some(true));
        assert_eq!(v(401), Some(false));
        assert_eq!(v(403), Some(false));
        assert_eq!(v(400), None);
        assert_eq!(v(404), None);
        assert_eq!(v(429), None);
        assert_eq!(v(502), None);
        assert_eq!(v(301), None);
        assert_eq!(v(307), None);
    }

    #[test]
    fn aws_access_denied_is_authenticated_but_other_403s_are_not() {
        let v = |t| interpret(Verdict::AwsAccessDenied, 403, Some(t), None).0;
        assert_eq!(v("AccessDeniedException:http://internal/"), Some(true));
        assert_eq!(v("UnrecognizedClientException"), Some(false));
        assert_eq!(v("InvalidSignatureException"), Some(false));
        assert_eq!(
            interpret(Verdict::Standard, 403, Some("AccessDeniedException"), None).0,
            Some(false),
            "only the AWS verdict reads the error type"
        );
    }

    #[test]
    fn cohere_body_without_a_valid_flag_is_unknown() {
        let v = |b: &[u8]| interpret(Verdict::CohereValidFlag, 200, None, Some(b)).0;
        assert_eq!(v(br#"{"valid":true}"#), Some(true));
        assert_eq!(v(br#"{"valid":false}"#), Some(false));
        assert_eq!(v(br"{}"), None);
        assert_eq!(v(b"not json"), None);
    }

    #[test]
    fn detail_never_echoes_the_upstream_body() {
        let body = br#"{"error":{"message":"sk-leaked","details":[{"reason":"API_KEY_INVALID"}]}}"#;
        let (_, detail) = interpret(Verdict::GoogleApiKeyInvalid, 400, None, Some(body));
        assert!(!detail.contains("sk-leaked"), "{detail}");
    }

    fn prober(kind: ProviderKind, base_url: Option<&str>) -> HttpProber {
        HttpProber {
            client: reqwest::Client::new(),
            kind,
            base_url: base_url.map(str::to_owned),
            api_key: Some("k".to_owned()),
            api_version: None,
        }
    }

    #[test]
    fn huggingface_router_uses_whoami_even_when_set_explicitly() {
        // The router's model list is public: probing it would pass any key.
        const WHOAMI: &str = "https://huggingface.co/api/whoami-v2";
        for base in [
            None,
            Some("https://router.huggingface.co/v1"),
            Some("https://router.huggingface.co/v1/"),
        ] {
            let probe = prober(ProviderKind::Huggingface, base).probe();
            assert_eq!(probe.map(|p| p.url).as_deref(), Some(WHOAMI), "{base:?}");
        }
        // A dedicated endpoint serves its own authenticated `/models`.
        let probe = prober(ProviderKind::Huggingface, Some("https://my-ep.example/v1")).probe();
        assert_eq!(
            probe.map(|p| p.url).as_deref(),
            Some("https://my-ep.example/v1/models")
        );
    }

    #[test]
    fn endpoint_label_drops_userinfo() {
        assert_eq!(
            endpoint_label(&reqwest::Method::GET, "https://u:p@proxy.local/v1/models"),
            "GET https://proxy.local/v1/models"
        );
        assert_eq!(
            endpoint_label(
                &reqwest::Method::POST,
                "https://api.cohere.com/v1/check-api-key"
            ),
            "POST https://api.cohere.com/v1/check-api-key"
        );
    }

    #[test]
    fn http_prober_debug_redacts_the_key() {
        let prober = HttpProber {
            client: reqwest::Client::new(),
            kind: ProviderKind::Openai,
            base_url: None,
            api_key: Some("sk-debug-secret".to_owned()),
            api_version: None,
        };
        let dbg = format!("{prober:?}");
        assert!(!dbg.contains("sk-debug-secret"), "{dbg}");
        assert!(dbg.contains("<redacted>"));
    }
}
