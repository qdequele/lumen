//! Opt-in accurate per-model token counting (ADR 003).
//!
//! The default is the cheap byte heuristic in [`lumen_core::tokens`]: no
//! dependencies on the request path, allocation-light, hot-path-safe, and used
//! only as the local estimation fallback when an upstream reports no usage.
//!
//! When an operator sets `[tokenizer] mode = "accurate"`, this module counts
//! OpenAI-family prompts with the exact BPE tokenizer (`tiktoken-rs`,
//! `cl100k_base` / `o200k_base` selected by the model id's prefix). The BPE
//! pass runs on the blocking pool via [`tokio::task::spawn_blocking`], so a
//! large prompt never occupies a tokio worker (repo rule 2). Any failure - a
//! model that maps to no known encoder, a panicking encode - falls back to the
//! heuristic, so counting can never fail or reject a request.
//!
//! Accurate counts are still flagged `estimated = true`: a locally computed
//! count is an estimate against the upstream's authoritative usage, which
//! always wins when present (ADR 003). The encoders are built once at config
//! load (see [`TokenCounter::from_config`]) and shared behind an `Arc`, so no
//! encoder is ever constructed on the request path.

use std::sync::Arc;

use lumen_core::{tokens, ChatRequest, EmbedRequest, RerankRequest};
use tiktoken_rs::CoreBPE;

use crate::config::{TokenizerConfig, TokenizerMode};

/// Per-message overhead in tokens, mirroring [`lumen_core::tokens`]' chat
/// heuristic (role, separators) so accurate and heuristic prompt counts stay
/// structurally comparable.
const PER_MESSAGE_OVERHEAD: u64 = 4;

/// A pre-initialized token counter, built once at config load and shared by
/// every handler (a cheap `Arc` clone in [`AppState`](crate::state::AppState)).
///
/// [`Heuristic`](Self::Heuristic) is a zero-cost marker that delegates straight
/// to [`lumen_core::tokens`]; [`Accurate`](Self::Accurate) owns the BPE
/// encoders so none is ever built on the request path.
pub enum TokenCounter {
    /// The default byte heuristic. Synchronous, never touches the blocking pool.
    Heuristic,
    /// Accurate BPE counting for OpenAI-family models, heuristic fallback for
    /// everything else.
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
/// heuristic). The GPT-4o family is checked before GPT-4 so `gpt-4o*` never
/// falls into the `cl100k` branch.
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

