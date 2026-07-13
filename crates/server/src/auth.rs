//! Virtual-key authentication for the `/v1` surface (M5 §5.2).
//!
//! The middleware resolves the presented bearer key against the in-memory
//! [`AuthState`] — a hash lookup, no database — and stores the live
//! [`KeyEntry`] in request extensions for the handlers' budget/quota
//! admission. Unknown, disabled and expired keys are indistinguishable to the
//! caller (FG-4004, 401). When auth is disabled in config the middleware is a
//! no-op and the gateway stays open.

use crate::state::AppState;
use axum::extract::{Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use ferrogate_auth::key::hash_key;
use ferrogate_auth::state::{AuthState, KeyEntry};
use ferrogate_auth::store::KeyStore;
use ferrogate_core::GatewayError;
use std::sync::Arc;

/// Everything the server keeps when auth is enabled.
pub struct AuthRuntime {
    /// The in-memory key table the request path enforces against.
    pub keys: AuthState,
    /// The SQLite store (admin API, flushes, usage log) — never consulted on
    /// the request path.
    pub store: KeyStore,
    /// BLAKE3 hash of the `FERROGATE_MASTER_KEY` value; the admin API
    /// compares hashes so the raw admin token is never retained as text.
    pub admin_token_hash: String,
    /// The parsed master key, for sealing provider keys at rest. `None` in
    /// test setups that exercise auth without encryption.
    pub master: Option<ferrogate_auth::crypto::MasterKey>,
}

impl std::fmt::Debug for AuthRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Nothing secret in here (the hash is one-way), but keep the output
        // minimal anyway.
        f.debug_struct("AuthRuntime")
            .field("keys", &self.keys.len())
            .finish_non_exhaustive()
    }
}

/// Request-extension wrapper for the authenticated key.
#[derive(Clone)]
pub struct AuthedKey(pub Arc<KeyEntry>);

/// Current unix time in whole seconds (clamped, never panics).
#[must_use]
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Constant-time string equality. Comparing BLAKE3 *hashes* already makes a
/// timing oracle useless (it leaks the hash, not a preimage), but there is no
/// reason to leak even that.
fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0_u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Extract the value of a `Bearer` authorization header.
fn bearer(request: &Request) -> Option<&str> {
    request
        .headers()
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

/// `/v1/*` middleware: authenticate the virtual key (when auth is enabled)
/// and expose it to handlers via extensions.
pub async fn require_virtual_key(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(auth) = state.auth.clone() else {
        // Auth disabled: open gateway, no key context.
        return next.run(request).await;
    };
    let entry = bearer(&request).and_then(|key| auth.keys.authenticate(key, now_unix()));
    match entry {
        Some(entry) => {
            request.extensions_mut().insert(AuthedKey(entry));
            next.run(request).await
        }
        None => crate::error::ApiError::from(GatewayError::Unauthorized).into_response(),
    }
}

/// `/admin/*` middleware: require the master key as a bearer token. Compared
/// by BLAKE3 hash so the raw value never sits in server state.
pub async fn require_master_key(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(auth) = state.auth.clone() else {
        // No auth runtime → no admin surface.
        return crate::error::ApiError::from(GatewayError::Unauthorized).into_response();
    };
    let authorized = bearer(&request)
        .is_some_and(|token| constant_time_eq(&hash_key(token), &auth.admin_token_hash));
    if authorized {
        next.run(request).await
    } else {
        crate::error::ApiError::from(GatewayError::Unauthorized).into_response()
    }
}
