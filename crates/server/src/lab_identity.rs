//! Self-description against the Lab (platform contract v2, section 3.6):
//! `GET {LAB_URL}/internal/instances/me` confirms the instance credentials
//! and tells the gateway which hosted deployment it is, for the boot log.
//! Called once at boot, off the request path. Rejected credentials (401 or
//! 403) and an answer for another product or instance abort boot; a Lab
//! that cannot answer only costs a warning, so a control-plane outage never
//! keeps a gateway down.

use crate::usage_events::INSTANCE_HEADER;
use std::fmt;
use zeroize::Zeroizing;

/// The Lab's description of this deployment (`GET /internal/instances/me`,
/// spec section 3.6).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct InstanceIdentity {
    /// Echo of `LAB_INSTANCE_ID`.
    pub instance_id: String,
    /// Always `hosted` (decision A: engines are hosted by Meilisearch only).
    pub kind: String,
    /// The Lab product this deployment serves; must be `lumen`.
    #[serde(default)]
    pub product: Option<String>,
    /// The hosting region (`eu-west-1`), for the boot log.
    #[serde(default)]
    pub region: Option<String>,
    /// The Lab's own public URL, for the boot log.
    #[serde(default)]
    pub lab_url: Option<String>,
}

/// Why the identity could not be confirmed. `Rejected` and `Malformed` are
/// configuration errors and abort boot; the rest is a warning.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// The Lab refused the instance credentials (401 or 403).
    #[error("the Lab rejected the instance credentials (HTTP {0}): check LAB_INSTANCE_ID and the instance secret")]
    Rejected(u16),
    /// Any other non-2xx answer (the endpoint may not exist on this Lab yet).
    #[error("the Lab answered HTTP {0} to GET /internal/instances/me")]
    Status(u16),
    /// Connect error, timeout, or a 2xx body that stalls or breaks mid-read;
    /// the message never carries the URL.
    #[error("could not reach the Lab: {0}")]
    Transport(String),
    /// A 2xx body that is not an identity, or one for another product (the
    /// Lab would skip every event with `product mismatch`, spec section 3.5)
    /// or for another instance.
    #[error("the Lab's answer to GET /internal/instances/me is not this gateway's identity: {0}")]
    Malformed(String),
}

impl IdentityError {
    /// Configuration errors that must abort boot.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Rejected(_) | Self::Malformed(_))
    }
}

/// Fetches the identity with the instance credentials. The secret is held
/// zeroized and redacted from `Debug`.
pub struct LabIdentityClient {
    client: reqwest::Client,
    endpoint: String,
    instance_id: String,
    secret: Zeroizing<String>,
}

impl fmt::Debug for LabIdentityClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LabIdentityClient")
            .field("endpoint", &self.endpoint)
            .field("instance_id", &self.instance_id)
            .field("secret", &"REDACTED")
            .finish_non_exhaustive()
    }
}

