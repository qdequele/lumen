//! The SQLite store: virtual keys, usage log, encrypted provider keys.
//!
//! The store is **off the request path** by design (M5 §5.2): the server
//! reads it at boot, writes budget flushes and usage batches from background
//! tasks, and serves the admin API from it. Request-time enforcement happens
//! against in-memory state only.

use crate::crypto::MasterKey;
use crate::key::{generate, hash_key, random_id, PlaintextKey};
use crate::{now_unix, AuthError};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::str::FromStr;
use std::time::Duration;

/// One virtual-key row, as loaded from the database. Never carries the
/// plaintext key — only its hash identifies it, and even that stays private
/// to the store.
#[derive(Debug, Clone, PartialEq, serde::Serialize, sqlx::FromRow)]
pub struct VirtualKeyRecord {
    /// Opaque identifier (primary key, safe to expose in the admin API).
    pub id: String,
    /// Human-readable label.
    pub name: String,
    /// Hard budget in USD; `None` = unlimited.
    pub budget_max: Option<f64>,
    /// Spend accumulated so far (flushed periodically from memory).
    pub budget_spent: f64,
    /// Requests-per-minute cap; `None` = unlimited.
    pub rpm_limit: Option<i64>,
    /// Tokens-per-minute cap; `None` = unlimited.
    pub tpm_limit: Option<i64>,
    /// Expiry as unix seconds; `None` = never.
    pub expires_at: Option<i64>,
    /// Disabled keys authenticate as invalid.
    pub disabled: bool,
    /// Creation time, unix seconds.
    pub created_at: i64,
}

/// Parameters for creating a key.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct NewKey {
    /// Human-readable label.
    pub name: String,
    /// Hard budget in USD; `None` = unlimited.
    pub budget_max: Option<f64>,
    /// Requests-per-minute cap.
    pub rpm_limit: Option<i64>,
    /// Tokens-per-minute cap.
    pub tpm_limit: Option<i64>,
    /// Expiry as unix seconds.
    pub expires_at: Option<i64>,
}

/// A partial update: `None` fields are left unchanged (fields cannot be
/// cleared back to NULL through a patch — create a new key instead).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct KeyPatch {
    /// New label.
    pub name: Option<String>,
    /// New hard budget in USD.
    pub budget_max: Option<f64>,
    /// New RPM cap.
    pub rpm_limit: Option<i64>,
    /// New TPM cap.
    pub tpm_limit: Option<i64>,
    /// New expiry (unix seconds).
    pub expires_at: Option<i64>,
    /// Enable/disable the key.
    pub disabled: Option<bool>,
}

/// One usage-log entry (M5 §5.3 / ADR 003). No prompt or response content —
/// counts, cost and labels only.
#[derive(Debug, Clone)]
pub struct UsageRecord {
    /// The virtual key that made the call; `None` when auth is disabled.
    pub key_id: Option<String>,
    /// Client-facing model id the client requested.
    pub model: String,
    /// Model that actually served the request — the same as `model` unless a
    /// fallback fired (M6 §6.2).
    pub model_used: String,
    /// `chat` | `embed` | `rerank`.
    pub capability: String,
    /// Input/prompt tokens.
    pub tokens_in: i64,
    /// Output/completion tokens (0 for embed/rerank).
    pub tokens_out: i64,
    /// Rerank search units, when the provider bills in them.
    pub search_units: Option<i64>,
    /// Number of media items (images, …) in the request (M9). 0 for text-only.
    pub media_count: i64,
    /// Total decoded media bytes in the request (M9). 0 for text-only.
    pub media_bytes: i64,
    /// Whether the token counts were locally estimated (ADR 003).
    pub estimated: bool,
    /// Cost in USD derived from the configured price table.
    pub cost: f64,
    /// End-to-end latency of the call in milliseconds.
    pub latency_ms: i64,
    /// HTTP status returned to the client.
    pub status: u16,
    /// ADR 002 metadata as a compact JSON object, when supplied.
    pub metadata: Option<String>,
    /// Unix seconds.
    pub ts: i64,
}

/// Handle to the SQLite database (pooled; cheap to clone).
#[derive(Debug, Clone)]
pub struct KeyStore {
    pool: SqlitePool,
}

impl KeyStore {
    /// Open (creating if missing) the database at `url` — e.g.
    /// `sqlite:///var/lib/lumen/lumen.db` — and apply embedded
    /// migrations.
    pub async fn connect(url: &str) -> Result<Self, AuthError> {
        let options = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        Self::migrate(pool).await
    }

