//! Opt-in accurate per-model token counting (ADR 003).
//!
//! The default is the cheap byte heuristic in [`lumen_core::tokens`]: no
//! dependencies on the request path, allocation-light, hot-path-safe, and used
//! only as the local estimation fallback when an upstream reports no usage.
//!
//! When an operator sets `[tokenizer] mode = "accurate"`, this module refines
//! that fallback for OpenAI-family models with the exact BPE tokenizer
//! (`tiktoken-rs`, `cl100k_base` / `o200k_base` selected by the model id's
//! prefix). Per ADR 003's hot-path rule the refinement NEVER runs on the
//! request path and never delays the client's response:
//!
//! - The response envelope always carries the cheap heuristic estimate
//!   (flagged `estimated`), computed inline as before.
//! - The handler then hands the open [`Accounting`](crate::accounting::Accounting)
//!   record plus the already-extracted text to a spawned background task,
//!   which calls a `refine_*` method here and closes the record with the
//!   refined count - so `usage_log` and Prometheus carry the accurate number.
//! - Inside `refine_*`, the CPU-bound BPE pass runs on the blocking pool via
//!   [`tokio::task::spawn_blocking`], never on a tokio worker (repo rule 2).
//!
//! Every `refine_*` returns `Option`: `None` means "no refinement applies"
//! (heuristic mode, a model tiktoken does not describe, or a blocking-pool
//! failure) and the caller keeps the heuristic numbers it already computed -
//! counting can never fail or slow a request.
//!
//! Refined counts are still flagged `estimated = true`: a locally computed
//! count is an estimate against the upstream's authoritative usage, which
//! always wins when present (ADR 003). The encoders are built once at config
//! load (see [`TokenCounter::from_config`]) and shared behind an `Arc`, so no
//! encoder is ever constructed on the request path.

use std::sync::Arc;

use tiktoken_rs::CoreBPE;

use crate::config::{TokenizerConfig, TokenizerMode};

/// Per-message overhead in tokens, mirroring [`lumen_core::tokens`]' chat
/// heuristic (role, separators) so accurate and heuristic prompt counts stay
/// structurally comparable.
const PER_MESSAGE_OVERHEAD: u64 = 4;

/// A pre-initialized token counter, built once at config load and shared by
/// every handler (a cheap `Arc` clone in [`AppState`](crate::state::AppState)).
///
/// [`Heuristic`](Self::Heuristic) is a zero-cost marker: every `refine_*`
/// returns `None` immediately and nothing is ever spawned.
/// [`Accurate`](Self::Accurate) owns the BPE encoders so none is ever built on
/// the request path.
pub enum TokenCounter {
    /// The default byte heuristic. Refinement is a no-op.
    Heuristic,
    /// Accurate BPE refinement for OpenAI-family models; everything else keeps
    /// the heuristic.
    Accurate(AccurateEncoders),
}

/// The OpenAI BPE encoders, pre-built at startup. `cl100k_base` backs GPT-4 /
/// GPT-3.5 / `text-embedding-3`; `o200k_base` backs the GPT-4o, o-series and
/// GPT-4.1 / GPT-5 families.
pub struct AccurateEncoders {
    cl100k: Arc<CoreBPE>,
    o200k: Arc<CoreBPE>,
}

/// Which tiktoken vocabulary a model id maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Cl100k,
    O200k,
}

/// Map an OpenAI(-compatible) model id to its BPE vocabulary by prefix, or
/// `None` for a model tiktoken does not describe (the caller then keeps the
/// heuristic). Matching runs on the gateway-facing model id (the operator's
/// alias), so an alias that does not carry the upstream family prefix keeps
/// the heuristic. The GPT-4o family is checked before GPT-4 so `gpt-4o*`
/// never falls into the `cl100k` branch.
fn family_for_model(model: &str) -> Option<Family> {
    let m = model.to_ascii_lowercase();
    // o200k_base: GPT-4o, o1/o3/o4 reasoning, GPT-4.1 and GPT-5 families.
    if m.starts_with("gpt-4o")
        || m.starts_with("chatgpt-4o")
        || m.starts_with("gpt-4.1")
        || m.starts_with("gpt-5")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
    {
        return Some(Family::O200k);
    }
    // cl100k_base: GPT-4, GPT-3.5-turbo, text-embedding-3-*, ada-002.
    if m.starts_with("gpt-4")
        || m.starts_with("gpt-3.5")
        || m.starts_with("text-embedding-3")
        || m.starts_with("text-embedding-ada")
    {
        return Some(Family::Cl100k);
    }
    None
}

impl AccurateEncoders {
    /// Build both encoders. Returns the tiktoken error message on failure so
    /// the caller can fall back to the heuristic with a warning (this never
    /// happens in practice: the vocabularies are embedded, not fetched).
    fn load() -> Result<Self, String> {
        let cl100k = tiktoken_rs::cl100k_base().map_err(|e| e.to_string())?;
        let o200k = tiktoken_rs::o200k_base().map_err(|e| e.to_string())?;
        Ok(Self {
            cl100k: Arc::new(cl100k),
            o200k: Arc::new(o200k),
        })
    }