impl LabIdentityClient {
    /// `lab_url` is the Lab base URL (`LAB_URL`); a trailing slash is ignored.
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        lab_url: &str,
        instance_id: String,
        secret: String,
    ) -> Self {
        Self {
            client,
            endpoint: format!("{}/internal/instances/me", lab_url.trim_end_matches('/')),
            instance_id,
            secret: Zeroizing::new(secret),
        }
    }

    /// One `GET /internal/instances/me` (spec section 3.6).
    ///
    /// # Errors
    /// See [`IdentityError`].
    pub async fn fetch(&self) -> Result<InstanceIdentity, IdentityError> {
        let response = self
            .client
            .get(&self.endpoint)
            .bearer_auth(self.secret.as_str())
            .header(INSTANCE_HEADER, &self.instance_id)
            .send()
            .await
            .map_err(|e| IdentityError::Transport(e.without_url().to_string()))?;
        let status = response.status().as_u16();
        match status {
            401 | 403 => return Err(IdentityError::Rejected(status)),
            200..=299 => {}
            other => return Err(IdentityError::Status(other)),
        }
        // Read the body and parse it separately: reqwest reports a body that
        // stalls or breaks mid-read as a decode error too, and that is an
        // outage (non-fatal), not a malformed identity (fatal).
        let body = response
            .bytes()
            .await
            .map_err(|e| IdentityError::Transport(e.without_url().to_string()))?;
        let identity: InstanceIdentity =
            serde_json::from_slice(&body).map_err(|e| IdentityError::Malformed(e.to_string()))?;
        // Instance ids are UUIDs (not secrets): compare case-insensitively.
        if !identity.instance_id.eq_ignore_ascii_case(&self.instance_id) {
            return Err(IdentityError::Malformed(format!(
                "the Lab answered for instance '{}', not '{}'",
                identity.instance_id, self.instance_id
            )));
        }
        if identity.product.as_deref().is_some_and(|p| p != "lumen") {
            return Err(IdentityError::Malformed(format!(
                "the instance is registered for product '{}', not lumen",
                identity.product.as_deref().unwrap_or_default()
            )));
        }
        if identity.kind != "hosted" {
            return Err(IdentityError::Malformed(format!(
                "unexpected instance kind '{}'",
                identity.kind
            )));
        }
        Ok(identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const INSTANCE: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b71";
    const SECRET: &str = "identity-secret-9c1d";

    fn client(server: &MockServer) -> LabIdentityClient {
        LabIdentityClient::new(
            lumen_providers::http::build_client_with(
                Duration::from_secs(2),
                Duration::from_secs(5),
            ),
            &format!("{}/", server.uri()),
            INSTANCE.to_owned(),
            SECRET.to_owned(),
        )
    }

    async fn mount(server: &MockServer, status: u16, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/internal/instances/me"))
            .and(header("authorization", format!("Bearer {SECRET}").as_str()))
            .and(header(INSTANCE_HEADER, INSTANCE))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn a_hosted_engine_learns_its_identity() {
        let server = MockServer::start().await;
        mount(
            &server,
            200,
            serde_json::json!({
                "instance_id": INSTANCE, "kind": "hosted", "product": "lumen",
                "region": "eu-west-1", "lab_url": server.uri()
            }),
        )
        .await;
        let identity = client(&server).fetch().await.unwrap();
        assert_eq!(identity.instance_id, INSTANCE);
        assert_eq!(identity.kind, "hosted");
        assert_eq!(identity.product.as_deref(), Some("lumen"));
        assert_eq!(identity.region.as_deref(), Some("eu-west-1"));
    }

    #[tokio::test]
    async fn rejected_credentials_are_fatal_and_outages_are_not() {
        for (status, fatal) in [(401, true), (403, true), (404, false), (503, false)] {
            let server = MockServer::start().await;
            mount(&server, status, serde_json::json!({})).await;
            let error = client(&server).fetch().await.unwrap_err();
            assert_eq!(error.is_fatal(), fatal, "{status}: {error}");
            if !fatal {
                assert!(
                    matches!(error, IdentityError::Status(_)),
                    "{status}: {error}"
                );
            }
        }
        // Unreachable: not fatal. A dropped `MockServer` goes back to
        // wiremock's shared pool and keeps listening, so take a port from a
        // plain listener and close it: nothing answers there.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let client = LabIdentityClient::new(
            lumen_providers::http::build_client_with(
                Duration::from_millis(500),
                Duration::from_secs(1),
            ),
            &format!("http://127.0.0.1:{port}"),
            INSTANCE.to_owned(),
            SECRET.to_owned(),
        );
        let error = client.fetch().await.unwrap_err();
        assert!(matches!(error, IdentityError::Transport(_)), "{error}");
        assert!(!error.is_fatal());
    }

    #[tokio::test]
    async fn another_product_or_a_malformed_answer_is_fatal() {
        for body in [
            serde_json::json!({ "instance_id": INSTANCE, "kind": "hosted", "product": "scrapix" }),
            serde_json::json!({ "hello": "world" }),
            // An answer for another instance: the credentials map elsewhere.
            serde_json::json!({
                "instance_id": "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b72",
                "kind": "hosted", "product": "lumen"
            }),
        ] {
            let server = MockServer::start().await;
            mount(&server, 200, body.clone()).await;
            let error = client(&server).fetch().await.unwrap_err();
            assert!(
                matches!(error, IdentityError::Malformed(_)),
                "{body}: {error}"
            );
            assert!(error.is_fatal());
        }
    }

    #[tokio::test]
    async fn a_2xx_whose_body_stalls_is_an_outage_not_a_config_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Headers arrive (200, 64-byte body announced), then the body stalls
        // past the client's overall timeout. Wiremock's `set_delay` holds the
        // headers too (that is already a `send()` timeout), so a raw socket
        // is the only way to stall mid-body.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 1024];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 64\r\n\r\n{\"instance_id\":",
                )
                .await
                .unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_secs(3)).await;
        });
        let client = LabIdentityClient::new(
            lumen_providers::http::build_client_with(
                Duration::from_millis(500),
                Duration::from_secs(1),
            ),
            &format!("http://127.0.0.1:{port}"),
            INSTANCE.to_owned(),
            SECRET.to_owned(),
        );
        let error = client.fetch().await.unwrap_err();
        assert!(matches!(error, IdentityError::Transport(_)), "{error}");
        assert!(!error.is_fatal());
        server.abort();
    }

    #[tokio::test]
    async fn a_2xx_with_invalid_json_stays_malformed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/internal/instances/me"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("{not json", "application/json"))
            .mount(&server)
            .await;
        let error = client(&server).fetch().await.unwrap_err();
        assert!(matches!(error, IdentityError::Malformed(_)), "{error}");
        assert!(error.is_fatal());
    }

    #[tokio::test]
    async fn the_instance_id_echo_ignores_ascii_case() {
        let server = MockServer::start().await;
        mount(
            &server,
            200,
            serde_json::json!({
                "instance_id": INSTANCE.to_ascii_uppercase(),
                "kind": "hosted", "product": "lumen"
            }),
        )
        .await;
        let identity = client(&server).fetch().await.unwrap();
        assert!(identity.instance_id.eq_ignore_ascii_case(INSTANCE));
    }

    #[tokio::test]
    async fn the_secret_never_appears_in_debug_or_errors() {
        let server = MockServer::start().await;
        mount(&server, 500, serde_json::json!({})).await;
        let client = client(&server);
        assert!(!format!("{client:?}").contains(SECRET));
        let error = client.fetch().await.unwrap_err();
        assert!(!error.to_string().contains(SECRET));
    }
}
