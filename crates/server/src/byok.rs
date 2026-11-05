//! Team provider key pools (BYOK) on the request path.
//!
//! A budget group (one hosted Lab account) may bring its own API keys for a
//! provider: a *pool* of up to 10 keys, plus a flag allowing the fallback to
//! the platform's own key. [`TeamPools`] holds every pool in memory, rebuilt
//! by the reloader from the encrypted store ([`refresh_pools`]); the request
//! path never touches the database.
//!
//! Per request, [`RequestKeys`] snapshots the pools of the authenticated
//! key's group and decides, for each attempt of the fallback chain, which key
//! the upstream call uses ([`RequestKeys::attempt`]):
//!
//! 1. no pool for the provider: the platform key, unless an earlier link of
//!    the same request found its pool exhausted with the fallback off (then
//!    the attempt is refused, so a chain never reaches the platform's keys);
//! 2. a pool: its keys round-robin; a rate-limited key is benched
//!    `max(retry_after, 30s)`, a key answering 401/403 is marked failing and
//!    benched 10 minutes, and the next key is tried; any other result is the
//!    attempt's result;
//! 3. no usable key left: the platform key when the group turned the
//!    fallback on, else `LM-3021` ([`ProviderError::TeamKeysExhausted`]).
//!
//! Pools are keyed by the group of the authenticated virtual key, never by
//! anything the client sends: a key with no group never uses a team key, and
//! one group's keys never serve another group. Secrets are held in
//! [`Zeroizing`] strings, never logged and never in `Debug` output.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use arc_swap::ArcSwap;
use lumen_auth::crypto::MasterKey;
use lumen_auth::store::{GroupProviderKeyRecord, KeyStore};
use lumen_auth::AuthError;
use lumen_core::ProviderError;
use lumen_providers::{ProviderKind, ProviderSet, Registry};
use lumen_router::executor::Link;
use zeroize::Zeroizing;

use crate::auth::AuthedKey;

/// Shortest bench for a rate-limited key.
const MIN_RATE_LIMIT_BENCH: Duration = Duration::from_secs(30);
/// Bench for a key the provider rejected (401/403).
const FAILING_BENCH: Duration = Duration::from_secs(600);

/// Which key served an attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Served {
    /// The platform's own provider key: tokens are billed.
    Platform,
    /// One of the group's team keys: tokens are not billed.
    Team {
        /// The team key's id (`gpk_...`), never its secret.
        key_id: String,
    },
}

/// The last observed health of a team key.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Health {
    /// Never used since the gateway started.
    Unused,
    /// Its last call succeeded.
    Ok,
    /// The provider rate-limited it; benched until `until_ms`.
    RateLimited { until_ms: i64 },
    /// The provider rejected it; benched until `until_ms`.
    Failing { until_ms: i64, reason: String },
}

/// One team key of a pool.
pub struct PoolKey {
    id: String,
    secret: Zeroizing<String>,
    health: Mutex<Health>,
    /// The provider instances built with this key, and the registry
    /// generation they were built under.
    cached: Mutex<Option<(u64, Arc<ProviderSet>)>>,
}

// Hand-written so the secret can never be printed.
impl std::fmt::Debug for PoolKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolKey")
            .field("id", &self.id)
            .field("secret", &"<redacted>")
            .field("health", &*lock(&self.health))
            .finish_non_exhaustive()
    }
}

impl PoolKey {
    fn new(id: String, secret: Zeroizing<String>) -> Self {
        Self {
            id,
            secret,
            health: Mutex::new(Health::Unused),
            cached: Mutex::new(None),
        }
    }

    fn benched(&self, now_ms: i64) -> bool {
        match &*lock(&self.health) {
            Health::RateLimited { until_ms } | Health::Failing { until_ms, .. } => {
                now_ms < *until_ms
            }
            Health::Unused | Health::Ok => false,
        }
    }

    fn set_health(&self, health: Health) {
        *lock(&self.health) = health;
    }

    /// This key's provider instances for the live registry, rebuilt when the
    /// registry was reloaded since they were cached.
    fn providers(&self, registry: &Registry, provider: &str) -> Option<Arc<ProviderSet>> {
        let generation = registry.generation();
        let mut cached = lock(&self.cached);
        if let Some((built_at, set)) = cached.as_ref() {
            if *built_at == generation {
                return Some(Arc::clone(set));
            }
        }
        let set = registry.rekeyed(provider, &self.secret)?;
        *cached = Some((generation, Arc::clone(&set)));
        Some(set)
    }
}

