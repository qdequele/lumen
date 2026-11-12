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

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
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

/// The DNS resolver of the shared fetch client: it answers ONLY with
/// addresses the guards vetted for a fetch that is in flight, and fails for
/// every other name. The client therefore has no way to reach an address
/// that did not pass [`resolve_vetted_addr`] for that exact host, and never
/// falls back to the system resolver (which a DNS-rebinding host could
/// answer differently the second time).
#[derive(Default)]
struct PinnedResolver {
    /// Host -> the pins of the hops currently fetching from it. Almost always
    /// zero or one entry per host; several only when concurrent fetches name
    /// the same host. Never held across an `.await`.
    pins: Mutex<HashMap<String, Vec<(u64, SocketAddr)>>>,
    /// Source of pin ids, so a hop removes its own pin and no other.
    next_id: AtomicU64,
}

impl PinnedResolver {
    /// Pin `host` to `addr` until the returned guard is dropped.
    fn pin(self: &Arc<Self>, host: &str, addr: SocketAddr) -> PinGuard {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.pins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(host.to_owned())
            .or_default()
            .push((id, addr));
        PinGuard {
            resolver: Arc::clone(self),
            host: host.to_owned(),
            id,
        }
    }

    /// The vetted IP to connect to for `host`, if a hop pinned one. With
    /// several concurrent hops on one host, the most recent pin: every pin was
    /// vetted for this host under the same policy, so any of them upholds the
    /// guard.
    ///
    /// Only the IP is answered, never the pinned port: pins are keyed by host,
    /// and concurrent hops may target different ports of one host
    /// (`http://h/` and `https://h/`, or an explicit port). The port always
    /// comes from the URL of the request being sent (see `resolve`).
    fn pinned(&self, host: &str) -> Option<IpAddr> {
        self.pins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(host)
            .and_then(|pins| pins.last())
            .map(|(_, addr)| addr.ip())
    }
}

impl reqwest::dns::Resolve for PinnedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let pinned = self.pinned(name.as_str());
        Box::pin(async move {
            match pinned {
                Some(ip) => {
                    // Port 0: the connector then applies the port of the
                    // request URL (explicit, or the scheme default), which is
                    // the port the guards vetted for this hop.
                    let addrs: reqwest::dns::Addrs =
                        Box::new(std::iter::once(SocketAddr::new(ip, 0)));
                    Ok(addrs)
                }
                // Fail closed: no vetted address, no connection.
                None => Err("image fetch: no vetted address for this host".into()),
            }
        })
    }
}

/// Removes one hop's pin when dropped, on every exit path (success, error,
/// cancellation).
struct PinGuard {
    resolver: Arc<PinnedResolver>,
    host: String,
    id: u64,
}

impl Drop for PinGuard {
    fn drop(&mut self) {
        let mut pins = self
            .resolver
            .pins
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(entries) = pins.get_mut(&self.host) {
            entries.retain(|(id, _)| *id != self.id);
            if entries.is_empty() {
                pins.remove(&self.host);
            }
        }
    }
}

/// The HTTP client every image fetch goes through, with the resolver that
/// pins it. Built once per process: building a `reqwest::Client` sets up the
/// rustls config and its root store, which used to be paid per image and per
/// redirect hop.
struct Fetcher {
    client: reqwest::Client,
    resolver: Arc<PinnedResolver>,
}

impl Fetcher {
    /// Build the client. Its properties are the ones each per-hop client had:
    ///
    /// - `no_proxy`: an environment proxy (`HTTPS_PROXY`...) would resolve the
    ///   target host itself, defeating both the vetted-address pin and the
    ///   private-IP block. Image fetches always go direct.
    /// - no redirect is followed by the client: each hop comes back to
    ///   [`fetch_to_data_uri`] and is re-validated by every guard.
    /// - DNS goes through [`PinnedResolver`] only.
    /// - no idle connection is kept: every hop dials again, so every hop
    ///   connects to an address vetted for it (a kept-alive connection would
    ///   reuse the address of an earlier fetch instead).
    ///
    /// The per-fetch timeout is set on each request, since it comes from the
    /// (hot-reloadable) policy.
    fn new() -> Result<Self, GatewayError> {
        let resolver = Arc::new(PinnedResolver::default());
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::clone(&resolver))
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|_| GatewayError::ImageFetchFailed)?;
        Ok(Self { client, resolver })
    }

    /// The process-wide fetcher. A build failure is not cached: the next
    /// fetch tries again, as the per-hop builds did.
    fn shared() -> Result<&'static Self, GatewayError> {
        static SHARED: OnceLock<Fetcher> = OnceLock::new();
        if let Some(fetcher) = SHARED.get() {
            return Ok(fetcher);
        }
        let built = Self::new()?;
        Ok(SHARED.get_or_init(|| built))
    }

    /// Send one GET to `url`, connecting only to `addr` (the address the
    /// guards vetted for the URL's host). The pin is held until the response
    /// head is in: the connection is established by then, and the body is
    /// read from that same connection.
    async fn get_pinned(
        &self,
        url: reqwest::Url,
        addr: SocketAddr,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response, GatewayError> {
        let host = url.host_str().ok_or(GatewayError::ImageUrlRejected)?;
        let _pin = self.resolver.pin(host, addr);
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(GatewayError::ImageFetchFailed),
            r = self.client.get(url).timeout(timeout).send() => {
                r.map_err(|_| GatewayError::ImageFetchFailed)
            }
        }
    }
}

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

