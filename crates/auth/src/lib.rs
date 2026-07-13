//! Authentication, virtual keys, quotas and hard budgets for Ferrogate.
//!
//! Budget enforcement happens *inside* the request path, before any upstream
//! call, so an exhausted budget can never leak spend to a provider — but the
//! database is never touched on that path: key state lives in memory and is
//! flushed to SQLite periodically (M5 §5.2).
//!
//! Module map:
//!
//! * [`key`] — virtual-key generation and hashing (plaintext never stored).
//! * [`crypto`] — AES-256-GCM sealing of provider keys at rest.
//! * [`store`] — the SQLite store (sqlx) with embedded migrations.
//! * [`state`] — the in-memory key table the request path enforces against.

pub mod crypto;
pub mod error;
pub mod key;
pub mod state;
pub mod store;

pub use error::AuthError;

/// Current unix time in whole seconds.
///
/// Clamps instead of failing on a pre-epoch clock — a nonsensical system time
/// must never take the gateway down.
pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