/// One group's keys for one provider.
#[derive(Debug)]
pub struct Pool {
    keys: Vec<Arc<PoolKey>>,
    fallback: bool,
    cursor: AtomicUsize,
}

/// group id -> provider name -> pool.
type GroupPools = HashMap<String, Arc<HashMap<String, Arc<Pool>>>>;

/// Every group's team key pools, swapped whole on reload.
#[derive(Default)]
pub struct TeamPools {
    pools: ArcSwap<GroupPools>,
}

impl std::fmt::Debug for TeamPools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamPools")
            .field("groups", &self.pools.load().len())
            .finish()
    }
}

impl TeamPools {
    /// Replace every pool with `keys` (decrypted store rows) and `settings`
    /// (`(group_id, provider, fallback)` rows). A key already present keeps
    /// its health and cached instances, and a pool keeps its cursor; a key
    /// absent from `keys` is gone for every new request (one in flight
    /// finishes on its own snapshot). A setting without keys makes no pool.
    pub fn replace(
        &self,
        keys: Vec<(GroupProviderKeyRecord, Zeroizing<String>)>,
        settings: &[(String, String, bool)],
    ) {
        let old = self.pools.load_full();
        let mut grouped: HashMap<String, HashMap<String, Vec<Arc<PoolKey>>>> = HashMap::new();
        for (record, secret) in keys {
            let previous = old
                .get(&record.group_id)
                .and_then(|providers| providers.get(&record.provider))
                .and_then(|pool| pool.keys.iter().find(|k| k.id == record.id))
                .cloned();
            let key = previous.unwrap_or_else(|| Arc::new(PoolKey::new(record.id, secret)));
            grouped
                .entry(record.group_id)
                .or_default()
                .entry(record.provider)
                .or_default()
                .push(key);
        }
        let mut next = GroupPools::new();
        for (group, providers) in grouped {
            let mut built = HashMap::new();
            for (provider, keys) in providers {
                let fallback = settings
                    .iter()
                    .any(|(g, p, on)| *on && *g == group && *p == provider);
                let cursor = old
                    .get(&group)
                    .and_then(|providers| providers.get(&provider))
                    .map_or(0, |pool| pool.cursor.load(Ordering::Relaxed));
                built.insert(
                    provider,
                    Arc::new(Pool {
                        keys,
                        fallback,
                        cursor: AtomicUsize::new(cursor),
                    }),
                );
            }
            next.insert(group, Arc::new(built));
        }
        self.pools.store(Arc::new(next));
    }

    /// The live status of one team key: `unused` | `ok` | `rate_limited` |
    /// `failing`, with a secret-free detail when it is not healthy. A key the
    /// pools do not hold (not loaded yet, or its provider no longer takes a
    /// team key) is `unused`.
    #[must_use]
    pub fn status(&self, group_id: &str, key_id: &str) -> (&'static str, Option<String>) {
        let pools = self.pools.load();
        let key = pools.get(group_id).and_then(|providers| {
            providers
                .values()
                .find_map(|pool| pool.keys.iter().find(|k| k.id == key_id))
        });
        let Some(key) = key else {
            return ("unused", None);
        };
        let health = lock(&key.health).clone();
        match health {
            Health::Unused => ("unused", None),
            Health::Ok => ("ok", None),
            Health::RateLimited { .. } => (
                "rate_limited",
                Some("the provider rate-limited this key".to_owned()),
            ),
            Health::Failing { reason, .. } => ("failing", Some(reason)),
        }
    }

