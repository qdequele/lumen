//! Per-account scoping of the admin surface (platform contract v2, section
//! 8.3). A hosted gateway serves many Lab accounts through one master key,
//! so the gateway enforces the account boundary itself: an `/admin/*`
//! request carrying `X-Lumen-Account-Ref: <uuid>` sees only keys and groups
//! whose group `account_ref` equals it, may create only inside that
//! account, and is refused (403 `LM-4005`) on the platform-only routes
//! (providers, config, webhooks, the OpenAPI document). Without the header
//! the master key keeps its unscoped, platform-operator behaviour: every
//! guard below is a no-op and costs no database read.
//!
//! Every out-of-scope id (a key or group of another account, a key with no
//! group) answers the exact `404 LM-1003` an unknown id gets, so a scoped
//! caller cannot tell a foreign id from one that never existed.

use crate::error::ApiError;
use axum::extract::{FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use lumen_auth::store::{GroupRecord, KeyStore, VirtualKeyRecord};
use lumen_core::GatewayError;
use std::collections::HashSet;

/// The scoping header.
pub const ACCOUNT_HEADER: &str = "X-Lumen-Account-Ref";

/// The account an admin call is scoped to: `None` without the header.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccountScope(Option<String>);

impl AccountScope {
    /// Read the header: absent is unscoped; a value that is not a UUID, or
    /// the header sent more than once, is `LM-1001` (never read as
    /// "unscoped", which would widen the call to every account). A valid
    /// UUID is kept in canonical lowercase.
    ///
    /// # Errors
    /// `LM-1001` when the header is present but not exactly one UUID.
    pub fn from_headers(headers: &HeaderMap) -> Result<Self, ApiError> {
        let mut values = headers.get_all(ACCOUNT_HEADER).iter();
        let Some(value) = values.next() else {
            return Ok(Self(None));
        };
        let invalid = || {
            ApiError::from(GatewayError::InvalidRequest(format!(
                "{ACCOUNT_HEADER} must be exactly one Lab account UUID"
            )))
        };
        if values.next().is_some() {
            return Err(invalid());
        }
        let account = value
            .to_str()
            .ok()
            .map(str::trim)
            .filter(|v| lumen_auth::billing::is_uuid(v))
            .ok_or_else(invalid)?;
        // Canonical lowercase, so lists, foreign-id checks and a forced
        // `account_ref` on a scoped create all agree whatever case the
        // caller sent.
        Ok(Self(Some(account.to_ascii_lowercase())))
    }

    /// The scoped account, if any.
    #[must_use]
    pub fn account(&self) -> Option<&str> {
        self.0.as_deref()
    }

    /// Whether `group` belongs to the scoped account (always, unscoped).
    #[must_use]
    pub fn allows_group(&self, group: &GroupRecord) -> bool {
        self.0
            .as_deref()
            .is_none_or(|account| group.account_ref.as_deref() == Some(account))
    }

    /// The group, unless it is unknown, a tombstone or out of scope: all
    /// three are the same 404, so a scoped caller cannot probe other ids.
    /// Always reads the store (the group view needs the record).
    ///
    /// # Errors
    /// `LM-1003`, or `LM-5001` on a store failure.
    pub async fn require_group(&self, store: &KeyStore, id: &str) -> Result<GroupRecord, ApiError> {
        store
            .get_group(id)
            .await
            .map_err(|e| internal(&e))?
            .filter(|group| self.allows_group(group))
            .ok_or_else(|| unknown_group(id))
    }

    /// [`require_group`](Self::require_group) on a scoped call; a no-op
    /// (no store read, so the unscoped behaviour is untouched) otherwise.
    ///
    /// # Errors
    /// `LM-1003`, or `LM-5001` on a store failure.
    pub async fn guard_group(&self, store: &KeyStore, id: &str) -> Result<(), ApiError> {
        if self.0.is_some() {
            self.require_group(store, id).await?;
        }
        Ok(())
    }

    /// On a scoped call, refuse a key that is unknown, a tombstone, has no
    /// group or has a group of another account: the same 404 for all of
    /// them. A no-op unscoped.
    ///
    /// # Errors
    /// `LM-1003`, or `LM-5001` on a store failure.
    pub async fn guard_key(&self, store: &KeyStore, id: &str) -> Result<(), ApiError> {
        if self.0.is_none() {
            return Ok(());
        }
        let key = store.get_key(id).await.map_err(|e| internal(&e))?;
        self.check_key(store, id, key).await
    }

    /// [`guard_key`](Self::guard_key) for the delete route, where a
    /// tombstoned key of the account passes: a retried delete must still
    /// reach the handler's unconditional in-memory eviction (which then
    /// answers the usual 404), or a key whose first delete was cut off after
    /// the database write would keep authenticating until a restart.
    ///
    /// # Errors
    /// `LM-1003`, or `LM-5001` on a store failure.
    pub async fn guard_key_delete(&self, store: &KeyStore, id: &str) -> Result<(), ApiError> {
        if self.0.is_none() {
            return Ok(());
        }
        let key = store
            .get_key_including_deleted(id)
            .await
            .map_err(|e| internal(&e))?;
        self.check_key(store, id, key).await
    }

    async fn check_key(
        &self,
        store: &KeyStore,
        id: &str,
        key: Option<VirtualKeyRecord>,
    ) -> Result<(), ApiError> {
        let Some(group_id) = key.and_then(|key| key.group_id) else {
            return Err(unknown_key(id));
        };
        let in_scope = store
            .get_group(&group_id)
            .await
            .map_err(|e| internal(&e))?
            .is_some_and(|group| self.allows_group(&group));
        if in_scope {
            Ok(())
        } else {
            Err(unknown_key(id))
        }
    }

    /// Keep only the keys whose group is in scope (every key, unscoped).
    /// Tombstoned groups count, so `?include_deleted=true` stays inside the
    /// account too.
    ///
    /// # Errors
    /// `LM-5001` on a store failure.
    pub async fn filter_keys(
        &self,
        store: &KeyStore,
        keys: Vec<VirtualKeyRecord>,
    ) -> Result<Vec<VirtualKeyRecord>, ApiError> {
        let Some(account) = self.0.as_deref() else {
            return Ok(keys);
        };
        let groups: HashSet<String> = store
            .list_groups(true)
            .await
            .map_err(|e| internal(&e))?
            .into_iter()
            .filter(|group| group.account_ref.as_deref() == Some(account))
            .map(|group| group.id)
            .collect();
        Ok(keys
            .into_iter()
            .filter(|key| key.group_id.as_ref().is_some_and(|g| groups.contains(g)))
            .collect())
    }
}

impl<S: Send + Sync> FromRequestParts<S> for AccountScope {
    type Rejection = ApiError;

    // No await inside: the header is parsed synchronously, so the future is
    // ready at once (and `clippy::unused_async` stays quiet).
    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        std::future::ready(Self::from_headers(&parts.headers))
    }
}

