//! In-memory key state and request-path admission control (M5 §5.2).
//!
//! Loaded once at boot from the [store](crate::store), then consulted on
//! every request without ever touching the database:
//!
//! * **authentication** - a BLAKE3 hash lookup in a [`DashMap`];
//! * **hard budget** - a compare-and-swap *reservation* of the estimated cost
//!   before the upstream call, adjusted to the real cost afterwards, so
//!   concurrent requests can never overrun the budget between check and debit;
//! * **RPM/TPM quotas** - per-minute windows packed into single atomics
//!   (window minute in the high 32 bits, count in the low 32), bumped with a
//!   CAS loop. The TPM window debits the pre-call estimate and is then settled
//!   to the real token count afterwards (mirroring the budget); a bump made by
//!   a request that a later admission step refuses is rolled back, so a
//!   rejected request consumes no quota.
//!
//! Money is tracked in integer **micro-USD** so the atomics stay exact; the
//! DB speaks USD floats at the edges ([`usd_to_micro`] / [`micro_to_usd`]).

use crate::key::hash_key;
use crate::store::{GroupRecord, VirtualKeyRecord};
use arc_swap::ArcSwapOption;
use dashmap::DashMap;
use lumen_core::{BudgetScope, GatewayError, QuotaKind};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Sentinel meaning "no limit" for the atomic limit fields.
const UNLIMITED: i64 = i64::MAX;

/// Convert USD to integer micro-USD (rounding to the nearest micro).
#[must_use]
#[allow(clippy::cast_possible_truncation)] // clamped: budgets are far below i64::MAX micros
pub fn usd_to_micro(usd: f64) -> i64 {
    let micros = (usd * 1_000_000.0).round();
    if micros >= 9.2e18 {
        i64::MAX
    } else if micros <= -9.2e18 {
        i64::MIN
    } else {
        micros as i64
    }
}

/// Convert integer micro-USD back to USD.
#[must_use]
#[allow(clippy::cast_precision_loss)] // f64 mantissa covers realistic budgets exactly
pub fn micro_to_usd(micro: i64) -> f64 {
    micro as f64 / 1_000_000.0
}

