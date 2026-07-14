//! Opt-in, guarded server-side image fetching (M9).
//!
//! When enabled, remote `http(s)` image URLs in a multimodal embedding request
//! are fetched, base64-encoded, and rewritten in place to `data:` URIs *before*
//! provider translation - so providers only ever receive inline bytes. Fetching
//! is **off by default**; when on it is bounded by SSRF and resource guards
//! (private-IP block, scheme/host/prefix allowlists, size cap, timeout, MIME
//! allowlist), see [`ImageFetchPolicy`].
//!
//! This is a deliberate, config-gated exception to the "never dereference a
//! user-supplied URL" rule (which still holds for chat vision): it is opt-in,
//! off the streaming hot path, and time-bounded. See the M9 design.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use base64::Engine;
use futures::StreamExt;
use lumen_core::{EmbedInput, EmbedItem, GatewayError};
use tokio_util::sync::CancellationToken;

/// Runtime policy for the guarded fetch stage, built from the operator's
/// `[image_fetch]` config. All guards are enforced on every remote fetch.
#[derive(Debug, Clone)]
pub struct ImageFetchPolicy {
    /// Master switch. When `false`, a remote image URL yields `LM-2005`.
    pub enabled: bool,
    /// Maximum bytes downloaded per image.
    pub max_bytes: u64,
    /// Per-fetch timeout.
    pub timeout: Duration,
    /// Permitted URL schemes (e.g. `["https"]`). Lower-cased on comparison.
    pub allowed_schemes: Vec<String>,
    /// Permitted hosts. Empty = any (public) host. An entry beginning with `.`
    /// matches that domain and its subdomains.
    pub allowed_hosts: Vec<String>,
    /// Permitted URL prefixes. Empty = no prefix restriction.
    pub allowed_url_prefixes: Vec<String>,
    /// **Test-only** escape hatch that disables the private-IP block. It is
    /// never deserialized from config and the server always constructs the
    /// policy with this `false`; only unit/integration tests set it `true`
    /// (their mock host binds loopback). Keeping it here - rather than a
    /// `#[cfg(test)]` field - lets the cross-crate integration tests build the
    /// struct without a test-only build of this crate.
    pub allow_private_ips: bool,
}

impl Default for ImageFetchPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_bytes: 10 * 1024 * 1024,
            timeout: Duration::from_secs(5),
            allowed_schemes: vec!["https".to_owned()],
            allowed_hosts: Vec::new(),
            allowed_url_prefixes: Vec::new(),
            allow_private_ips: false,
        }
    }
}

/// Maximum redirect hops followed during a fetch; each hop is re-validated
/// against every guard.
const MAX_REDIRECTS: usize = 3;

/// Maximum remote images fetched for one request. Bounds the work a single
/// request can trigger *after* budget admission (a body can otherwise name
/// thousands of URLs); an over-limit request is rejected before any fetch.
const MAX_IMAGES_PER_REQUEST: usize = 32;

/// How many image fetches run concurrently. Bounds wall-clock to
/// roughly `ceil(N / CONCURRENCY) * timeout`.
const FETCH_CONCURRENCY: usize = 4;

/// Whether a URL is an inline `data:` URI (never fetched).
fn is_data_uri(url: &str) -> bool {
    url.trim_start().to_ascii_lowercase().starts_with("data:")
}

