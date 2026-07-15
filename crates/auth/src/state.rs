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
use crate::store::VirtualKeyRecord;
use dashmap::DashMap;
use lumen_core::{GatewayError, QuotaKind};
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

/// The live, request-path view of one virtual key.
#[derive(Debug)]
pub struct KeyEntry {
    id: String,
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
    /// reserve the estimated cost against the hard budget. When a later
    /// admission step refuses (TPM after RPM, or the budget after both), the
    /// earlier bumps are rolled back so a rejected request consumes no quota
    /// and never reserves budget.
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
        let max = self.budget_max_micro.load(Ordering::SeqCst);
        if max == UNLIMITED {
            self.spent_micro.fetch_add(reserve, Ordering::SeqCst);
        } else {
            // CAS loop: check-and-reserve is one atomic step, so two requests
            // can never both fit into the same remaining budget.
            let mut current = self.spent_micro.load(Ordering::SeqCst);
            loop {
                if current.saturating_add(reserve) > max {
                    // Refused after the quota bumps: unwind them so the budget
                    // rejection does not also consume the caller's quota.
                    if rpm_tracked {
                        adjust_window(&self.rpm_window, minute, -1);
                    }
                    if tpm_tracked {
                        adjust_window(&self.tpm_window, minute, -tpm_debit);
                    }
                    return Err(GatewayError::BudgetExceeded);
                }
                match self.spent_micro.compare_exchange_weak(
                    current,
                    current + reserve,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(actual) => current = actual,
                }
            }
        }
        self.dirty.store(true, Ordering::SeqCst);

        Ok(Reservation {
            entry: Arc::clone(self),
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
    /// Commit the real cost and token count of the call. The budget releases
    /// any over-reservation (or charges the shortfall), and the TPM window is
    /// adjusted from the pre-call estimate to the real token count. In both
    /// dimensions the real figure wins even past the limit - the *next*
    /// request is the one that gets refused.
    pub fn settle(mut self, actual_cost_micro: i64, actual_tokens: i64) {
        let delta = actual_cost_micro.max(0) - self.reserved_micro;
        self.entry.spent_micro.fetch_add(delta, Ordering::SeqCst);
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
            // Refund the budget only: no money was spent. The TPM debit is
            // kept on purpose (see the struct doc) - a request that hit the
            // gateway counts against the rate limit even when it failed.
            self.entry
                .spent_micro
                .fetch_sub(self.reserved_micro, Ordering::SeqCst);
            self.entry.dirty.store(true, Ordering::SeqCst);
        }
    }
}

/// The full in-memory key table: hash → entry for authentication, id → the
/// same entries for admin updates and flushing.
#[derive(Debug, Default)]
pub struct AuthState {
    by_hash: DashMap<String, Arc<KeyEntry>>,
    by_id: DashMap<String, Arc<KeyEntry>>,
}

impl AuthState {
    /// Build the state from `(key hash, record)` pairs loaded at boot.
    #[must_use]
    pub fn load(entries: Vec<(String, VirtualKeyRecord)>) -> Self {
        let state = Self::default();
        for (hash, record) in entries {
            state.upsert(hash, &record);
        }
        state
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
    /// same id from installing divergent entries in the two maps.
    pub fn upsert(&self, hash: String, record: &VirtualKeyRecord) {
        match self.by_id.entry(record.id.clone()) {
            dashmap::Entry::Occupied(existing) => existing.get().apply_limits(record),
            dashmap::Entry::Vacant(slot) => {
                let entry = Arc::new(KeyEntry::from_record(record));
                self.by_hash.insert(hash, Arc::clone(&entry));
                slot.insert(entry);
            }
        }
    }

    /// Apply an admin update to an existing key (by id). Spend is preserved;
    /// limits, expiry and the disabled flag take effect immediately.
    pub fn apply(&self, record: &VirtualKeyRecord) {
        if let Some(entry) = self.by_id.get(&record.id) {
            entry.apply_limits(record);
        }
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