    /// Drop one key from the live pools now, without waiting for the
    /// reloader (which stays the reconciler): a deleted key serves no new
    /// request once the delete has answered. A request already holding a
    /// snapshot finishes on it. A pool left without keys goes away.
    pub fn remove(&self, group_id: &str, provider: &str, key_id: &str) {
        self.pools.rcu(|current| {
            let mut next = GroupPools::clone(current);
            let Some(providers) = current.get(group_id) else {
                return next;
            };
            let mut providers = HashMap::clone(providers);
            if let Some(pool) = providers.get(provider) {
                let keys: Vec<_> = pool
                    .keys
                    .iter()
                    .filter(|k| k.id != key_id)
                    .cloned()
                    .collect();
                if keys.is_empty() {
                    providers.remove(provider);
                } else {
                    let cursor = pool.cursor.load(Ordering::Relaxed) % keys.len();
                    providers.insert(
                        provider.to_owned(),
                        Arc::new(Pool {
                            keys,
                            fallback: pool.fallback,
                            cursor: AtomicUsize::new(cursor),
                        }),
                    );
                }
            }
            if providers.is_empty() {
                next.remove(group_id);
            } else {
                next.insert(group_id.to_owned(), Arc::new(providers));
            }
            next
        });
    }

    fn group(&self, group_id: &str) -> Option<Arc<HashMap<String, Arc<Pool>>>> {
        self.pools.load().get(group_id).cloned()
    }
}

/// Rebuild `pools` from the store: every team key (decrypted) and fallback
/// setting, keeping only pools whose provider is configured in `registry`
/// with a kind that takes a team key (a whole-document config write can drop
/// a provider or change its kind without deleting its pool rows). Runs in the
/// reloader, never on the request path.
///
/// # Errors
/// A store or decryption error; the pools are then left as they were.
pub async fn refresh_pools(
    pools: &TeamPools,
    store: &KeyStore,
    master: &MasterKey,
    registry: &Registry,
) -> Result<(), AuthError> {
    let keys = store.load_group_provider_keys(master).await?;
    let settings = store.group_provider_settings().await?;
    let eligible = |provider: &str| {
        registry
            .provider_kind(provider)
            .is_some_and(ProviderKind::byok_eligible)
    };
    let keys: Vec<_> = keys
        .into_iter()
        .filter(|(record, _)| eligible(&record.provider))
        .collect();
    pools.replace(keys, &settings);
    Ok(())
}

/// The team key pools one request may use: those of its authenticated key's
/// group, snapshotted at the start of the request.
#[derive(Debug)]
pub struct RequestKeys {
    /// The authenticated key's budget group, when it has any pool.
    group: Option<String>,
    pools: Option<Arc<HashMap<String, Arc<Pool>>>>,
    /// The first link of the chain (index, provider) served by a team pool
    /// with the fallback off, decided up front from chain order: every later
    /// link without a team pool refuses, whatever happened to that link (an
    /// error, a first-token timeout, an open circuit that skipped it).
    chain_lock: Option<(usize, String)>,
    /// The provider whose team pool (fallback off) failed this request: a
    /// later link without a pool refuses instead of using the platform key.
    locked: Mutex<Option<String>>,
}

impl RequestKeys {
    /// The pools of `key`'s group; none when the request has no
    /// authenticated key or the key has no group.
    #[must_use]
    pub fn new(pools: &Arc<TeamPools>, key: Option<&AuthedKey>) -> Self {
        let group = key.and_then(|AuthedKey(entry)| entry.group_id());
        let pools = group.as_deref().and_then(|group| pools.group(group));
        Self {
            group: group.filter(|_| pools.is_some()),
            pools,
            chain_lock: None,
            locked: Mutex::new(None),
        }
    }