/// Whether the URL falls under one of the allowed prefixes (empty =
/// unrestricted).
///
/// The comparison is made on the PARSED, normalised URL (the one the request
/// is actually sent to), never on the client's raw string: dot segments
/// (`/tenant-a/../tenant-b/`, also percent-encoded) are already resolved, the
/// host is lower-cased and default ports dropped. Each prefix is normalised
/// the same way; one that does not parse as a URL matches nothing (fail
/// closed). A prefix without a query or fragment must end on a path boundary:
/// `https://h/tenant-a` covers `https://h/tenant-a/x.png` but neither
/// `https://h/tenant-ab/x.png` nor `https://h.evil.example/`.
fn prefix_allowed(url: &reqwest::Url, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let target = url.as_str();
    allowed.iter().any(|raw| {
        let Ok(prefix) = reqwest::Url::parse(raw) else {
            tracing::debug!("image fetch: an allowed_url_prefixes entry is not a valid URL");
            return false;
        };
        let p = prefix.as_str();
        let Some(rest) = target.strip_prefix(p) else {
            return false;
        };
        if prefix.query().is_some() || prefix.fragment().is_some() {
            // The operator pinned part of the query: a plain prefix match.
            return true;
        }
        p.ends_with('/') || rest.is_empty() || rest.starts_with(['/', '?', '#'])
    })
}