/// Resolve every remote image URL in `input` to an inline `data:` URI, in
/// place, applying the policy's guards. `data:` URIs are left untouched;
/// non-multimodal inputs are a no-op. Returns the first guard/fetch error.
///
/// Remote images are fetched with bounded concurrency and capped in number
/// ([`MAX_IMAGES_PER_REQUEST`]); an over-limit request is rejected before any
/// fetch, so this stage cannot be turned into an unbounded fan-out.
///
/// # Errors
/// - [`GatewayError::ImageFetchDisabled`] (`LM-2005`) - a remote URL while
///   `policy.enabled` is `false`.
/// - [`GatewayError::ImageUrlRejected`] (`LM-2006`) - a guard rejected the URL
///   (scheme, host/prefix allowlist, private IP, size cap, non-image type) or
///   the request exceeded [`MAX_IMAGES_PER_REQUEST`].
/// - [`GatewayError::ImageFetchFailed`] (`LM-2007`) - the remote host failed,
///   timed out, or the DNS lookup failed.
pub async fn resolve_image_parts(
    input: &mut EmbedInput,
    policy: &ImageFetchPolicy,
    cancel: &CancellationToken,
) -> Result<(), GatewayError> {
    let EmbedInput::Multi(items) = input else {
        return Ok(());
    };

    // Phase 1 - gather remote image URLs in traversal order (immutable walk).
    // A remote URL while fetching is disabled fails fast here.
    let mut remote: Vec<String> = Vec::new();
    for item in items.iter() {
        if let EmbedItem::Parts(parts) = item {
            for part in parts {
                if let Some(image) = part.image() {
                    if is_data_uri(&image.url) {
                        continue;
                    }
                    if !policy.enabled {
                        return Err(GatewayError::ImageFetchDisabled);
                    }
                    remote.push(image.url.clone());
                }
            }
        }
    }
    if remote.is_empty() {
        return Ok(());
    }
    if remote.len() > MAX_IMAGES_PER_REQUEST {
        return Err(GatewayError::ImageUrlRejected);
    }

    // Phase 2 - fetch concurrently, preserving order, short-circuiting on the
    // first error (which cancels the remaining in-flight fetches).
    let fetched = fetch_all(&remote, policy, cancel).await?;

    // Phase 3 - write the results back in the same traversal order.
    let mut results = fetched.into_iter();
    for item in items.iter_mut() {
        if let EmbedItem::Parts(parts) = item {
            for part in parts.iter_mut() {
                if let Some(image) = part.image_mut() {
                    if is_data_uri(&image.url) {
                        continue;
                    }
                    if let Some(data_uri) = results.next() {
                        image.url = data_uri;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Fetch every URL with bounded concurrency, returning the resulting `data:`
/// URIs in the same order. The first error short-circuits and drops the
/// remaining in-flight fetches.
async fn fetch_all(
    urls: &[String],
    policy: &ImageFetchPolicy,
    cancel: &CancellationToken,
) -> Result<Vec<String>, GatewayError> {
    use futures::stream::{self, TryStreamExt};
    // `buffered` preserves order and polls up to `FETCH_CONCURRENCY` at once;
    // `try_collect` short-circuits (and drops the rest) on the first error.
    // Each future owns its URL (`async move`) to keep the stream's item type
    // free of a per-element borrow.
    stream::iter(
        urls.iter()
            .cloned()
            .map(|url| async move { fetch_to_data_uri(&url, policy, cancel).await }),
    )
    .buffered(FETCH_CONCURRENCY)
    .try_collect()
    .await
}

/// Whether the URL string starts with one of the allowed prefixes (empty =
/// unrestricted).
fn prefix_allowed(url: &str, allowed: &[String]) -> bool {
    allowed.is_empty() || allowed.iter().any(|p| url.starts_with(p))
}

/// Fetch a remote URL under all guards, returning a `data:<mime>;base64,<...>`
/// URI. Follows up to [`MAX_REDIRECTS`] redirects, re-validating each hop.
async fn fetch_to_data_uri(
    url: &str,
    policy: &ImageFetchPolicy,
    cancel: &CancellationToken,
) -> Result<String, GatewayError> {
    let mut current = url.to_owned();

    for _hop in 0..=MAX_REDIRECTS {
        let parsed = reqwest::Url::parse(&current).map_err(|_| GatewayError::ImageUrlRejected)?;

        // Guard 1: scheme allowlist.
        let scheme = parsed.scheme().to_ascii_lowercase();
        if !policy
            .allowed_schemes
            .iter()
            .any(|s| s.eq_ignore_ascii_case(&scheme))
        {
            return Err(GatewayError::ImageUrlRejected);
        }
        // Guard 2 + 3: host and prefix allowlists.
        let host = parsed.host_str().ok_or(GatewayError::ImageUrlRejected)?;
        if !host_allowed(host, &policy.allowed_hosts)
            || !prefix_allowed(&current, &policy.allowed_url_prefixes)
        {
            // Server-side diagnostic only; the host is not returned to the client.
            tracing::debug!(host, "image fetch rejected: host/prefix not allowed");
            return Err(GatewayError::ImageUrlRejected);
        }
        // Guard 4: resolve DNS and block non-public addresses; pin the
        // connection to a vetted address (DNS-rebinding safe).
        let port = parsed
            .port_or_known_default()
            .ok_or(GatewayError::ImageUrlRejected)?;
        let addr = resolve_vetted_addr(host, port, policy, cancel).await?;

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(policy.timeout)
            .resolve(host, addr)
            .build()
            .map_err(|_| GatewayError::ImageFetchFailed)?;

        let resp = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(GatewayError::ImageFetchFailed),
            r = client.get(&current).send() => r.map_err(|_| GatewayError::ImageFetchFailed)?,
        };

        let status = resp.status();
        if status.is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or(GatewayError::ImageFetchFailed)?;
            // Resolve relative redirects against the current URL.
            current = parsed
                .join(location)
                .map_err(|_| GatewayError::ImageUrlRejected)?
                .to_string();
            continue;
        }
        if !status.is_success() {
            return Err(GatewayError::ImageFetchFailed);
        }

        // Guard 6: MIME allowlist (must be image/*).
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| {
                ct.split(';')
                    .next()
                    .unwrap_or(ct)
                    .trim()
                    .to_ascii_lowercase()
            })
            .filter(|ct| ct.starts_with("image/"))
            .ok_or(GatewayError::ImageUrlRejected)?;

        // Guard 5a: reject an advertised over-limit body before reading.
        if resp
            .content_length()
            .is_some_and(|len| len > policy.max_bytes)
        {
            return Err(GatewayError::ImageUrlRejected);
        }

        // Guard 5b: stream with a hard cap so a lying/absent Content-Length
        // can't blow the budget.
        let bytes = read_capped(resp, policy.max_bytes, cancel).await?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        return Ok(format!("data:{mime};base64,{b64}"));
    }

    // Too many redirects.
    Err(GatewayError::ImageFetchFailed)
}

/// Resolve `host:port`, rejecting the whole URL if *any* resolved address is
/// non-public (defends against split-horizon DNS returning a mix). Returns a
/// single vetted socket address to pin the connection to.
async fn resolve_vetted_addr(
    host: &str,
    port: u16,
    policy: &ImageFetchPolicy,
    cancel: &CancellationToken,
) -> Result<std::net::SocketAddr, GatewayError> {
    let allow_private = policy.allow_private_ips;
    let host_owned = host.to_owned();
    // std DNS resolution is blocking - keep it off the runtime. Bound it by the
    // per-fetch timeout and honor cancellation so a slow resolver cannot exceed
    // the budget or ignore a client disconnect.
    let lookup = tokio::task::spawn_blocking(move || {
        (host_owned.as_str(), port)
            .to_socket_addrs()
            .map(Iterator::collect::<Vec<SocketAddr>>)
    });
    let addrs: Vec<SocketAddr> = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(GatewayError::ImageFetchFailed),
        joined = tokio::time::timeout(policy.timeout, lookup) => joined
            .map_err(|_| GatewayError::ImageFetchFailed)?      // timed out
            .map_err(|_| GatewayError::ImageFetchFailed)?      // join error
            .map_err(|_| GatewayError::ImageFetchFailed)?,     // resolve error
    };

    let first = addrs
        .first()
        .copied()
        .ok_or(GatewayError::ImageFetchFailed)?;
    if !allow_private && addrs.iter().any(|a| !is_public_ip(&a.ip())) {
        // Server-side diagnostic only; the address is not returned to the client.
        tracing::debug!(
            host,
            "image fetch rejected: resolves to a non-public address"
        );
        return Err(GatewayError::ImageUrlRejected);
    }
    Ok(first)
}

/// Read a response body, aborting with [`GatewayError::ImageUrlRejected`] as
/// soon as the accumulated size would exceed `max_bytes`, and honoring
/// cancellation between chunks.
async fn read_capped(
    resp: reqwest::Response,
    max_bytes: u64,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, GatewayError> {
    let cap = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    loop {
        let chunk = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(GatewayError::ImageFetchFailed),
            next = stream.next() => match next {
                Some(Ok(bytes)) => bytes,
                Some(Err(_)) => return Err(GatewayError::ImageFetchFailed),
                None => break,
            },
        };
        if buf.len().saturating_add(chunk.len()) > cap {
            return Err(GatewayError::ImageUrlRejected);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Whether an IP address is a routable public address. Any loopback, private,
/// link-local, unique-local, unspecified, shared, documentation, or multicast
/// address returns `false` - the SSRF block that cannot be disabled.
#[must_use]
pub(crate) fn is_public_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || o[0] == 0
                // 100.64.0.0/10 carrier-grade NAT (shared).
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                // 192.0.0.0/24 IETF protocol assignments.
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                // 224.0.0.0/4 multicast and 240.0.0.0/4 reserved.
                || o[0] >= 224)
        }
        IpAddr::V6(v6) => {
            // IPv4-mapped (::ffff:a.b.c.d) - classify by the embedded v4.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(&IpAddr::V4(v4));
            }
            let seg = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                // fc00::/7 unique local.
                || (seg[0] & 0xfe00) == 0xfc00
                // fe80::/10 link-local unicast.
                || (seg[0] & 0xffc0) == 0xfe80
                // ff00::/8 multicast.
                || (seg[0] & 0xff00) == 0xff00
                // Transition ranges that embed an IPv4 address an attacker
                // could aim at an internal host - rejected outright (they are
                // deprecated/rare as public unicast, so the block is safe):
                //   ::/96      IPv4-compatible (first 96 bits zero)
                //   2002::/16  6to4
                //   2001:0::/32 Teredo
                //   64:ff9b::/96 NAT64 well-known prefix
                || seg[..6].iter().all(|s| *s == 0)
                || seg[0] == 0x2002
                || (seg[0] == 0x2001 && seg[1] == 0x0000)
                || (seg[0] == 0x0064 && seg[1] == 0xff9b))
        }
    }
}

