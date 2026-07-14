//! Integration tests for the guarded image-fetch stage (M9).
//!
//! A wiremock server plays the remote image host. Because wiremock binds
//! loopback (which the SSRF guard blocks), the fetch-permitting tests set the
//! test-only `allow_private_ips` escape hatch; the SSRF test leaves it off and
//! asserts loopback is rejected without any request reaching the host.

use std::time::Duration;

use base64::Engine;
use lumen_core::{ContentPart, EmbedInput, EmbedItem, GatewayError, ImageUrl};
use lumen_providers::image_fetch::{resolve_image_parts, ImageFetchPolicy};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A permissive, fetch-enabled policy for the happy-path tests (loopback
/// allowed only because wiremock binds 127.0.0.1).
fn enabled_policy() -> ImageFetchPolicy {
    ImageFetchPolicy {
        enabled: true,
        max_bytes: 1024 * 1024,
        timeout: Duration::from_secs(5),
        allowed_schemes: vec!["http".to_owned(), "https".to_owned()],
        allowed_hosts: Vec::new(),
        allowed_url_prefixes: Vec::new(),
        allow_private_ips: true,
    }
}

/// A `Multi` input whose single item carries a text part and one image part
/// pointing at `url`.
fn input_with_image(url: &str) -> EmbedInput {
    EmbedInput::Multi(vec![EmbedItem::Parts(vec![
        ContentPart {
            kind: "text".to_owned(),
            text: Some("a caption".to_owned()),
            image_url: None,
            extra: serde_json::Map::new(),
        },
        ContentPart {
            kind: "image_url".to_owned(),
            text: None,
            image_url: Some(ImageUrl {
                url: url.to_owned(),
                detail: None,
            }),
            extra: serde_json::Map::new(),
        },
    ])])
}

/// Pull the (single) image part's URL back out for assertions.
fn image_url_of(input: &EmbedInput) -> String {
    let EmbedInput::Multi(items) = input else {
        panic!("expected Multi");
    };
    let EmbedItem::Parts(parts) = &items[0] else {
        panic!("expected Parts");
    };
    parts
        .iter()
        .find_map(|p| p.image_url.as_ref())
        .expect("image part")
        .url
        .clone()
}

#[tokio::test]
async fn fetches_remote_image_and_rewrites_to_data_uri() {
    let host = MockServer::start().await;
    // 1x1 PNG-ish payload; content doesn't matter, only that bytes round-trip.
    let bytes = vec![0x89u8, 0x50, 0x4e, 0x47, 0x01, 0x02, 0x03, 0x04];
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(bytes.clone()),
        )
        .mount(&host)
        .await;

    let url = format!("{}/cat.png", host.uri());
    let mut input = input_with_image(&url);
    resolve_image_parts(&mut input, &enabled_policy(), &CancellationToken::new())
        .await
        .expect("fetch succeeds");

    let rewritten = image_url_of(&input);
    let expected_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    assert_eq!(rewritten, format!("data:image/png;base64,{expected_b64}"));
}

#[tokio::test]
async fn data_uri_is_passed_through_without_fetching() {
    let original = "data:image/png;base64,QUJD";
    let mut input = input_with_image(original);
    // Even with fetching disabled, a data: URI is untouched (no network).
    let policy = ImageFetchPolicy {
        enabled: false,
        ..enabled_policy()
    };
    resolve_image_parts(&mut input, &policy, &CancellationToken::new())
        .await
        .expect("data uri passthrough");
    assert_eq!(image_url_of(&input), original);
}

#[tokio::test]
async fn remote_url_with_fetch_disabled_is_lm2005() {
    let mut input = input_with_image("https://example.com/cat.png");
    let policy = ImageFetchPolicy {
        enabled: false,
        ..enabled_policy()
    };
    let err = resolve_image_parts(&mut input, &policy, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, GatewayError::ImageFetchDisabled));
    assert_eq!(err.code(), "LM-2005");
}

#[tokio::test]
async fn oversized_image_is_rejected_lm2006() {
    let host = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(vec![0u8; 4096]),
        )
        .mount(&host)
        .await;

    let mut input = input_with_image(&format!("{}/big.png", host.uri()));
    let policy = ImageFetchPolicy {
        max_bytes: 1024, // smaller than the 4096-byte body
        ..enabled_policy()
    };
    let err = resolve_image_parts(&mut input, &policy, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, GatewayError::ImageUrlRejected));
    assert_eq!(err.code(), "LM-2006");
}

#[tokio::test]
async fn non_image_content_type_is_rejected_lm2006() {
    let host = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_string("<html>nope</html>"),
        )
        .mount(&host)
        .await;

    let mut input = input_with_image(&format!("{}/page", host.uri()));
    let err = resolve_image_parts(&mut input, &enabled_policy(), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, GatewayError::ImageUrlRejected));
}

#[tokio::test]
async fn loopback_is_blocked_when_private_ips_not_allowed() {
    let host = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "image/png"))
        .mount(&host)
        .await;

    let mut input = input_with_image(&format!("{}/cat.png", host.uri()));
    // Default guard: loopback (wiremock) must be rejected as SSRF.
    let policy = ImageFetchPolicy {
        allow_private_ips: false,
        ..enabled_policy()
    };
    let err = resolve_image_parts(&mut input, &policy, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, GatewayError::ImageUrlRejected));

    // The host was never contacted (blocked before connect).
    assert!(host.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn host_not_in_allowlist_is_rejected() {
    let mut input = input_with_image("https://evil.example/cat.png");
    let policy = ImageFetchPolicy {
        allowed_hosts: vec!["cdn.trusted.com".to_owned()],
        ..enabled_policy()
    };
    let err = resolve_image_parts(&mut input, &policy, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, GatewayError::ImageUrlRejected));
}

#[tokio::test]
async fn too_many_remote_images_is_rejected_before_fetching() {
    let host = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(vec![1u8, 2, 3]),
        )
        .mount(&host)
        .await;

    // 33 remote image parts — over the per-request cap (32).
    let parts: Vec<ContentPart> = (0..33)
        .map(|i| ContentPart {
            kind: "image_url".to_owned(),
            text: None,
            image_url: Some(ImageUrl {
                url: format!("{}/img{i}.png", host.uri()),
                detail: None,
            }),
            extra: serde_json::Map::new(),
        })
        .collect();
    let mut input = EmbedInput::Multi(vec![EmbedItem::Parts(parts)]);

    let err = resolve_image_parts(&mut input, &enabled_policy(), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, GatewayError::ImageUrlRejected));
    // Rejected before any fetch.
    assert!(host.received_requests().await.unwrap().is_empty());
}
