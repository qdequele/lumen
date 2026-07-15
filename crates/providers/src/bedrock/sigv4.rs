//! AWS Signature Version 4 request signing for Bedrock.
//!
//! A small, self-contained SigV4 implementation over `hmac` + `sha2` (both
//! pure-Rust, rustls-compatible, no OpenSSL) rather than pulling in the AWS SDK
//! runtime. Bedrock's Converse endpoints are a single `POST` with an empty
//! query string, so only the request-signing subset is needed; hand-rolling it
//! keeps the dependency and RAM footprint aligned with LUMEN's pillars.
//!
//! Reference: <https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv4-create-signed-request.html>
//!
//! Canonical-path encoding: for every service EXCEPT S3, SigV4 requires each
//! path segment of the canonical request to be URI-encoded TWICE. The wire URL
//! carries the single-encoded path (`...v2%3A0`); the canonical request signs
//! the double-encoded form (`...v2%253A0`), because AWS re-encodes the received
//! wire path once more before recomputing the signature. Signing the
//! single-encoded path yields `403 SignatureDoesNotMatch` for any model id
//! containing a reserved character (every versioned id with a `:`).
//!
//! Secrets (the secret access key, the session token) are never logged and
//! never placed in any error; only the derived, opaque `Authorization` value is
//! returned to the caller.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// The SigV4 algorithm identifier used in the `Authorization` header and the
/// string-to-sign.
const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// The Bedrock runtime signs under the `bedrock` service name (the endpoint is
/// `bedrock-runtime.{region}.amazonaws.com` but the signing service is
/// `bedrock`).
pub(super) const SERVICE: &str = "bedrock";

/// Resolved AWS credentials used to sign a request. `secret_access_key` and
/// `session_token` are secrets: this type has no `Debug` that reveals them (see
/// [`super::Credentials`], which owns the redaction) and is only borrowed here.
pub(super) struct SigningParams<'a> {
    /// AWS access key id (public half; safe to place in the `Credential=` scope).
    pub access_key_id: &'a str,
    /// AWS secret access key (secret; used only as HMAC key material).
    pub secret_access_key: &'a str,
    /// Optional STS session token, sent as `x-amz-security-token`.
    pub session_token: Option<&'a str>,
    /// AWS region (e.g. `us-east-1`); part of the credential scope.
    pub region: &'a str,
}

/// The subset of headers produced by signing, ready to attach to the request.
/// Every value is derived, non-secret material EXCEPT `x-amz-security-token`
/// (the session token), which the caller must not log.
pub(super) struct SignedHeaders {
    /// `Authorization` header value.
    pub authorization: String,
    /// `x-amz-date` header value (`YYYYMMDDTHHMMSSZ`).
    pub amz_date: String,
    /// `x-amz-content-sha256` header value (lowercase hex SHA-256 of the body).
    pub content_sha256: String,
    /// `x-amz-security-token`, present only for temporary credentials.
    pub security_token: Option<String>,
}

/// Sign a Bedrock `POST` request.
///
/// * `host` is the bare `Host` header value (no scheme), e.g.
///   `bedrock-runtime.us-east-1.amazonaws.com`.
/// * `wire_path` is the request path exactly as sent on the wire, with each
///   segment percent-encoded ONCE (the caller encodes the model id). The
///   canonical request internally signs the DOUBLE-encoded form of this path,
///   per the non-S3 SigV4 rule (see the module docs).
/// * `body` is the exact request body that will be sent.
/// * `timestamp` is the request instant as Unix seconds (injected so the signer
///   is deterministic and unit-testable against AWS's published vectors).
///
/// The query string is always empty (Bedrock Converse takes none) and the
/// content type is always `application/json`.
pub(super) fn sign_request(
    params: &SigningParams<'_>,
    host: &str,
    wire_path: &str,
    body: &[u8],
    timestamp: u64,
) -> SignedHeaders {
    let (amz_date, date_stamp) = format_amz_time(timestamp);
    let content_sha256 = hex::encode(Sha256::digest(body));

    // Canonical headers, sorted by lowercase name. `content-type`, `host`,
    // `x-amz-content-sha256` and `x-amz-date` are always signed; the session
    // token is signed too when present so it cannot be stripped in flight.
    let mut headers: Vec<(&str, String)> = vec![
        ("content-type", "application/json".to_owned()),
        ("host", host.to_owned()),
        ("x-amz-content-sha256", content_sha256.clone()),
        ("x-amz-date", amz_date.clone()),
    ];
    if let Some(token) = params.session_token {
        headers.push(("x-amz-security-token", token.to_owned()));
    }
    headers.sort_by(|a, b| a.0.cmp(b.0));

    // The canonical request signs the DOUBLE-encoded path (non-S3 SigV4 rule);
    // the wire URL keeps the single-encoded form.
    let canonical_path = canonical_uri(wire_path);
    let (canonical, signed_headers) =
        canonical_request("POST", &canonical_path, "", &headers, &content_sha256);

    let scope = format!("{date_stamp}/{}/{SERVICE}/aws4_request", params.region);
    let sts = string_to_sign(&amz_date, &scope, &canonical);
    let signature = compute_signature(
        params.secret_access_key,
        &date_stamp,
        params.region,
        SERVICE,
        &sts,
    );

    let authorization = format!(
        "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        params.access_key_id
    );

    SignedHeaders {
        authorization,
        amz_date,
        content_sha256,
        security_token: params.session_token.map(str::to_owned),
    }
}