    /// Open a fresh in-memory database (tests and ephemeral runs).
    ///
    /// A single never-recycled connection: each new SQLite `:memory:`
    /// connection is a *different* empty database, so pooling more than one —
    /// or letting the pool close an idle one — would silently lose all data.
    pub async fn in_memory() -> Result<Self, AuthError> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await?;
        Self::migrate(pool).await
    }

    async fn migrate(pool: SqlitePool) -> Result<Self, AuthError> {
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    // ---- Virtual keys -------------------------------------------------------

    /// Create a virtual key. The returned [`PlaintextKey`] is the only copy of
    /// the clear key that will ever exist — the database gets its hash.
    pub async fn create_key(
        &self,
        params: NewKey,
    ) -> Result<(PlaintextKey, VirtualKeyRecord), AuthError> {
        let plaintext = generate();
        let record = VirtualKeyRecord {
            id: random_id(),
            name: params.name,
            budget_max: params.budget_max,
            budget_spent: 0.0,
            rpm_limit: params.rpm_limit,
            tpm_limit: params.tpm_limit,
            expires_at: params.expires_at,
            disabled: false,
            created_at: now_unix(),
        };
        sqlx::query(
            "INSERT INTO virtual_keys \
             (id, key_hash, name, budget_max, budget_spent, rpm_limit, tpm_limit, expires_at, disabled, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&record.id)
        .bind(hash_key(plaintext.reveal()))
        .bind(&record.name)
        .bind(record.budget_max)
        .bind(record.budget_spent)
        .bind(record.rpm_limit)
        .bind(record.tpm_limit)
        .bind(record.expires_at)
        .bind(record.disabled)
        .bind(record.created_at)
        .execute(&self.pool)
        .await?;
        Ok((plaintext, record))
    }

    /// Look a key up by the BLAKE3 hash of its plaintext.
    pub async fn find_by_hash(&self, hash: &str) -> Result<Option<VirtualKeyRecord>, AuthError> {
        let record = sqlx::query_as::<_, VirtualKeyRecord>(
            "SELECT id, name, budget_max, budget_spent, rpm_limit, tpm_limit, expires_at, disabled, created_at \
             FROM virtual_keys WHERE key_hash = ?",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(record)
    }

    /// Every key **with its hash** — exclusively for building the in-memory
    /// [`AuthState`](crate::state::AuthState) at boot. The hash never leaves
    /// the auth layer.
    pub async fn load_auth_entries(&self) -> Result<Vec<(String, VirtualKeyRecord)>, AuthError> {
        let rows = sqlx::query(
            "SELECT id, key_hash, name, budget_max, budget_spent, rpm_limit, tpm_limit, expires_at, disabled, created_at \
             FROM virtual_keys",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let hash: String = row.try_get("key_hash")?;
            let record = VirtualKeyRecord {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                budget_max: row.try_get("budget_max")?,
                budget_spent: row.try_get("budget_spent")?,
                rpm_limit: row.try_get("rpm_limit")?,
                tpm_limit: row.try_get("tpm_limit")?,
                expires_at: row.try_get("expires_at")?,
                disabled: row.try_get("disabled")?,
                created_at: row.try_get("created_at")?,
            };
            entries.push((hash, record));
        }
        Ok(entries)
    }

    /// Every key, hash included nowhere — for boot loading and the admin API.
    pub async fn list_keys(&self) -> Result<Vec<VirtualKeyRecord>, AuthError> {
        let records = sqlx::query_as::<_, VirtualKeyRecord>(
            "SELECT id, name, budget_max, budget_spent, rpm_limit, tpm_limit, expires_at, disabled, created_at \
             FROM virtual_keys ORDER BY created_at, id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(records)
    }

    /// Apply a partial update; returns the updated record, or `None` when the
    /// id does not exist.
    pub async fn update_key(
        &self,
        id: &str,
        patch: KeyPatch,
    ) -> Result<Option<VirtualKeyRecord>, AuthError> {
        let changed = sqlx::query(
            "UPDATE virtual_keys SET \
               name = COALESCE(?, name), \
               budget_max = COALESCE(?, budget_max), \
               rpm_limit = COALESCE(?, rpm_limit), \
               tpm_limit = COALESCE(?, tpm_limit), \
               expires_at = COALESCE(?, expires_at), \
               disabled = COALESCE(?, disabled) \
             WHERE id = ?",
        )
        .bind(patch.name)
        .bind(patch.budget_max)
        .bind(patch.rpm_limit)
        .bind(patch.tpm_limit)
        .bind(patch.expires_at)
        .bind(patch.disabled)
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if changed == 0 {
            return Ok(None);
        }
        let record = sqlx::query_as::<_, VirtualKeyRecord>(
            "SELECT id, name, budget_max, budget_spent, rpm_limit, tpm_limit, expires_at, disabled, created_at \
             FROM virtual_keys WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(record)
    }

    /// Persist absolute spent amounts `(key id, spent USD)` — the periodic
    /// flush of in-memory counters. One transaction for the whole batch.
    pub async fn persist_budgets(&self, spent: &[(String, f64)]) -> Result<(), AuthError> {
        let mut tx = self.pool.begin().await?;
        for (id, amount) in spent {
            sqlx::query("UPDATE virtual_keys SET budget_spent = ? WHERE id = ?")
                .bind(amount)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    // ---- Usage log ----------------------------------------------------------

    /// Insert a batch of usage records in one transaction (the async writer's
    /// flush path).
    pub async fn insert_usage(&self, batch: &[UsageRecord]) -> Result<(), AuthError> {
        let mut tx = self.pool.begin().await?;
        for rec in batch {
            sqlx::query(
                "INSERT INTO usage_log \
                 (key_id, model, model_used, capability, tokens_in, tokens_out, search_units, media_count, media_bytes, estimated, cost, latency_ms, status, metadata, ts) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&rec.key_id)
            .bind(&rec.model)
            .bind(&rec.model_used)
            .bind(&rec.capability)
            .bind(rec.tokens_in)
            .bind(rec.tokens_out)
            .bind(rec.search_units)
            .bind(rec.media_count)
            .bind(rec.media_bytes)
            .bind(rec.estimated)
            .bind(rec.cost)
            .bind(rec.latency_ms)
            .bind(i64::from(rec.status))
            .bind(&rec.metadata)
            .bind(rec.ts)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Delete usage entries with `ts` strictly older than `cutoff` (retention
    /// purge). Returns the number of rows removed.
    pub async fn purge_usage_older_than(&self, cutoff: i64) -> Result<u64, AuthError> {
        let deleted = sqlx::query("DELETE FROM usage_log WHERE ts < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(deleted)
    }

    /// Number of usage rows (tests and diagnostics).
    pub async fn count_usage(&self) -> Result<i64, AuthError> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM usage_log")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get("n")?)
    }

    // ---- Provider keys (encrypted at rest) ----------------------------------

    /// Store (or replace) a provider key, sealed with the master key.
    pub async fn store_provider_key(
        &self,
        name: &str,
        plaintext: &str,
        master: &MasterKey,
    ) -> Result<(), AuthError> {
        let sealed = master.seal(plaintext.as_bytes())?;
        sqlx::query(
            "INSERT INTO provider_keys (name, ciphertext, created_at) VALUES (?, ?, ?) \
             ON CONFLICT (name) DO UPDATE SET ciphertext = excluded.ciphertext",
        )
        .bind(name)
        .bind(sealed)
        .bind(now_unix())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load and decrypt a provider key. `Ok(None)` when absent; an error when
    /// present but undecryptable (wrong master key / corruption) — that must
    /// fail loudly, not silently behave like a missing key.
    pub async fn load_provider_key(
        &self,
        name: &str,
        master: &MasterKey,
    ) -> Result<Option<String>, AuthError> {
        let row = sqlx::query("SELECT ciphertext FROM provider_keys WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else { return Ok(None) };
        let sealed: Vec<u8> = row.try_get("ciphertext")?;
        let plaintext = master.open(&sealed)?;
        String::from_utf8(plaintext)
            .map(Some)
            .map_err(|_| AuthError::Decrypt)
    }

    // ---- Diagnostics --------------------------------------------------------

    /// Render every stored row as text — a **test/diagnostic** helper backing
    /// the "no plaintext secret at rest" assertions. Never call this from a
    /// request path.
    pub async fn debug_dump(&self) -> Result<String, AuthError> {
        let mut dump = String::new();
        for table in ["virtual_keys", "usage_log"] {
            // `quote()` renders any SQLite value as a literal; the column list
            // is fixed per table so this stays injection-free.
            let sql = match table {
                "virtual_keys" => {
                    "SELECT quote(id)||'|'||quote(key_hash)||'|'||quote(name)||'|'||quote(budget_max)||'|'||quote(budget_spent)||'|'||quote(rpm_limit)||'|'||quote(tpm_limit)||'|'||quote(expires_at)||'|'||quote(disabled)||'|'||quote(created_at) AS r FROM virtual_keys"
                }
                _ => {
                    "SELECT quote(id)||'|'||quote(key_id)||'|'||quote(model)||'|'||quote(model_used)||'|'||quote(capability)||'|'||quote(tokens_in)||'|'||quote(tokens_out)||'|'||quote(search_units)||'|'||quote(estimated)||'|'||quote(cost)||'|'||quote(latency_ms)||'|'||quote(status)||'|'||coalesce(metadata,'')||'|'||quote(ts) AS r FROM usage_log"
                }
            };
            for row in sqlx::query(sql).fetch_all(&self.pool).await? {
                let r: String = row.try_get("r")?;
                dump.push_str(&r);
                dump.push('\n');
            }
        }
        // Provider-key blobs: decode the raw stored bytes as (lossy) text so a
        // plaintext accidentally stored unencrypted WOULD be caught by greps.
        for row in sqlx::query("SELECT name, ciphertext FROM provider_keys")
            .fetch_all(&self.pool)
            .await?
        {
            let name: String = row.try_get("name")?;
            let blob: Vec<u8> = row.try_get("ciphertext")?;
            dump.push_str(&name);
            dump.push('|');
            dump.push_str(&String::from_utf8_lossy(&blob));
            dump.push('\n');
        }
        Ok(dump)
    }
}