/// Fetch a remote URL under all guards, returning a `data:<mime>;base64,<...>`
/// URI. Follows up to [`MAX_REDIRECTS`] redirects, re-validating each hop.
async fn fetch_to_data_uri(
    url: &str,
    policy: &ImageFetchPolicy,
    cancel: &CancellationToken,
) -> Result<String, GatewayError> {
    let fetcher = Fetcher::shared()?;
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
            || !prefix_allowed(&parsed, &policy.allowed_url_prefixes)
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

        // Send the parsed URL that the guards vetted, not the raw string,
        // through the shared client pinned to the vetted address for this
        // hop (no proxy, no client-side redirect, see [`Fetcher`]).
        let resp = fetcher
            .get_pinned(parsed.clone(), addr, policy.timeout, cancel)
            .await?;

        let status = resp.status();
        if status.is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or(GatewayError::ImageFetchFailed)?;
            // Resolve relative redirects against the current URL. The next
            // hop is re-parsed and re-validated by every guard above.
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
pub fn is_public_ip(ip: &IpAddr) -> bool {
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
pub fn host_allowed(host: &str, allowed: &[String]) -> bool {
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
    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).unwrap()
    }

    #[test]
    fn empty_prefix_allowlist_permits_any() {
        assert!(prefix_allowed(&url("https://h.example/x.png"), &[]));
    }

    #[test]
    fn prefix_is_matched_on_the_normalised_url() {
        let allow = vec!["https://h.example/tenant-a/".to_owned()];
        assert!(prefix_allowed(
            &url("https://h.example/tenant-a/x.png"),
            &allow
        ));
        // Dot segments (plain or percent-encoded) are resolved before matching.
        assert!(!prefix_allowed(
            &url("https://h.example/tenant-a/../tenant-b/x.png"),
            &allow
        ));
        assert!(!prefix_allowed(
            &url("https://h.example/tenant-a/%2e%2e/tenant-b/x.png"),
            &allow
        ));
        // Host case and a default port do not defeat a legitimate match.
        assert!(prefix_allowed(
            &url("https://H.Example:443/tenant-a/x.png"),
            &allow
        ));
    }

    #[test]
    fn prefix_match_ends_on_a_path_boundary() {
        let allow = vec!["https://h.example/tenant-a".to_owned()];
        assert!(prefix_allowed(&url("https://h.example/tenant-a"), &allow));
        assert!(prefix_allowed(
            &url("https://h.example/tenant-a/x.png"),
            &allow
        ));
        assert!(prefix_allowed(
            &url("https://h.example/tenant-a?x=1"),
            &allow
        ));
        assert!(!prefix_allowed(
            &url("https://h.example/tenant-ab/x.png"),
            &allow
        ));
        // A host-only prefix cannot be extended into another host.
        let host_only = vec!["https://h.example".to_owned()];
        assert!(prefix_allowed(&url("https://h.example/x.png"), &host_only));
        assert!(!prefix_allowed(
            &url("https://h.example.evil.test/x.png"),
            &host_only
        ));
        assert!(!prefix_allowed(
            &url("https://h.example@evil.test/x.png"),
            &host_only
        ));
    }

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn image_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/img.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"PNG".to_vec()))
            .mount(&server)
            .await;
        server
    }

    /// The shared client reaches a NAMED host only through the address pinned
    /// for the hop: the name below does not exist in DNS, so a 200 proves the
    /// connection went to the pinned address.
    #[tokio::test]
    async fn pinned_host_is_fetched_from_the_vetted_address() {
        let server = image_server().await;
        let fetcher = Fetcher::new().unwrap();
        let url = url(&format!(
            "http://pinned.invalid:{}/img.png",
            server.address().port()
        ));
        let resp = fetcher
            .get_pinned(
                url,
                *server.address(),
                Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    /// Fail closed: without a pin the client resolves nothing, not even a
    /// name the system resolver knows (`localhost`), and a pin is gone as
    /// soon as its hop ends, with no idle connection left to reuse.
    #[tokio::test]
    async fn unpinned_host_is_never_resolved_and_pins_do_not_outlive_their_hop() {
        let server = image_server().await;
        let fetcher = Fetcher::new().unwrap();
        let target = url(&format!(
            "http://localhost:{}/img.png",
            server.address().port()
        ));

        // No pin at all: the system resolver would answer for `localhost`.
        let unpinned = fetcher.client.get(target.clone()).send().await;
        assert!(unpinned.is_err(), "{unpinned:?}");

        // A pinned hop works, and reads its body.
        let resp = fetcher
            .get_pinned(
                target.clone(),
                *server.address(),
                Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(resp.bytes().await.unwrap().as_ref(), b"PNG");
        assert!(fetcher.resolver.pinned("localhost").is_none());

        // After the hop: no pin, and no kept-alive connection to fall back on.
        let after = fetcher.client.get(target).send().await;
        assert!(after.is_err(), "{after:?}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    /// A pin is scoped to its host and to its guard: other hosts stay
    /// unresolvable, and concurrent hops on one host each remove only their
    /// own pin.
    #[test]
    fn pins_are_per_host_and_removed_by_their_own_guard() {
        let resolver = Arc::new(PinnedResolver::default());
        let a: SocketAddr = "1.1.1.1:443".parse().unwrap();
        let b: SocketAddr = "8.8.8.8:443".parse().unwrap();

        let first = resolver.pin("h.example", a);
        assert_eq!(resolver.pinned("h.example"), Some(a.ip()));
        assert_eq!(resolver.pinned("other.example"), None);

        let second = resolver.pin("h.example", b);
        assert_eq!(resolver.pinned("h.example"), Some(b.ip()));
        drop(first);
        assert_eq!(resolver.pinned("h.example"), Some(b.ip()));
        drop(second);
        assert_eq!(resolver.pinned("h.example"), None);
        assert!(resolver.pins.lock().unwrap().is_empty());
    }

    /// The port of a connection comes from the request URL, never from a
    /// pin: a concurrent hop on another port of the same host (here a pin
    /// for port 1, still held) must not redirect this hop there.
    #[tokio::test]
    async fn a_pin_for_another_port_of_the_host_does_not_change_the_port_dialled() {
        let server = image_server().await;
        let fetcher = Fetcher::new().unwrap();
        let ip = server.address().ip();
        let target = url(&format!(
            "http://pinned.invalid:{}/img.png",
            server.address().port()
        ));

        let hop = fetcher.resolver.pin("pinned.invalid", *server.address());
        // Pinned later, so it is the pin the resolver answers with.
        let other_port = fetcher
            .resolver
            .pin("pinned.invalid", SocketAddr::new(ip, 1));

        let resp = fetcher
            .client
            .get(target)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        drop((hop, other_port));
    }

    /// A cancelled hop drops its pin.
    #[tokio::test]
    async fn cancelled_hop_releases_its_pin() {
        let fetcher = Fetcher::new().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        // TEST-NET-1: never connected to, the cancelled branch wins first.
        let err = fetcher
            .get_pinned(
                url("http://h.example/img.png"),
                "192.0.2.1:80".parse().unwrap(),
                Duration::from_secs(5),
                &cancel,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, GatewayError::ImageFetchFailed), "{err:?}");
        assert!(fetcher.resolver.pinned("h.example").is_none());
    }

    #[test]
    fn unparseable_prefix_matches_nothing() {
        let allow = vec!["not a url".to_owned()];
        assert!(!prefix_allowed(&url("https://h.example/x.png"), &allow));
    }
}