/// Atomically reserve `reserve` micro-USD against `spent`, bounded by
/// `max_bound` ([`UNLIMITED`] = no bound). Check-and-reserve is one atomic
/// step (a CAS loop), so two requests can never both fit into the same
/// remaining budget. Returns `false`, without reserving, when the addition
/// would exceed the bound. Shared by the key budget and the group pool
/// (ADR 009).
fn reserve_micro(spent: &AtomicI64, max_bound: i64, reserve: i64) -> bool {
    if max_bound == UNLIMITED {
        spent.fetch_add(reserve, Ordering::SeqCst);
        return true;
    }
    let mut current = spent.load(Ordering::SeqCst);
    loop {
        if current.saturating_add(reserve) > max_bound {
            return false;
        }
        match spent.compare_exchange_weak(
            current,
            current + reserve,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

/// The live, request-path view of one budget group (ADR 009): the budget
/// half of a [`KeyEntry`], shared by every member key through an `Arc`.
#[derive(Debug)]
pub struct GroupEntry {
    id: String,
    /// Shared hard budget in micro-USD; [`UNLIMITED`] = none.
    budget_max_micro: AtomicI64,
    /// Committed + currently-reserved pool spend in micro-USD.
    spent_micro: AtomicI64,
    /// Pool spend changed since the last flush.
    dirty: AtomicBool,
}

impl GroupEntry {
    fn from_record(record: &GroupRecord) -> Self {
        Self {
            id: record.id.clone(),
            budget_max_micro: AtomicI64::new(record.budget_max.map_or(UNLIMITED, usd_to_micro)),
            spent_micro: AtomicI64::new(usd_to_micro(record.budget_spent)),
            dirty: AtomicBool::new(false),
        }
    }

    /// Overwrite the adjustable fields from an (admin-updated) record. The
    /// accrued pool spend is deliberately NOT overwritten - memory is the
    /// source of truth for spend after boot, exactly like keys.
    fn apply_limits(&self, record: &GroupRecord) {
        self.budget_max_micro.store(
            record.budget_max.map_or(UNLIMITED, usd_to_micro),
            Ordering::SeqCst,
        );
    }

    /// The group's opaque id (the `budget_groups.id` column).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Current pool spend (committed + reserved) in micro-USD.
    #[must_use]
    pub fn spent_micro(&self) -> i64 {
        self.spent_micro.load(Ordering::SeqCst)
    }
}

/// The live, request-path view of one virtual key.
#[derive(Debug)]
pub struct KeyEntry {
    id: String,
    /// The budget group this key draws from (ADR 009); lock-free load on
    /// the hot path, swapped by admin membership changes. The `Arc` is
    /// shared with [`AuthState::groups`], so a group budget patch is
    /// visible to every member instantly.
    group: ArcSwapOption<GroupEntry>,
    /// Hard budget in micro-USD; [`UNLIMITED`] = none.
    budget_max_micro: AtomicI64,
    /// Committed + currently-reserved spend in micro-USD.
    spent_micro: AtomicI64,
    /// Requests per minute; [`UNLIMITED`] = none.
    rpm_limit: AtomicI64,
    /// Tokens per minute; [`UNLIMITED`] = none.
    tpm_limit: AtomicI64,
    /// Unix-seconds expiry; [`UNLIMITED`] = never.
    expires_at: AtomicI64,
    disabled: AtomicBool,
    /// Packed quota windows: minute in the high 32 bits, count in the low 32.
    rpm_window: AtomicU64,
    tpm_window: AtomicU64,
    /// Spend changed since the last flush.
    dirty: AtomicBool,
}

impl KeyEntry {
    fn from_record(record: &VirtualKeyRecord) -> Self {
        let entry = Self {
            id: record.id.clone(),
            group: ArcSwapOption::empty(),
            budget_max_micro: AtomicI64::new(UNLIMITED),
            spent_micro: AtomicI64::new(usd_to_micro(record.budget_spent)),
            rpm_limit: AtomicI64::new(UNLIMITED),
            tpm_limit: AtomicI64::new(UNLIMITED),
            expires_at: AtomicI64::new(UNLIMITED),
            disabled: AtomicBool::new(record.disabled),
            rpm_window: AtomicU64::new(0),
            tpm_window: AtomicU64::new(0),
            dirty: AtomicBool::new(false),
        };
        entry.apply_limits(record);
        entry
    }

    /// Swap the live group this key draws from (admin membership change /
    /// boot resolve). The pointer is resolved by [`AuthState`], never here:
    /// the entry knows only the group it enforces against.
    fn set_group(&self, group: Option<Arc<GroupEntry>>) {
        self.group.store(group);
    }

    /// The id of the budget group this key currently draws from, if any -
    /// a snapshot for usage attribution at accounting begin.
    #[must_use]
    pub fn group_id(&self) -> Option<String> {
        self.group.load().as_ref().map(|g| g.id.clone())
    }

    /// Overwrite the adjustable fields from an (admin-updated) record. The
    /// accrued spend is deliberately NOT overwritten - memory is the source
    /// of truth for spend after boot.
    fn apply_limits(&self, record: &VirtualKeyRecord) {
        self.budget_max_micro.store(
            record.budget_max.map_or(UNLIMITED, usd_to_micro),
            Ordering::SeqCst,
        );
        self.rpm_limit
            .store(record.rpm_limit.unwrap_or(UNLIMITED), Ordering::SeqCst);
        self.tpm_limit
            .store(record.tpm_limit.unwrap_or(UNLIMITED), Ordering::SeqCst);
        self.expires_at
            .store(record.expires_at.unwrap_or(UNLIMITED), Ordering::SeqCst);
        self.disabled.store(record.disabled, Ordering::SeqCst);
    }

    /// The key's opaque id (the `virtual_keys.id` column).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Current spend (committed + reserved) in micro-USD.
    #[must_use]
    pub fn spent_micro(&self) -> i64 {
        self.spent_micro.load(Ordering::SeqCst)
    }

    fn usable(&self, now: i64) -> bool {
        !self.disabled.load(Ordering::SeqCst) && now < self.expires_at.load(Ordering::SeqCst)
    }

    /// Admit one request: bump the RPM and TPM windows, then atomically
    /// reserve the estimated cost against the key's hard budget and, when
    /// the key belongs to a budget group (ADR 009), against the group's
    /// shared pool. When a later admission step refuses (TPM after RPM, the
    /// key budget after both, or the group pool after all three), the
    /// earlier bumps and reservations are rolled back so a rejected request
    /// consumes no quota and never reserves budget.
    ///
    /// The TPM window is debited with the pre-call **estimate**;
    /// [`Reservation::settle`] later adjusts it to the real token count, and
    /// dropping the reservation unsettled refunds the estimate (the call never
    /// happened).
    ///
    /// # Errors
    ///
    /// [`GatewayError::QuotaExceeded`] (429) or
    /// [`GatewayError::BudgetExceeded`] (402) - all decided in memory,
    /// *before* any upstream call.
    pub fn admit(
        self: &Arc<Self>,
        now: i64,
        estimated_tokens: i64,
        estimated_cost_micro: i64,
    ) -> Result<Reservation, GatewayError> {
        let minute = now.div_euclid(60);
        let retry_after = || Some(Duration::from_secs(window_remaining_secs(now)));

        let rpm = self.rpm_limit.load(Ordering::SeqCst);
        let rpm_tracked = rpm != UNLIMITED;
        if rpm_tracked && !bump_window(&self.rpm_window, minute, rpm, 1) {
            return Err(GatewayError::QuotaExceeded {
                quota: QuotaKind::Rpm,
                retry_after: retry_after(),
            });
        }

        let tpm = self.tpm_limit.load(Ordering::SeqCst);
        let tpm_tracked = tpm != UNLIMITED;
        let tpm_debit = estimated_tokens.max(0);
        if tpm_tracked && !bump_window(&self.tpm_window, minute, tpm, tpm_debit) {
            // A rejected request must not burn a request slot: roll back the
            // RPM bump we just made.
            if rpm_tracked {
                adjust_window(&self.rpm_window, minute, -1);
            }
            return Err(GatewayError::QuotaExceeded {
                quota: QuotaKind::Tpm,
                retry_after: retry_after(),
            });
        }

        let reserve = estimated_cost_micro.max(0);
        let unwind_windows = |entry: &Self| {
            // Refused after the quota bumps: unwind them so a budget
            // rejection does not also consume the caller's quota (ADR 007).
            if rpm_tracked {
                adjust_window(&entry.rpm_window, minute, -1);
            }
            if tpm_tracked {
                adjust_window(&entry.tpm_window, minute, -tpm_debit);
            }
        };
        let key_max = self.budget_max_micro.load(Ordering::SeqCst);
        if !reserve_micro(&self.spent_micro, key_max, reserve) {
            unwind_windows(self);
            return Err(GatewayError::BudgetExceeded {
                scope: BudgetScope::Key,
            });
        }

        // Group pool last (ADR 009): a refusal here unwinds everything the
        // earlier steps took, key-budget reservation included. Between that
        // reservation and this unwind a concurrent request may observe the
        // key's spend transiently inflated and get refused - conservative,
        // never an overrun.
        let group = self.group.load_full();
        if let Some(group) = &group {
            let group_max = group.budget_max_micro.load(Ordering::SeqCst);
            if !reserve_micro(&group.spent_micro, group_max, reserve) {
                self.spent_micro.fetch_sub(reserve, Ordering::SeqCst);
                // The refund is a spend change too: a flush may have drained
                // the transiently inflated value between the reserve and this
                // unwind, so leave the entry dirty for the next flush to
                // correct (mirrors `Reservation::drop`).
                self.dirty.store(true, Ordering::SeqCst);
                unwind_windows(self);
                return Err(GatewayError::BudgetExceeded {
                    scope: BudgetScope::Group,
                });
            }
            group.dirty.store(true, Ordering::SeqCst);
        }
        self.dirty.store(true, Ordering::SeqCst);

        Ok(Reservation {
            entry: Arc::clone(self),
            group,
            reserved_micro: reserve,
            tpm_debit: tpm_tracked.then_some(tpm_debit),
            minute,
            settled: false,
        })
    }
}

/// A budget (and TPM) reservation held for the duration of one upstream call.
///
/// [`settle`](Self::settle) replaces the reserved estimate with the real cost
/// and real token count. Dropping without settling refunds the **budget**
/// reservation (the call failed or was cancelled - no money was spent) but
/// deliberately keeps the **TPM** debit: the tokens-per-minute window is a
/// rate limiter, and a request that hit the gateway counts even when the
/// upstream call then failed. Only a successful [`settle`](Self::settle),
/// which knows the real token count, adjusts the TPM window.
#[derive(Debug)]
pub struct Reservation {
    entry: Arc<KeyEntry>,
    /// The group pool the reservation was ALSO made against, captured at
    /// admission (ADR 009): a key moved to another group mid-flight still
    /// settles against the pool that admitted it.
    group: Option<Arc<GroupEntry>>,
    reserved_micro: i64,
    /// Tokens debited to the TPM window at admit, to settle/refund against the
    /// real usage; `None` when TPM is untracked (no debit was made).
    tpm_debit: Option<i64>,
    /// The minute the debits were made in: a settle/refund only touches the
    /// TPM window while it still belongs to this minute, otherwise the slot
    /// has already rolled over and there is nothing to adjust.
    minute: i64,
    settled: bool,
}

impl Reservation {
    /// The id of the group pool this reservation was charged against, if
    /// any - the authoritative attribution for the request's usage row: a
    /// concurrent admin membership swap cannot make the row disagree with
    /// the pool that actually admitted it (ADR 009).
    #[must_use]
    pub fn group_id(&self) -> Option<String> {
        self.group.as_ref().map(|g| g.id.clone())
    }

    /// Commit the real cost and token count of the call. The budget releases
    /// any over-reservation (or charges the shortfall), and the TPM window is
    /// adjusted from the pre-call estimate to the real token count. In both
    /// dimensions the real figure wins even past the limit - the *next*
    /// request is the one that gets refused.
    pub fn settle(mut self, actual_cost_micro: i64, actual_tokens: i64) {
        let delta = actual_cost_micro.max(0) - self.reserved_micro;
        self.entry.spent_micro.fetch_add(delta, Ordering::SeqCst);
        if let Some(group) = &self.group {
            // The pool sees the same real cost as the key (ADR 009).
            group.spent_micro.fetch_add(delta, Ordering::SeqCst);
            group.dirty.store(true, Ordering::SeqCst);
        }
        if let Some(debited) = self.tpm_debit {
            let token_delta = actual_tokens.max(0) - debited;
            adjust_window(&self.entry.tpm_window, self.minute, token_delta);
        }
        self.entry.dirty.store(true, Ordering::SeqCst);
        self.settled = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.settled {
            // Refund the budgets only: no money was spent. The TPM debit is
            // kept on purpose (see the struct doc) - a request that hit the
            // gateway counts against the rate limit even when it failed.
            self.entry
                .spent_micro
                .fetch_sub(self.reserved_micro, Ordering::SeqCst);
            self.entry.dirty.store(true, Ordering::SeqCst);
            if let Some(group) = &self.group {
                group
                    .spent_micro
                    .fetch_sub(self.reserved_micro, Ordering::SeqCst);
                group.dirty.store(true, Ordering::SeqCst);
            }
        }
    }
}

/// The full in-memory key table: hash → entry for authentication, id → the
/// same entries for admin updates and flushing, plus the budget groups the
/// keys draw from (ADR 009).
#[derive(Debug, Default)]
pub struct AuthState {
    by_hash: DashMap<String, Arc<KeyEntry>>,
    by_id: DashMap<String, Arc<KeyEntry>>,
    groups: DashMap<String, Arc<GroupEntry>>,
}

impl AuthState {
    /// Build the state from the group records and `(key hash, record)` pairs
    /// loaded at boot. Groups load first: keys resolve their group pointer
    /// against them.
    #[must_use]
    pub fn load(groups: Vec<GroupRecord>, entries: Vec<(String, VirtualKeyRecord)>) -> Self {
        let state = Self::default();
        for record in groups {
            state.upsert_group(&record);
        }
        for (hash, record) in entries {
            state.upsert(hash, &record);
        }
        state
    }

    /// Resolve a record's `group_id` to the live group entry. A dangling id
    /// (only reachable through out-of-band DB edits - the store validates
    /// membership writes) leaves the key without live pool enforcement,
    /// with a warning, rather than refusing all its traffic.
    fn resolve_group(&self, record: &VirtualKeyRecord) -> Option<Arc<GroupEntry>> {
        let group_id = record.group_id.as_deref()?;
        let resolved = self.groups.get(group_id).map(|g| Arc::clone(&g));
        if resolved.is_none() {
            tracing::warn!(
                key_id = %record.id,
                group_id = %group_id,
                "key references an unknown budget group; pool enforcement disabled for it"
            );
        }
        resolved
    }

    /// Number of keys loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// True when no keys are loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Resolve a presented bearer key to its live entry. `None` for unknown,
    /// disabled or expired keys - indistinguishable to the caller by design.
    #[must_use]
    pub fn authenticate(&self, presented: &str, now: i64) -> Option<Arc<KeyEntry>> {
        let entry = self.by_hash.get(&hash_key(presented))?;
        entry.usable(now).then(|| Arc::clone(&entry))
    }

    /// Insert a brand-new key (admin create / boot load). Holding the
    /// `by_id` entry across the decision keeps two concurrent upserts of the
    /// same id from installing divergent entries in the two maps. Either
    /// way the key's group pointer is re-resolved from the record.
    pub fn upsert(&self, hash: String, record: &VirtualKeyRecord) {
        let group = self.resolve_group(record);
        match self.by_id.entry(record.id.clone()) {
            dashmap::Entry::Occupied(existing) => {
                existing.get().apply_limits(record);
                existing.get().set_group(group);
            }
            dashmap::Entry::Vacant(slot) => {
                let entry = Arc::new(KeyEntry::from_record(record));
                entry.set_group(group);
                self.by_hash.insert(hash, Arc::clone(&entry));
                slot.insert(entry);
            }
        }
    }

    /// Apply an admin update to an existing key (by id). Spend is preserved;
    /// limits, expiry, the disabled flag and group membership take effect
    /// immediately.
    pub fn apply(&self, record: &VirtualKeyRecord) {
        if let Some(entry) = self.by_id.get(&record.id) {
            entry.apply_limits(record);
            entry.set_group(self.resolve_group(record));
        }
    }

    /// Remove a key from the live table (admin delete). The key stops
    /// authenticating on the very next request - no restart. The `by_hash`
    /// side is found by entry identity, so the hash itself never has to
    /// travel back out of the store.
    ///
    /// Returns the evicted entry so the caller can flush its final accrued
    /// spend before it is dropped (the periodic flusher will never see this
    /// id again once it is gone from `by_id`); `None` when the id was not
    /// live (already removed, or never loaded).
    pub fn remove(&self, id: &str) -> Option<Arc<KeyEntry>> {
        let (_, entry) = self.by_id.remove(id)?;
        self.by_hash.retain(|_, e| !Arc::ptr_eq(e, &entry));
        Some(entry)
    }

    /// Swap the hash an existing key authenticates under (admin rotate).
    /// The live entry itself is KEPT - and with it the accrued spend and the
    /// current quota windows - so rotation never resets budget state. The
    /// old plaintext stops working immediately; the new one works without a
    /// restart. An id missing from the live table is inserted fresh from the
    /// record (defensive - create and boot always populate it).
    pub fn rotate(&self, new_hash: String, record: &VirtualKeyRecord) {
        let Some(entry) = self.by_id.get(&record.id).map(|e| Arc::clone(&e)) else {
            self.upsert(new_hash, record);
            return;
        };
        // Rotation is a rare admin action: a linear sweep of `by_hash` to
        // evict the old alias is fine, and keeps hashes out of `KeyEntry`.
        self.by_hash.retain(|_, e| !Arc::ptr_eq(e, &entry));
        entry.apply_limits(record);
        entry.set_group(self.resolve_group(record));
        self.by_hash.insert(new_hash, entry);
    }

    /// Insert or refresh a budget group (admin create / boot load / reload).
    /// An existing entry only has its limits re-applied and keeps its
    /// in-memory pool spend, mirroring key upserts.
    pub fn upsert_group(&self, record: &GroupRecord) {
        match self.groups.entry(record.id.clone()) {
            dashmap::Entry::Occupied(existing) => existing.get().apply_limits(record),
            dashmap::Entry::Vacant(slot) => {
                slot.insert(Arc::new(GroupEntry::from_record(record)));
            }
        }
    }

    /// Apply an admin update to an existing group (by id). Pool spend is
    /// preserved; the budget takes effect for every member on their very
    /// next request (the members share the entry through an `Arc`).
    pub fn apply_group(&self, record: &GroupRecord) {
        if let Some(entry) = self.groups.get(&record.id) {
            entry.apply_limits(record);
        }
    }

    /// Remove a group from the live table (admin delete). The store refuses
    /// deletion while active member keys exist, so by the time this runs no
    /// live key should still point at the entry; any straggler holding the
    /// `Arc` keeps enforcing against the detached pool until its own next
    /// membership update - never a panic, never unlimited spend.
    ///
    /// Returns the evicted entry so the caller can flush its final accrued
    /// pool spend (the periodic flusher will never see this id again);
    /// `None` when the id was not live.
    pub fn remove_group(&self, id: &str) -> Option<Arc<GroupEntry>> {
        self.groups.remove(id).map(|(_, entry)| entry)
    }

    /// Collect `(key id, spent USD)` for every key whose spend changed since
    /// the last call, marking them clean. Any spend that lands between the
    /// clean-marking and the read re-dirties the entry, so nothing is lost.
    #[must_use]
    pub fn drain_dirty(&self) -> Vec<(String, f64)> {
        let mut out = Vec::new();
        for entry in &self.by_id {
            if entry.dirty.swap(false, Ordering::SeqCst) {
                out.push((
                    entry.id.clone(),
                    micro_to_usd(entry.spent_micro.load(Ordering::SeqCst)),
                ));
            }
        }
        out
    }

    /// Collect `(group id, pool spend USD)` for every group whose spend
    /// changed since the last call, marking them clean - the group half of
    /// [`drain_dirty`](Self::drain_dirty), with the same re-dirtying
    /// guarantee for spend that lands mid-drain.
    #[must_use]
    pub fn drain_dirty_groups(&self) -> Vec<(String, f64)> {
        let mut out = Vec::new();
        for entry in &self.groups {
            if entry.dirty.swap(false, Ordering::SeqCst) {
                out.push((
                    entry.id.clone(),
                    micro_to_usd(entry.spent_micro.load(Ordering::SeqCst)),
                ));
            }
        }
        out
    }
}

/// Seconds until the current per-minute quota window rolls over.
fn window_remaining_secs(now: i64) -> u64 {
    let into_window = now.rem_euclid(60);
    // 1..=60: even at the window boundary we advertise a positive wait.
    u64::try_from(60 - into_window).unwrap_or(60)
}

/// Bump a packed `minute << 32 | count` window by `add`, respecting `limit`.
/// Returns `false` (without bumping) when the addition would exceed the
/// limit. A stale window from a previous minute counts as empty.
fn bump_window(window: &AtomicU64, minute: i64, limit: i64, add: i64) -> bool {
    // The minute fits u32 until year ~10430; clamp defensively regardless.
    let minute_tag = u64::try_from(minute).unwrap_or(0) & 0xFFFF_FFFF;
    let mut current = window.load(Ordering::SeqCst);
    loop {
        let count = if current >> 32 == minute_tag {
            i64::from(u32::try_from(current & 0xFFFF_FFFF).unwrap_or(u32::MAX))
        } else {
            0
        };
        if count.saturating_add(add) > limit {
            return false;
        }
        let new_count = u64::try_from(count + add)
            .unwrap_or(u64::from(u32::MAX))
            .min(u64::from(u32::MAX));
        let next = (minute_tag << 32) | new_count;
        match window.compare_exchange_weak(current, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

/// Adjust a packed `minute << 32 | count` window's count by `delta` (which may
/// be negative), saturating into `[0, u32::MAX]`. A no-op once the window has
/// rolled past `minute` - that slot's tokens have already expired, so there is
/// nothing left to unwind or settle. Used to roll back a bump when a later
/// admission step rejects, to refund an unsettled reservation, and to settle
/// the TPM estimate to the real token count.
fn adjust_window(window: &AtomicU64, minute: i64, delta: i64) {
    if delta == 0 {
        return;
    }
    let minute_tag = u64::try_from(minute).unwrap_or(0) & 0xFFFF_FFFF;
    let mut current = window.load(Ordering::SeqCst);
    loop {
        if current >> 32 != minute_tag {
            // The window belongs to another minute now: the debit we would
            // adjust is already gone.
            return;
        }
        let count = i64::from(u32::try_from(current & 0xFFFF_FFFF).unwrap_or(u32::MAX));
        let new_count = count.saturating_add(delta).clamp(0, i64::from(u32::MAX));
        let next = (minute_tag << 32) | u64::try_from(new_count).unwrap_or(0);
        match window.compare_exchange_weak(current, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usd_micro_conversions_roundtrip() {
        assert_eq!(usd_to_micro(1.0), 1_000_000);
        assert_eq!(usd_to_micro(0.000_001), 1);
        assert!((micro_to_usd(usd_to_micro(12.345_678)) - 12.345_678).abs() < 1e-9);
        // Extreme values clamp instead of overflowing.
        assert_eq!(usd_to_micro(f64::MAX), i64::MAX);
        assert_eq!(usd_to_micro(f64::MIN), i64::MIN);
    }

    #[test]
    fn packed_window_resets_between_minutes() {
        let w = AtomicU64::new(0);
        assert!(bump_window(&w, 100, 2, 1));
        assert!(bump_window(&w, 100, 2, 1));
        assert!(!bump_window(&w, 100, 2, 1));
        // Next minute: fresh count.
        assert!(bump_window(&w, 101, 2, 1));
        // …and an old-minute bump also starts fresh rather than underflowing.
        assert!(bump_window(&w, 99, 2, 1));
    }

    #[test]
    fn window_remaining_is_always_1_to_60() {
        assert_eq!(window_remaining_secs(0), 60);
        assert_eq!(window_remaining_secs(59), 1);
        assert_eq!(window_remaining_secs(60), 60);
        // Pre-epoch clocks still produce a sane hint.
        assert_eq!(window_remaining_secs(-1), 1);
    }
}