/// Whether `host` is permitted by the allowlist. An empty allowlist permits any
/// host. A `.suffix` entry matches the domain and its subdomains; other entries
/// match exactly. Comparison is case-insensitive.
#[must_use]
pub(crate) fn host_allowed(host: &str, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    allowed.iter().any(|entry| {
        let entry = entry.to_ascii_lowercase();
        if let Some(suffix) = entry.strip_prefix('.') {
            host == suffix || host.ends_with(&format!(".{suffix}"))
        } else {
            host == entry
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn private_and_local_ips_are_not_public() {
        assert!(!is_public_ip(&v4(127, 0, 0, 1))); // loopback
        assert!(!is_public_ip(&v4(10, 0, 0, 5))); // private
        assert!(!is_public_ip(&v4(172, 16, 3, 4))); // private
        assert!(!is_public_ip(&v4(192, 168, 1, 1))); // private
        assert!(!is_public_ip(&v4(169, 254, 169, 254))); // link-local (cloud metadata)
        assert!(!is_public_ip(&v4(0, 0, 0, 0))); // unspecified
        assert!(!is_public_ip(&v4(100, 64, 0, 1))); // CGNAT shared
        assert!(!is_public_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!is_public_ip(&"fd00::1".parse().unwrap())); // ULA
        assert!(!is_public_ip(&"fe80::1".parse().unwrap())); // link-local
                                                             // IPv4-mapped loopback must be caught, not treated as public.
        assert!(!is_public_ip(&"::ffff:127.0.0.1".parse().unwrap()));
        // IPv6 transition ranges that embed IPv4 must not slip through
        // (attacker-controlled DNS could aim them at an internal v4 host).
        assert!(!is_public_ip(&"64:ff9b::7f00:1".parse().unwrap())); // NAT64 → 127.0.0.1
        assert!(!is_public_ip(&"2002:7f00:1::".parse().unwrap())); // 6to4
        assert!(!is_public_ip(&"2001:0:abcd::".parse().unwrap())); // Teredo
        assert!(!is_public_ip(&"::7f00:1".parse().unwrap())); // IPv4-compatible
    }

    #[test]
    fn public_ips_are_public() {
        assert!(is_public_ip(&v4(1, 1, 1, 1)));
        assert!(is_public_ip(&v4(8, 8, 8, 8)));
        assert!(is_public_ip(&"2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn empty_host_allowlist_permits_any() {
        assert!(host_allowed("example.com", &[]));
    }

    #[test]
    fn exact_and_suffix_host_matching() {
        let allow = vec!["cdn.example.com".to_owned(), ".mycompany.com".to_owned()];
        assert!(host_allowed("cdn.example.com", &allow));
        assert!(!host_allowed("evil.com", &allow));
        // Suffix entry matches the apex and subdomains.
        assert!(host_allowed("mycompany.com", &allow));
        assert!(host_allowed("assets.mycompany.com", &allow));
        // But not a look-alike that merely ends with the string.
        assert!(!host_allowed("notmycompany.com", &allow));
        // Case-insensitive.
        assert!(host_allowed("CDN.Example.COM", &allow));
    }
}