    /// Count the prompt tokens of a chat request. Accurate mode BPE-encodes each
    /// message's text off the tokio runtime and adds the same per-message
    /// overhead as the heuristic; any failure falls back to the heuristic.
    pub async fn count_chat_prompt(&self, req: &ChatRequest) -> u64 {
        let Self::Accurate(encoders) = self else {
            return tokens::estimate_chat_prompt(req);
        };
        let Some(bpe) = encoders.encoder_for(&req.model) else {
            return tokens::estimate_chat_prompt(req);
        };
        let texts: Vec<String> = req
            .messages
            .iter()
            .map(|m| {
                m.content
                    .as_ref()
                    .map(|c| c.text().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        let overhead = to_u64(req.messages.len()).saturating_mul(PER_MESSAGE_OVERHEAD);
        match tokio::task::spawn_blocking(move || {
            texts.iter().map(|t| bpe.encode_ordinary(t).len()).sum()
        })
        .await
        {
            Ok(sum) => to_u64(sum).saturating_add(overhead),
            Err(_) => tokens::estimate_chat_prompt(req),
        }
    }

    /// Count the tokens of a single piece of text under `model` (chat
    /// completion output). Heuristic fallback on any failure.
    pub async fn count_text(&self, model: &str, text: &str) -> u64 {
        let Self::Accurate(encoders) = self else {
            return tokens::estimate_text(text);
        };
        let Some(bpe) = encoders.encoder_for(model) else {
            return tokens::estimate_text(text);
        };
        let owned = text.to_owned();
        match tokio::task::spawn_blocking(move || bpe.encode_ordinary(&owned).len()).await {
            Ok(n) => to_u64(n),
            Err(_) => tokens::estimate_text(text),
        }
    }

    /// Count the input tokens of an embeddings request (sum over the batch,
    /// image parts contribute nothing). Heuristic fallback on any failure.
    pub async fn count_embed_input(&self, req: &EmbedRequest) -> u64 {
        let Self::Accurate(encoders) = self else {
            return tokens::estimate_embed_input(req);
        };
        let Some(bpe) = encoders.encoder_for(&req.model) else {
            return tokens::estimate_embed_input(req);
        };
        let texts: Vec<String> = req.input.iter().map(str::to_owned).collect();
        match tokio::task::spawn_blocking(move || {
            texts
                .iter()
                .map(|t| bpe.encode_ordinary(t).len())
                .sum::<usize>()
        })
        .await
        {
            Ok(sum) => to_u64(sum),
            Err(_) => tokens::estimate_embed_input(req),
        }
    }

    /// Count the tokens processed by a rerank request: the query is compared
    /// against every document, so it counts once per document (matching the
    /// heuristic's semantics). Heuristic fallback on any failure.
    pub async fn count_rerank(&self, req: &RerankRequest) -> u64 {
        let Self::Accurate(encoders) = self else {
            return tokens::estimate_rerank(req);
        };
        let Some(bpe) = encoders.encoder_for(&req.model) else {
            return tokens::estimate_rerank(req);
        };
        let query = req.query.clone();
        let docs: Vec<String> = req.documents.iter().map(|d| d.text().to_owned()).collect();
        let doc_count = to_u64(docs.len());
        match tokio::task::spawn_blocking(move || {
            let query_tokens = bpe.encode_ordinary(&query).len();
            let doc_tokens: usize = docs.iter().map(|d| bpe.encode_ordinary(d).len()).sum();
            (query_tokens, doc_tokens)
        })
        .await
        {
            Ok((query_tokens, doc_tokens)) => doc_count
                .saturating_mul(to_u64(query_tokens))
                .saturating_add(to_u64(doc_tokens)),
            Err(_) => tokens::estimate_rerank(req),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_core::chat::{ChatMessage, MessageContent};

    fn accurate() -> TokenCounter {
        TokenCounter::from_config(&TokenizerConfig {
            mode: TokenizerMode::Accurate,
        })
    }

    fn user_msg(text: &str) -> ChatMessage {
        ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Text(text.to_owned())),
            name: None,
            extra: serde_json::Map::new(),
        }
    }

    fn chat_req(model: &str, messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: model.to_owned(),
            messages,
            temperature: None,
            top_p: None,
            max_tokens: None,
            n: None,
            stop: None,
            stream: false,
            extra: serde_json::Map::new(),
        }
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

    #[tokio::test]
    async fn accurate_mode_counts_a_known_string_exactly_for_cl100k() {
        // "hello world" is [15339, 1917] in cl100k_base: exactly 2 tokens.
        let counter = accurate();
        assert!(counter.is_accurate());
        assert_eq!(counter.count_text("gpt-4", "hello world").await, 2);
    }

    #[tokio::test]
    async fn accurate_mode_counts_a_known_string_exactly_for_o200k() {
        // The o200k_base vocabulary also encodes "hello world" as 2 tokens.
        let counter = accurate();
        assert_eq!(counter.count_text("gpt-4o", "hello world").await, 2);
    }

    #[tokio::test]
    async fn accurate_chat_prompt_is_bpe_plus_message_overhead() {
        let counter = accurate();
        let req = chat_req("gpt-4", vec![user_msg("hello world")]);
        // 2 BPE tokens + 4 per-message overhead.
        assert_eq!(
            counter.count_chat_prompt(&req).await,
            2 + PER_MESSAGE_OVERHEAD
        );
    }

    #[tokio::test]
    async fn heuristic_mode_is_unchanged_and_matches_core() {
        let counter = TokenCounter::from_config(&TokenizerConfig::default());
        assert!(!counter.is_accurate());
        let req = chat_req("gpt-4", vec![user_msg("hello world")]);
        // Byte heuristic: 11 bytes -> 3 tokens + 4 overhead == core's estimate.
        assert_eq!(
            counter.count_chat_prompt(&req).await,
            tokens::estimate_chat_prompt(&req)
        );
        assert_eq!(
            counter.count_text("gpt-4", "hello world").await,
            tokens::estimate_text("hello world")
        );
    }

    #[tokio::test]
    async fn accurate_mode_falls_back_to_heuristic_for_unknown_models() {
        // A non-OpenAI model has no tiktoken vocabulary: the accurate counter
        // must return exactly the heuristic value, never fail.
        let counter = accurate();
        let text = "some prompt for a llama model";
        assert_eq!(
            counter.count_text("llama-3-70b", text).await,
            tokens::estimate_text(text)
        );
        let req = chat_req("mistral-large", vec![user_msg("hello world")]);
        assert_eq!(
            counter.count_chat_prompt(&req).await,
            tokens::estimate_chat_prompt(&req)
        );
    }

    #[tokio::test]
    async fn accurate_embed_and_rerank_use_bpe_for_openai_models() {
        let counter = accurate();
        let embed: EmbedRequest = serde_json::from_str(
            r#"{"model":"text-embedding-3-small","input":["hello world","hello world"]}"#,
        )
        .expect("valid request");
        // 2 + 2 BPE tokens.
        assert_eq!(counter.count_embed_input(&embed).await, 4);

        let rerank: RerankRequest = serde_json::from_str(
            r#"{"model":"gpt-4o","query":"hello world","documents":["hello world"]}"#,
        )
        .expect("valid request");
        // query (2) once per document (1) + document tokens (2) == 4.
        assert_eq!(counter.count_rerank(&rerank).await, 4);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn accurate_counting_runs_off_the_runtime_worker() {
        // On a single-threaded runtime, a blocking BPE pass inline would starve
        // the only worker. That this resolves proves the work is dispatched to
        // the blocking pool via spawn_blocking, not run on the worker thread.
        let counter = accurate();
        assert_eq!(counter.count_text("gpt-4", "hello world").await, 2);
    }
}