/// The 404 `LM-1003` an unknown key id gets in `admin` (same message).
fn unknown_key(id: &str) -> ApiError {
    GatewayError::NotFound(format!("unknown key id '{id}'")).into()
}

/// The 404 `LM-1003` an unknown group id gets in `admin` (same message).
fn unknown_group(id: &str) -> ApiError {
    GatewayError::NotFound(format!("unknown group id '{id}'")).into()
}

fn internal(error: &lumen_auth::AuthError) -> ApiError {
    GatewayError::Internal(error.to_string()).into()
}

/// Middleware for the platform-only routes: `403 LM-4005` when the call
/// carries the scoping header (whatever its value), untouched otherwise.
pub async fn platform_only(request: Request, next: Next) -> Response {
    if request.headers().contains_key(ACCOUNT_HEADER) {
        return ApiError::from(GatewayError::PlatformOnly).into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const A: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";

    #[test]
    fn the_header_is_read_strictly() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            AccountScope::from_headers(&headers).ok(),
            Some(AccountScope::default())
        );
        headers.insert(ACCOUNT_HEADER, HeaderValue::from_static(A));
        assert_eq!(
            AccountScope::from_headers(&headers).ok(),
            Some(AccountScope(Some(A.to_owned())))
        );
        headers.append(ACCOUNT_HEADER, HeaderValue::from_static(A));
        assert!(AccountScope::from_headers(&headers).is_err(), "twice");
        for bad in ["", "acme", "0192f3c1-7c2e-7b1a-9f00"] {
            let mut headers = HeaderMap::new();
            headers.insert(ACCOUNT_HEADER, HeaderValue::from_static(bad));
            let error = AccountScope::from_headers(&headers).unwrap_err();
            assert_eq!(error.0.code(), "LM-1001", "{bad:?}");
        }
    }

    #[test]
    fn allows_group_matches_the_account_exactly() {
        let group = |account_ref: Option<&str>| GroupRecord {
            id: "g".to_owned(),
            name: "g".to_owned(),
            budget_max: None,
            budget_spent: 0.0,
            created_at: 0,
            deleted_at: None,
            account_ref: account_ref.map(str::to_owned),
        };
        let scoped = AccountScope(Some(A.to_owned()));
        assert!(scoped.allows_group(&group(Some(A))));
        assert!(!scoped.allows_group(&group(None)));
        assert!(!scoped.allows_group(&group(Some("other"))));
        assert!(AccountScope::default().allows_group(&group(None)));
    }
}