/// Build the canonical request and the `SignedHeaders` list from its parts.
/// `headers` must already be sorted by lowercase name. Generic over method /
/// path / query so the known-answer tests can drive it with the AWS
/// `aws-sig-v4-test-suite` inputs verbatim.
fn canonical_request(
    method: &str,
    canonical_path: &str,
    canonical_query: &str,
    headers: &[(&str, String)],
    payload_hash: &str,
) -> (String, String) {
    let canonical_headers = headers.iter().fold(String::new(), |mut acc, header| {
        acc.push_str(header.0);
        acc.push(':');
        acc.push_str(header.1.trim());
        acc.push('\n');
        acc
    });
    let signed_headers: String = headers
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(";");
    let canonical = format!(
        "{method}\n{canonical_path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    (canonical, signed_headers)
}

/// Build the SigV4 string-to-sign from the timestamp, credential scope and the
/// canonical request.
fn string_to_sign(amz_date: &str, scope: &str, canonical_request: &str) -> String {
    let hashed = hex::encode(Sha256::digest(canonical_request.as_bytes()));
    format!("{ALGORITHM}\n{amz_date}\n{scope}\n{hashed}")
}

/// Derive the signing key and produce the final lowercase-hex signature.
fn compute_signature(
    secret_access_key: &str,
    date_stamp: &str,
    region: &str,
    service: &str,
    string_to_sign: &str,
) -> String {
    let key = derive_signing_key(secret_access_key, date_stamp, region, service);
    hex::encode(hmac(&key, string_to_sign.as_bytes()))
}

/// Double-encode a single-encoded wire path for the canonical request (the
/// non-S3 SigV4 rule): every segment is percent-encoded once more, so `%3A`
/// becomes `%253A` while slashes and unreserved characters are untouched.
fn canonical_uri(wire_path: &str) -> String {
    wire_path
        .split('/')
        .map(uri_encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Percent-encode one path segment per RFC 3986: every byte except the
/// unreserved set (`A-Z a-z 0-9 - _ . ~`) becomes `%XX`. Applied once by the
/// caller to build the wire path (the model id contains `:` in versioned ids
/// like `...-v2:0`), and once more by [`canonical_uri`] for the canonical
/// request.
pub(super) fn uri_encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for &byte in segment.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(upper_hex_nibble(byte >> 4));
            out.push(upper_hex_nibble(byte & 0x0f));
        }
    }
    out
}

/// One uppercase hex digit for the low nibble of `n`.
fn upper_hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'A' + (n - 10)) as char,
    }
}