    /// The encoder for `model`, or `None` when no OpenAI-family prefix matches.
    fn encoder_for(&self, model: &str) -> Option<Arc<CoreBPE>> {
        match family_for_model(model)? {
            Family::Cl100k => Some(Arc::clone(&self.cl100k)),
            Family::O200k => Some(Arc::clone(&self.o200k)),
        }
    }
}

/// Convert a `usize` token count to `u64`, saturating (a count never overflows
/// in practice; this only avoids a lossy `as` cast under clippy pedantic).
fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

impl TokenCounter {
    /// Build the counter from config. In `accurate` mode the encoders are
    /// constructed here (at config load, off the request path); if that fails,
    /// the counter degrades to the heuristic with a warning rather than
    /// aborting boot.
    #[must_use]
    pub fn from_config(config: &TokenizerConfig) -> Self {
        match config.mode {
            TokenizerMode::Heuristic => Self::Heuristic,
            TokenizerMode::Accurate => match AccurateEncoders::load() {
                Ok(encoders) => Self::Accurate(encoders),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "accurate tokenizer unavailable; using the byte heuristic"
                    );
                    Self::Heuristic
                }
            },
        }
    }

    /// Whether accurate counting is active (for the boot log / tests).
    #[must_use]
    pub fn is_accurate(&self) -> bool {
        matches!(self, Self::Accurate(_))
    }

    /// Whether this counter can refine `model`'s heuristic count with an exact
    /// BPE pass (accurate mode AND a known OpenAI-family prefix). Handlers use
    /// this to decide whether deferring accounting to a background refinement
    /// task is worth anything; when `false`, they settle inline with the
    /// heuristic exactly as in heuristic mode - zero added work.
    #[must_use]
    pub fn refines(&self, model: &str) -> bool {
        match self {
            Self::Heuristic => false,
            Self::Accurate(_) => family_for_model(model).is_some(),
        }
    }

    /// The encoder for `model` when refinement applies.
    fn encoder_for(&self, model: &str) -> Option<Arc<CoreBPE>> {
        match self {
            Self::Heuristic => None,
            Self::Accurate(encoders) => encoders.encoder_for(model),
        }
    }

    /// Refine a chat estimate: exact `(input, output)` token counts for the
    /// already-extracted message texts (plus the same per-message overhead as
    /// the heuristic) and the concatenated completion text. `None` = keep the
    /// heuristic. Runs the BPE pass on the blocking pool.
    pub async fn refine_chat(
        &self,
        model: &str,
        message_texts: Vec<String>,
        output_text: String,
    ) -> Option<(u64, u64)> {
        let bpe = self.encoder_for(model)?;
        let overhead = to_u64(message_texts.len()).saturating_mul(PER_MESSAGE_OVERHEAD);
        let counts = tokio::task::spawn_blocking(move || {
            let input: usize = message_texts
                .iter()
                .map(|t| bpe.encode_ordinary(t).len())
                .sum();
            let output = bpe.encode_ordinary(&output_text).len();
            (input, output)
        })
        .await
        .ok()?;
        Some((to_u64(counts.0).saturating_add(overhead), to_u64(counts.1)))
    }

    /// Refine an embeddings estimate: exact input token count summed over the
    /// batch. `None` = keep the heuristic. Runs on the blocking pool.
    pub async fn refine_embed(&self, model: &str, texts: Vec<String>) -> Option<u64> {
        let bpe = self.encoder_for(model)?;
        let sum = tokio::task::spawn_blocking(move || {
            texts
                .iter()
                .map(|t| bpe.encode_ordinary(t).len())
                .sum::<usize>()
        })
        .await
        .ok()?;
        Some(to_u64(sum))
    }

    /// Refine a rerank estimate: the query is compared against every document,
    /// so it counts once per document (matching the heuristic's semantics).
    /// `None` = keep the heuristic. Runs on the blocking pool.
    pub async fn refine_rerank(
        &self,
        model: &str,
        query: String,
        documents: Vec<String>,
    ) -> Option<u64> {
        let bpe = self.encoder_for(model)?;
        let doc_count = to_u64(documents.len());
        let (query_tokens, doc_tokens) = tokio::task::spawn_blocking(move || {
            let query_tokens = bpe.encode_ordinary(&query).len();
            let doc_tokens: usize = documents.iter().map(|d| bpe.encode_ordinary(d).len()).sum();
            (query_tokens, doc_tokens)
        })
        .await
        .ok()?;
        Some(
            doc_count
                .saturating_mul(to_u64(query_tokens))
                .saturating_add(to_u64(doc_tokens)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accurate() -> TokenCounter {
        TokenCounter::from_config(&TokenizerConfig {
            mode: TokenizerMode::Accurate,
        })
    }

    #[test]
    fn model_prefix_selects_the_right_vocabulary() {
        assert_eq!(family_for_model("gpt-4o-mini"), Some(Family::O200k));
        assert_eq!(family_for_model("gpt-4o"), Some(Family::O200k));
        assert_eq!(family_for_model("o1-preview"), Some(Family::O200k));
        assert_eq!(family_for_model("gpt-4.1"), Some(Family::O200k));
        // gpt-4o must not be captured by the gpt-4 (cl100k) branch.
        assert_eq!(family_for_model("gpt-4"), Some(Family::Cl100k));
        assert_eq!(family_for_model("gpt-4-turbo"), Some(Family::Cl100k));
        assert_eq!(family_for_model("gpt-3.5-turbo"), Some(Family::Cl100k));
        assert_eq!(
            family_for_model("text-embedding-3-small"),
            Some(Family::Cl100k)
        );
        // Non-OpenAI models map to no encoder (heuristic fallback).
        assert_eq!(family_for_model("claude-3-5-sonnet"), None);
        assert_eq!(family_for_model("mistral-large"), None);
    }

    #[test]
    fn refines_is_true_only_for_accurate_mode_and_known_families() {
        let counter = accurate();
        assert!(counter.is_accurate());
        assert!(counter.refines("gpt-4"));
        assert!(counter.refines("gpt-4o-mini"));
        assert!(!counter.refines("claude-3-5-sonnet"));
        assert!(!counter.refines("rerank-v3.5"));

        let heuristic = TokenCounter::from_config(&TokenizerConfig::default());
        assert!(!heuristic.is_accurate());
        // Heuristic mode never refines, even for OpenAI-family models.
        assert!(!heuristic.refines("gpt-4"));
    }

    #[tokio::test]
    async fn refine_counts_a_known_string_exactly_for_cl100k() {
        // "hello world" is [15339, 1917] in cl100k_base: exactly 2 tokens,
        // plus the 4-token per-message overhead for the one message.
        let counter = accurate();
        let refined = counter
            .refine_chat("gpt-4", vec!["hello world".to_owned()], String::new())
            .await;
        assert_eq!(refined, Some((2 + PER_MESSAGE_OVERHEAD, 0)));
    }

    #[tokio::test]
    async fn refine_counts_a_known_string_exactly_for_o200k() {
        // The o200k_base vocabulary also encodes "hello world" as 2 tokens.
        let counter = accurate();
        let refined = counter
            .refine_chat(
                "gpt-4o",
                vec!["hello world".to_owned()],
                "hello world".to_owned(),
            )
            .await;
        assert_eq!(refined, Some((2 + PER_MESSAGE_OVERHEAD, 2)));
    }

    #[tokio::test]
    async fn heuristic_mode_never_refines_the_fallback_path() {
        // In heuristic mode every refine_* is None: the caller keeps the
        // heuristic numbers and nothing touches the blocking pool.
        let counter = TokenCounter::from_config(&TokenizerConfig::default());
        assert_eq!(
            counter
                .refine_chat("gpt-4", vec!["hello world".to_owned()], String::new())
                .await,
            None
        );
        assert_eq!(
            counter
                .refine_embed("text-embedding-3-small", vec!["hello world".to_owned()])
                .await,
            None
        );
        assert_eq!(
            counter
                .refine_rerank("gpt-4o", "q".to_owned(), vec!["d".to_owned()])
                .await,
            None
        );
    }

    #[tokio::test]
    async fn unknown_models_never_refine_in_accurate_mode() {
        // A non-OpenAI model has no tiktoken vocabulary: refinement declines
        // (None) and the caller keeps the heuristic - never an error.
        let counter = accurate();
        assert_eq!(
            counter
                .refine_chat("llama-3-70b", vec!["some prompt".to_owned()], String::new())
                .await,
            None
        );
        assert_eq!(
            counter
                .refine_embed("mistral-embed", vec!["text".to_owned()])
                .await,
            None
        );
    }

    #[tokio::test]
    async fn refine_embed_and_rerank_count_exactly() {
        let counter = accurate();
        assert_eq!(
            counter
                .refine_embed(
                    "text-embedding-3-small",
                    vec!["hello world".to_owned(), "hello world".to_owned()],
                )
                .await,
            Some(4)
        );
        // query (2 tokens) once per document (1) + document tokens (2) == 4.
        assert_eq!(
            counter
                .refine_rerank(
                    "gpt-4o",
                    "hello world".to_owned(),
                    vec!["hello world".to_owned()],
                )
                .await,
            Some(4)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refinement_runs_off_the_runtime_worker() {
        // On a single-threaded runtime, a blocking BPE pass inline would starve
        // the only worker. That this resolves proves the work is dispatched to
        // the blocking pool via spawn_blocking, not run on the worker thread.
        let counter = accurate();
        let refined = counter
            .refine_chat("gpt-4", vec!["hello world".to_owned()], String::new())
            .await;
        assert_eq!(refined, Some((2 + PER_MESSAGE_OVERHEAD, 0)));
    }
}