    /// Fix the chain this request runs, in link order (`providers[i]` is
    /// link `i`'s provider), so [`attempt`](Self::attempt) can refuse every
    /// link after the first one served by a team pool with the fallback off.
    #[must_use]
    pub fn for_chain<'a>(
        mut self,
        registry: &Registry,
        providers: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        self.chain_lock = providers.into_iter().enumerate().find_map(|(i, p)| {
            self.team_pool(registry, p)
                .filter(|pool| !pool.fallback)
                .map(|_| (i, p.to_owned()))
        });
        self
    }

    /// Give each link the request serves from a team pool its own circuit
    /// breaker scope (the group, ADR 018): a link whose provider has a
    /// usable team pool (see [`has_usable_pool`](Self::has_usable_pool)) of
    /// an eligible kind. Every other link keeps the platform breaker.
    pub fn scope_links(&self, registry: &Registry, links: &mut [Link]) {
        let now = now_ms();
        for link in links {
            let usable = self
                .team_pool(registry, &link.provider_name)
                .is_some_and(|pool| pool.keys.iter().any(|k| !k.benched(now)));
            link.breaker_scope = self.group.clone().filter(|_| usable);
        }
    }

    /// Whether the group has a pool for `provider` with a key not benched
    /// right now (the admission estimate then reserves no token cost).
    #[must_use]
    pub fn has_usable_pool(&self, provider: &str) -> bool {
        let now = now_ms();
        self.pool(provider)
            .is_some_and(|pool| pool.keys.iter().any(|k| !k.benched(now)))
    }

    fn pool(&self, provider: &str) -> Option<&Arc<Pool>> {
        self.pools.as_ref()?.get(provider)
    }

    /// The group's pool for `provider`, if the live registry still has the
    /// provider with a kind that takes a team key (rows are filtered at load,
    /// and checked again here: a reload may have changed the kind).
    fn team_pool(&self, registry: &Registry, provider: &str) -> Option<&Arc<Pool>> {
        let eligible = registry
            .provider_kind(provider)
            .is_some_and(ProviderKind::byok_eligible);
        self.pool(provider).filter(|_| eligible)
    }

    /// Why a link without a team pool must refuse, if it must.
    fn refusal(&self, link: usize) -> Option<String> {
        if let Some((first, provider)) = &self.chain_lock {
            if link > *first {
                return Some(provider.clone());
            }
        }
        lock(&self.locked).clone()
    }

    /// Run one attempt for link `link` of the chain, served by `provider`:
    /// `keyed` with each usable team key's provider instances, or `platform`
    /// with the platform key, per the module rules. `platform` is called at
    /// most once, and never on a refused link.
    ///
    /// # Errors
    /// The attempt's own error, or [`ProviderError::TeamKeysExhausted`] when
    /// the pool has no usable key and the fallback is off, or when `provider`
    /// has no pool and the link comes after one served by a pool with the
    /// fallback off (see [`for_chain`](Self::for_chain)).
    pub async fn attempt<T, P, PF, K, KF>(
        &self,
        registry: &Registry,
        link: usize,
        provider: &str,
        platform: P,
        mut keyed: K,
    ) -> Result<(T, Served), ProviderError>
    where
        P: FnOnce() -> PF,
        PF: Future<Output = Result<T, ProviderError>>,
        K: FnMut(Arc<ProviderSet>) -> KF,
        KF: Future<Output = Result<T, ProviderError>>,
    {
        let Some(pool) = self.team_pool(registry, provider) else {
            if let Some(locked) = self.refusal(link) {
                return Err(ProviderError::TeamKeysExhausted { provider: locked });
            }
            return platform().await.map(|value| (value, Served::Platform));
        };
        // A pool with the fallback off locks the rest of the request before
        // any key is tried: every error from it (and a drop mid-rotation, on a
        // first-token timeout) leaves later pool-less links refusing.
        if !pool.fallback {
            *lock(&self.locked) = Some(provider.to_owned());
        }

        let count = pool.keys.len();
        let start = pool.cursor.load(Ordering::Relaxed);
        for step in 0..count {
            let index = (start + step) % count;
            let Some(key) = pool.keys.get(index) else {
                continue;
            };
            if key.benched(now_ms()) {
                continue;
            }
            let Some(set) = key.providers(registry, provider) else {
                continue;
            };
            match keyed(set).await {
                Ok(value) => {
                    key.set_health(Health::Ok);
                    pool.cursor.store((index + 1) % count, Ordering::Relaxed);
                    return Ok((
                        value,
                        Served::Team {
                            key_id: key.id.clone(),
                        },
                    ));
                }
                Err(ProviderError::RateLimited { retry_after, .. }) => {
                    let bench =
                        retry_after.map_or(MIN_RATE_LIMIT_BENCH, |r| r.max(MIN_RATE_LIMIT_BENCH));
                    key.set_health(Health::RateLimited {
                        until_ms: now_ms().saturating_add(duration_ms(bench)),
                    });
                    tracing::info!(provider, key_id = %key.id, "team key rate-limited; trying the next one");
                }
                Err(ProviderError::Upstream { status, .. }) if matches!(status, 401 | 403) => {
                    key.set_health(Health::Failing {
                        until_ms: now_ms().saturating_add(duration_ms(FAILING_BENCH)),
                        reason: format!("the provider rejected this key (HTTP {status})"),
                    });
                    tracing::warn!(provider, key_id = %key.id, status, "team key rejected by the provider; trying the next one");
                }
                Err(error) => return Err(error),
            }
        }

        if pool.fallback {
            return platform().await.map(|value| (value, Served::Platform));
        }
        Err(ProviderError::TeamKeysExhausted {
            provider: provider.to_owned(),
        })
    }
}

