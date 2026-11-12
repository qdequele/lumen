//! Account virtual models (ADR 022): each Lab account's own virtual models,
//! stored as one JSON document per account in the auth database, compiled
//! into an in-memory table per account over the platform's routing table,
//! and resolved only for that account's keys.
//!
//! The request path never reads the database: the compiled tables live in
//! the [`ResilienceRuntime`](crate::resilience::ResilienceRuntime). They are
//! refreshed by the admin write that changed them, after every platform
//! reload, and by [`spawn_refresher`], which polls the table's generation so
//! every process sharing the database converges.

use std::sync::Arc;
use std::time::Duration;

use lumen_auth::store::{AccountModelsRow, KeyStore};
use lumen_router::virtual_models::{RoutingTable, VirtualModelConfig};

use crate::resilience::ResilienceRuntime;

/// Most virtual models one account may hold.
pub const MAX_ACCOUNT_VIRTUAL_MODELS: usize = 32;

/// Largest account document, in bytes of its JSON text.
pub const MAX_ACCOUNT_DOCUMENT_BYTES: usize = 256 * 1024;

/// How often [`spawn_refresher`] polls for writes made by other processes.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// The `virtual_model` label of `lumen_virtual_model_requests_total` for an
/// account model, and the `model` label of `lumen_request_duration_seconds`
/// for a request to one that failed before any target served it:
/// team-defined ids never become label values.
pub const METRIC_LABEL: &str = "account";

/// The document text of `configs`: a JSON array, in the account's order.
///
/// # Errors
/// A value that does not serialise (not reachable for parsed configs).
pub fn document_text(configs: &[VirtualModelConfig]) -> Result<String, serde_json::Error> {
    serde_json::to_string(configs)
}

/// The `If-Match` version of a document text.
#[must_use]
pub fn document_hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

/// The version of an account that never wrote a document.
#[must_use]
pub fn empty_hash() -> String {
    document_hash("[]")
}

/// One account's virtual models: the stored configs and their compiled table.
pub struct AccountModels {
    /// The stored configs, in the account's order.
    pub configs: Vec<VirtualModelConfig>,
    /// The document's `If-Match` version.
    pub hash: String,
    /// The store generation this was read at (0 when never stored).
    pub generation: i64,
    /// The configs compiled over the platform snapshot current at install.
    pub table: RoutingTable,
}

// Manual: the configs carry preset and remap text.
impl std::fmt::Debug for AccountModels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountModels")
            .field("models", &self.configs.len())
            .field("hash", &self.hash)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// A stored row, parsed: `(account, configs, hash, generation)`.
pub type ParsedRow = (String, Vec<VirtualModelConfig>, String, i64);

/// Parse a stored row. A row that does not parse (an out-of-band edit) is
/// logged without its content and skipped, so the account keeps what it had.
#[must_use]
pub fn parse_row(row: AccountModelsRow) -> Option<ParsedRow> {
    match serde_json::from_str::<Vec<VirtualModelConfig>>(&row.document) {
        Ok(configs) => Some((row.account_ref, configs, row.hash, row.generation)),
        Err(error) => {
            tracing::warn!(
                account = %row.account_ref,
                generation = row.generation,
                line = error.line(),
                column = error.column(),
                "stored account virtual models do not parse; keeping the current ones"
            );
            None
        }
    }
}

/// Load every row written after the runtime's last seen generation and
/// install it. A store error keeps the current tables.
pub async fn refresh(store: &KeyStore, resilience: &ResilienceRuntime) {
    let since = resilience.account_generation();
    match store.account_virtual_models_since(since).await {
        Ok(rows) => {
            // Every fetched row moves the watermark, one that does not parse
            // included, so a corrupt newest row is not re-read every tick.
            let latest = rows.iter().map(|r| r.generation).max();
            let parsed: Vec<ParsedRow> = rows.into_iter().filter_map(parse_row).collect();
            if !parsed.is_empty() {
                resilience.install_accounts(parsed, true);
            }
            if let Some(latest) = latest {
                resilience.advance_account_generation(latest);
            }
        }
        Err(error) => tracing::warn!(
            %error,
            "account virtual models refresh failed; keeping the current tables"
        ),
    }
}

/// Poll the store every [`REFRESH_INTERVAL`] and load what other processes
/// wrote. One indexed `MAX(generation)` read per tick; rows are read only
/// when it moved. Runs off the request path until the process exits.
#[must_use]
pub fn spawn_refresher(
    store: KeyStore,
    resilience: Arc<ResilienceRuntime>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(REFRESH_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match store.account_virtual_models_generation().await {
                Ok(latest) if latest > resilience.account_generation() => {
                    refresh(&store, &resilience).await;
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    %error,
                    "account virtual models poll failed; keeping the current tables"
                ),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_empty_hash_is_the_hash_of_an_empty_document() {
        assert_eq!(empty_hash(), document_hash(&document_text(&[]).unwrap()));
    }

    #[test]
    fn a_corrupt_row_is_skipped() {
        let row = AccountModelsRow {
            account_ref: "a".to_owned(),
            document: "{not json".to_owned(),
            hash: "h".to_owned(),
            generation: 1,
        };
        assert!(parse_row(row).is_none());
    }
}