/// Derive the SigV4 signing key: nested HMACs over date, region, service and
/// the terminating `aws4_request` literal. `service` is a parameter (not the
/// [`SERVICE`] constant) so the derivation can be checked against AWS's
/// published `iam` worked example in the tests.
fn derive_signing_key(
    secret_access_key: &str,
    date_stamp: &str,
    region: &str,
    service: &str,
) -> Vec<u8> {
    let k_secret = format!("AWS4{secret_access_key}");
    let k_date = hmac(k_secret.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    hmac(&k_service, b"aws4_request")
}

/// HMAC-SHA256 of `data` under `key`. The `hmac` crate accepts any key length,
/// so construction never fails.
fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Format a Unix timestamp as the pair (`YYYYMMDDTHHMMSSZ`, `YYYYMMDD`) in UTC.
///
/// Computed from epoch seconds directly (civil-from-days) so no date/time crate
/// is pulled in. Valid for all timestamps after the epoch.
fn format_amz_time(secs: u64) -> (String, String) {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    let amz_date = format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z");
    let date_stamp = format!("{year:04}{month:02}{day:02}");
    (amz_date, date_stamp)
}

/// Convert a count of days since the Unix epoch (1970-01-01) into a civil
/// `(year, month, day)` in UTC. Howard Hinnant's public-domain algorithm.
///
/// The casts are inherent to the integer algorithm and provably in range: the
/// day count fits an `i64` for any realistic timestamp, and the derived month
/// (1-12) and day (1-31) fit a `u32` without truncation or sign loss.
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn civil_from_days(days_since_epoch: u64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01 so leap days fall at the end of each cycle.
    let z = days_since_epoch as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11] (March-based)
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's public example credentials from the SigV4 test suite (not real).
    const TEST_SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

    #[test]
    fn amz_time_matches_known_instant() {
        // 2015-08-30T12:36:00Z = 1440938160 (AWS SigV4 documentation vector).
        let (amz, stamp) = format_amz_time(1_440_938_160);
        assert_eq!(amz, "20150830T123600Z");
        assert_eq!(stamp, "20150830");
    }

    #[test]
    fn amz_time_handles_epoch_and_leap_day() {
        let (amz, stamp) = format_amz_time(0);
        assert_eq!(amz, "19700101T000000Z");
        assert_eq!(stamp, "19700101");
        // 2020-02-29T23:59:59Z = 1583020799.
        let (amz, stamp) = format_amz_time(1_583_020_799);
        assert_eq!(amz, "20200229T235959Z");
        assert_eq!(stamp, "20200229");
    }

    #[test]
    fn uri_encode_leaves_unreserved_and_encodes_colon() {
        assert_eq!(
            uri_encode_segment("anthropic.claude-3-5-sonnet-20241022-v2:0"),
            "anthropic.claude-3-5-sonnet-20241022-v2%3A0"
        );
        // Unreserved characters pass through untouched.
        assert_eq!(uri_encode_segment("aZ0-_.~"), "aZ0-_.~");
        // Space and slash are encoded.
        assert_eq!(uri_encode_segment("a b/c"), "a%20b%2Fc");
    }

    /// The canonical path is the wire path encoded ONCE MORE (non-S3 rule):
    /// `%3A` (wire) must become `%253A` (canonical), slashes untouched.
    #[test]
    fn canonical_uri_double_encodes_the_wire_path() {
        assert_eq!(
            canonical_uri("/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/converse"),
            "/model/anthropic.claude-3-5-sonnet-20241022-v2%253A0/converse"
        );
        // A colon-free path is unchanged by the second pass.
        assert_eq!(
            canonical_uri("/model/test/converse"),
            "/model/test/converse"
        );
    }

    #[test]
    fn signing_key_derivation_matches_aws_published_vector() {
        // AWS's documented worked example ("Deriving the signing key",
        // service = iam). The credentials are AWS's public example, not real.
        let key = derive_signing_key(TEST_SECRET, "20150830", "us-east-1", "iam");
        assert_eq!(
            hex::encode(&key),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    /// Known-answer test: the `get-vanilla` case of AWS's published
    /// `aws-sig-v4-test-suite`, asserting the EXACT final signature.
    ///
    /// Inputs (from the suite): `GET /` against `example.amazonaws.com` at
    /// `20150830T123600Z`, service `service`, region `us-east-1`, empty body,
    /// only `host` and `x-amz-date` signed. Published signature:
    /// `5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31`.
    #[test]
    fn get_vanilla_matches_aws_test_suite_signature() {
        let empty_hash = hex::encode(Sha256::digest(b""));
        let headers: Vec<(&str, String)> = vec![
            ("host", "example.amazonaws.com".to_owned()),
            ("x-amz-date", "20150830T123600Z".to_owned()),
        ];
        let (canonical, signed_headers) = canonical_request("GET", "/", "", &headers, &empty_hash);
        assert_eq!(signed_headers, "host;x-amz-date");
        let sts = string_to_sign(
            "20150830T123600Z",
            "20150830/us-east-1/service/aws4_request",
            &canonical,
        );
        let signature = compute_signature(TEST_SECRET, "20150830", "us-east-1", "service", &sts);
        assert_eq!(
            signature,
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// Known-answer test: the `post-vanilla` case of the same suite. Published
    /// signature: `5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b`.
    #[test]
    fn post_vanilla_matches_aws_test_suite_signature() {
        let empty_hash = hex::encode(Sha256::digest(b""));
        let headers: Vec<(&str, String)> = vec![
            ("host", "example.amazonaws.com".to_owned()),
            ("x-amz-date", "20150830T123600Z".to_owned()),
        ];
        let (canonical, _) = canonical_request("POST", "/", "", &headers, &empty_hash);
        let sts = string_to_sign(
            "20150830T123600Z",
            "20150830/us-east-1/service/aws4_request",
            &canonical,
        );
        let signature = compute_signature(TEST_SECRET, "20150830", "us-east-1", "service", &sts);
        assert_eq!(
            signature,
            "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b"
        );
    }

    /// Known-answer test for the DOUBLE-encoded canonical path: signing a wire
    /// path containing `%3A` must produce the signature computed over the
    /// `%253A` canonical form. Expected value computed independently (Python
    /// hashlib/hmac following the documented SigV4 steps) over the canonical
    /// request with path `/model/anthropic.claude-3-5-sonnet-20241022-v2%253A0/converse`.
    /// Signing the single-encoded path instead yields
    /// `a0cf8808e8d8092566bf4a44fd054a5f90fc49525072e5eddd3f76be3ab22db4`
    /// (the SignatureDoesNotMatch bug this test guards against).
    #[test]
    fn sign_request_signs_the_double_encoded_canonical_path() {
        let params = SigningParams {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: TEST_SECRET,
            session_token: None,
            region: "us-east-1",
        };
        let signed = sign_request(
            &params,
            "bedrock-runtime.us-east-1.amazonaws.com",
            "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/converse",
            b"{}",
            1_440_938_160,
        );
        let signature = signed
            .authorization
            .rsplit("Signature=")
            .next()
            .expect("signature present");
        assert_eq!(
            signature, "f90d1ffff57230eb5fe7b2cda7a813d66a075984a0a6a666ce8e5d6031e53b54",
            "canonical path must be double-encoded (%253A), not single-encoded (%3A)"
        );
    }

    #[test]
    fn sign_request_produces_well_formed_authorization() {
        let params = SigningParams {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: TEST_SECRET,
            session_token: None,
            region: "us-east-1",
        };
        let signed = sign_request(
            &params,
            "bedrock-runtime.us-east-1.amazonaws.com",
            "/model/test/converse",
            b"{}",
            1_440_938_160,
        );
        assert_eq!(signed.amz_date, "20150830T123600Z");
        assert_eq!(signed.content_sha256, hex::encode(Sha256::digest(b"{}")));
        // Authorization carries the algorithm, the scoped credential, the exact
        // signed-header list, and a 64-hex signature.
        assert!(signed.authorization.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request"
        ));
        assert!(signed
            .authorization
            .contains("SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date"));
        let sig = signed
            .authorization
            .rsplit("Signature=")
            .next()
            .expect("signature present");
        assert_eq!(sig.len(), 64);
        assert!(sig.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(signed.security_token.is_none());
    }

    #[test]
    fn session_token_is_signed_and_returned() {
        let params = SigningParams {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "secret",
            session_token: Some("FwoGZXIvYXdz//////TOKEN"),
            region: "eu-west-1",
        };
        let signed = sign_request(
            &params,
            "bedrock-runtime.eu-west-1.amazonaws.com",
            "/model/test/converse",
            b"{}",
            1_440_938_160,
        );
        // The token is echoed for the header AND folded into SignedHeaders.
        assert_eq!(
            signed.security_token.as_deref(),
            Some("FwoGZXIvYXdz//////TOKEN")
        );
        assert!(signed.authorization.contains("x-amz-security-token"));
    }

    /// The secret access key must never appear in any signer output.
    #[test]
    fn signer_output_never_contains_the_secret() {
        let params = SigningParams {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: TEST_SECRET,
            session_token: Some("SECRET-SESSION-TOKEN"),
            region: "us-east-1",
        };
        let signed = sign_request(
            &params,
            "bedrock-runtime.us-east-1.amazonaws.com",
            "/model/m/converse",
            b"payload",
            1_440_938_160,
        );
        assert!(!signed.authorization.contains(TEST_SECRET));
        assert!(!signed.content_sha256.contains(TEST_SECRET));
        // The session token IS the security-token header value by definition,
        // but must not leak into the Authorization line beyond its header name.
        assert!(!signed.authorization.contains("SECRET-SESSION-TOKEN"));
    }
}