/// Lock, recovering from a poisoned mutex rather than panicking (CLAUDE.md
/// rule 1): the guarded data is plain state, harmless after a panic.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn duration_ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_core::Capability;
    use lumen_providers::{ModelSpec, ProviderSpec};
    use std::sync::atomic::AtomicUsize;

    fn registry(kind: ProviderKind) -> Registry {
        let spec = ProviderSpec {
            name: "p".to_owned(),
            kind,
            api_key: Some("platform".to_owned()),
            base_url: Some("http://127.0.0.1:9/v1".to_owned()),
            api_version: None,
            decisions_path: None,
            forward_unknown_fields: None,
            strict: false,
            connect_timeout_ms: None,
            models: vec![ModelSpec {
                id: "m".to_owned(),
                upstream_id: "m".to_owned(),
                capabilities: vec![Capability::Chat],
                modalities: vec!["text".to_owned()],
                release_date: None,
            }],
        };
        Registry::build(vec![spec], reqwest::Client::new(), Duration::from_secs(5))
            .expect("registry")
    }

    fn record(id: &str, group: &str) -> (GroupProviderKeyRecord, Zeroizing<String>) {
        (
            GroupProviderKeyRecord {
                id: id.to_owned(),
                group_id: group.to_owned(),
                provider: "p".to_owned(),
                label: "l".to_owned(),
                last4: "0000".to_owned(),
                created_at: 0,
            },
            Zeroizing::new(format!("secret-{id}")),
        )
    }

    fn keys_for(pools: &TeamPools, group: &str) -> RequestKeys {
        RequestKeys {
            group: Some(group.to_owned()),
            pools: pools.group(group),
            chain_lock: None,
            locked: Mutex::new(None),
        }
    }

    fn rate_limited() -> ProviderError {
        ProviderError::RateLimited {
            provider: "p".to_owned(),
            retry_after: None,
        }
    }

    #[tokio::test]
    async fn round_robins_on_success_and_benches_rate_limited_keys() {
        let reg = registry(ProviderKind::Openai);
        let pools = TeamPools::default();
        pools.replace(vec![record("k1", "g"), record("k2", "g")], &[]);
        let keys = keys_for(&pools, "g");
        let calls = AtomicUsize::new(0);

        // k1 is rate-limited: k2 serves, k1 is benched.
        let ((), served) = keys
            .attempt(
                &reg,
                0,
                "p",
                || async { Ok::<_, ProviderError>(()) },
                |_| {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if n == 0 {
                            Err(rate_limited())
                        } else {
                            Ok(())
                        }
                    }
                },
            )
            .await
            .expect("served");
        assert_eq!(
            served,
            Served::Team {
                key_id: "k2".into()
            }
        );
        assert_eq!(pools.status("g", "k1").0, "rate_limited");
        assert_eq!(pools.status("g", "k2").0, "ok");
        assert_eq!(pools.status("g", "nope").0, "unused");

        // Next request: the cursor moved past k2, k1 is benched, k2 serves.
        let ((), served) = keys
            .attempt(
                &reg,
                0,
                "p",
                || async { Ok::<_, ProviderError>(()) },
                |_| async { Ok(()) },
            )
            .await
            .expect("served");
        assert_eq!(
            served,
            Served::Team {
                key_id: "k2".into()
            }
        );
        assert!(keys.has_usable_pool("p"));
    }

    #[tokio::test]
    async fn exhausted_pool_locks_the_request_unless_fallback_is_on() {
        let reg = registry(ProviderKind::Openai);
        let pools = TeamPools::default();
        pools.replace(vec![record("k1", "g")], &[]);
        let keys = keys_for(&pools, "g");
        let platform_calls = AtomicUsize::new(0);
        let platform = || {
            platform_calls.fetch_add(1, Ordering::SeqCst);
            async { Ok::<_, ProviderError>(()) }
        };
        let err = keys
            .attempt(&reg, 0, "p", platform, |_| async { Err(rate_limited()) })
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::TeamKeysExhausted { ref provider } if provider == "p")
        );
        assert!(!keys.has_usable_pool("p"));
        // A provider without a pool is refused for the rest of this request.
        let err = keys
            .attempt(
                &reg,
                1,
                "other",
                || {
                    platform_calls.fetch_add(1, Ordering::SeqCst);
                    async { Ok::<_, ProviderError>(()) }
                },
                |_| async { Ok(()) },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::TeamKeysExhausted { ref provider } if provider == "p")
        );
        assert_eq!(platform_calls.load(Ordering::SeqCst), 0);

        // The same pool with the fallback on serves on the platform key.
        pools.replace(vec![record("k1", "g")], &[("g".into(), "p".into(), true)]);
        assert_eq!(
            pools.status("g", "k1").0,
            "rate_limited",
            "health survives a reload"
        );
        let keys = keys_for(&pools, "g");
        let ((), served) = keys
            .attempt(
                &reg,
                0,
                "p",
                || async { Ok::<_, ProviderError>(()) },
                |_| async { Ok(()) },
            )
            .await
            .expect("served");
        assert_eq!(served, Served::Platform);
    }

    #[tokio::test]
    async fn rejected_key_is_failing_and_other_errors_are_returned() {
        let reg = registry(ProviderKind::Openai);
        let pools = TeamPools::default();
        pools.replace(vec![record("k1", "g"), record("k2", "g")], &[]);
        let keys = keys_for(&pools, "g");
        let calls = AtomicUsize::new(0);
        let err = keys
            .attempt(
                &reg,
                0,
                "p",
                || async { Ok::<_, ProviderError>(()) },
                |_| {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        Err(ProviderError::Upstream {
                            provider: "p".into(),
                            status: if n == 0 { 401 } else { 500 },
                            retryable: n != 0,
                        })
                    }
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Upstream { status: 500, .. }));
        let (status, detail) = pools.status("g", "k1");
        assert_eq!(status, "failing");
        assert!(detail.is_some_and(|d| d.contains("401") && !d.contains("secret")));
        // The 500 is the second key's result, not a health change.
        assert_eq!(pools.status("g", "k2").0, "unused");
    }

    #[tokio::test]
    async fn no_group_or_ineligible_kind_uses_the_platform_key() {
        let pools = Arc::new(TeamPools::default());
        pools.replace(vec![record("k1", "g")], &[]);
        let no_group = RequestKeys::new(&pools, None);
        assert!(!no_group.has_usable_pool("p"));
        let other_group = keys_for(&pools, "h");
        assert!(!other_group.has_usable_pool("p"));

        let vllm = registry(ProviderKind::Vllm);
        let keys = keys_for(&pools, "g");
        let ((), served) = keys
            .attempt(
                &vllm,
                0,
                "p",
                || async { Ok::<_, ProviderError>(()) },
                |_| async { Err(rate_limited()) },
            )
            .await
            .expect("served");
        assert_eq!(served, Served::Platform);
    }

    #[tokio::test]
    async fn chain_order_decides_the_lock_up_front() {
        let reg = registry(ProviderKind::Openai);
        let pools = TeamPools::default();
        pools.replace(vec![record("k1", "g")], &[]);
        let platform_calls = AtomicUsize::new(0);
        let platform = || {
            platform_calls.fetch_add(1, Ordering::SeqCst);
            async { Ok::<_, ProviderError>(()) }
        };

        // Chain p -> other: link 1 refuses although link 0 never ran (an
        // open circuit skipped it, or a timeout dropped it).
        let keys = keys_for(&pools, "g").for_chain(&reg, ["p", "other"]);
        let err = keys
            .attempt(&reg, 1, "other", platform, |_| async { Ok(()) })
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::TeamKeysExhausted { ref provider } if provider == "p")
        );
        assert_eq!(platform_calls.load(Ordering::SeqCst), 0);

        // Chain other -> p: the platform-keyed link before the team link runs.
        let keys = keys_for(&pools, "g").for_chain(&reg, ["other", "p"]);
        let ((), served) = keys
            .attempt(
                &reg,
                0,
                "other",
                || async { Ok::<_, ProviderError>(()) },
                |_| async { Ok(()) },
            )
            .await
            .expect("served");
        assert_eq!(served, Served::Platform);

        // With the fallback on, nothing locks.
        pools.replace(vec![record("k1", "g")], &[("g".into(), "p".into(), true)]);
        let keys = keys_for(&pools, "g").for_chain(&reg, ["p", "other"]);
        let ((), served) = keys
            .attempt(
                &reg,
                1,
                "other",
                || async { Ok::<_, ProviderError>(()) },
                |_| async { Ok(()) },
            )
            .await
            .expect("served");
        assert_eq!(served, Served::Platform);
    }

    #[tokio::test]
    async fn any_team_key_error_without_fallback_locks_later_links() {
        let reg = registry(ProviderKind::Openai);
        let pools = TeamPools::default();
        pools.replace(vec![record("k1", "g")], &[]);
        // No chain given: the dynamic lock alone covers it.
        let keys = keys_for(&pools, "g");
        let err = keys
            .attempt(
                &reg,
                0,
                "p",
                || async { Ok::<_, ProviderError>(()) },
                |_| async {
                    Err::<(), _>(ProviderError::Upstream {
                        provider: "p".into(),
                        status: 503,
                        retryable: true,
                    })
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Upstream { status: 503, .. }));
        let err = keys
            .attempt(
                &reg,
                1,
                "other",
                || async { Ok::<_, ProviderError>(()) },
                |_| async { Ok(()) },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::TeamKeysExhausted { .. }));
    }

    #[tokio::test]
    async fn removed_key_is_gone_for_new_requests() {
        let reg = registry(ProviderKind::Openai);
        let pools = TeamPools::default();
        pools.replace(vec![record("k1", "g"), record("k2", "g")], &[]);
        let before = keys_for(&pools, "g");
        pools.remove("g", "p", "k1");
        for _ in 0..3 {
            let ((), served) = keys_for(&pools, "g")
                .attempt(
                    &reg,
                    0,
                    "p",
                    || async { Ok::<_, ProviderError>(()) },
                    |_| async { Ok(()) },
                )
                .await
                .expect("served");
            assert_eq!(
                served,
                Served::Team {
                    key_id: "k2".into()
                }
            );
        }
        // A snapshot taken before the delete still holds both keys.
        assert_eq!(before.pool("p").map(|p| p.keys.len()), Some(2));
        pools.remove("g", "p", "k2");
        assert!(!keys_for(&pools, "g").has_usable_pool("p"));
        pools.remove("nope", "p", "k2");
    }

    #[test]
    fn debug_never_shows_the_secret() {
        let key = PoolKey::new("k1".into(), Zeroizing::new("sk-very-secret".into()));
        let dbg = format!("{key:?}");
        assert!(
            !dbg.contains("sk-very-secret") && dbg.contains("<redacted>"),
            "{dbg}"
        );
    }

    #[test]
    fn only_links_with_a_usable_team_pool_get_a_breaker_scope() {
        let reg = registry(ProviderKind::Openai);
        let pools = Arc::new(TeamPools::default());
        pools.replace(vec![record("k1", "g")], &[]);
        let link = |provider: &str| Link {
            provider_name: provider.to_owned(),
            model_id: "m".to_owned(),
            escapes: Vec::new(),
            breaker_scope: None,
        };
        let mut links = vec![link("p"), link("other")];
        keys_for(&pools, "g").scope_links(&reg, &mut links);
        assert_eq!(links[0].breaker_scope.as_deref(), Some("g"));
        assert_eq!(links[1].breaker_scope, None);

        // No group, or another group: platform scope everywhere.
        let mut links = vec![link("p")];
        RequestKeys::new(&pools, None).scope_links(&reg, &mut links);
        assert_eq!(links[0].breaker_scope, None);
        keys_for(&pools, "h").scope_links(&reg, &mut links);
        assert_eq!(links[0].breaker_scope, None);

        // An ineligible kind: the pool is ignored, so is the scope.
        let mut links = vec![link("p")];
        keys_for(&pools, "g").scope_links(&registry(ProviderKind::Vllm), &mut links);
        assert_eq!(links[0].breaker_scope, None);

        // Every key benched: the link runs on the platform key, or refuses.
        if let Some(pool) = pools.group("g").and_then(|g| g.get("p").cloned()) {
            for key in &pool.keys {
                key.set_health(Health::RateLimited {
                    until_ms: now_ms() + 60_000,
                });
            }
        }
        let mut links = vec![link("p")];
        keys_for(&pools, "g").scope_links(&reg, &mut links);
        assert_eq!(links[0].breaker_scope, None);
    }
}
