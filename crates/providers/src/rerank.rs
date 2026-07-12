//! Gateway-side finalisation for rerank requests.
//!
//! Providers only have to return scored results (an `index` into the request's
//! documents plus a `relevance_score`); this helper guarantees the client-facing
//! invariants regardless of upstream behaviour:
//!
//! * `top_n` is clamped to the document count (silently) before the upstream
//!   call, and the result set is truncated to it afterwards (TEI, for instance,
//!   has no `top_n` and returns every document);
//! * results are sorted by descending `relevance_score` — the gateway never
//!   trusts the upstream to have ordered them;
//! * source document texts are echoed into `document` only when
//!   `return_documents` was set (bandwidth-saving default is off).

use std::cmp::Ordering;

use ferrogate_core::{
    ProviderError, RerankProvider, RerankRequest, RerankResponse, RerankResultDocument,
};
use tokio_util::sync::CancellationToken;

/// Run a rerank through `provider`, applying the gateway invariants above.
///
/// Cancellation propagates to the in-flight upstream call.
pub async fn rerank(
    provider: &dyn RerankProvider,
    mut req: RerankRequest,
    cancel: &CancellationToken,
) -> Result<RerankResponse, ProviderError> {
    let n_docs = req.documents.len();

    // Clamp top_n to the document count (silent — spec 3.1). Done before the
    // upstream call so providers that honour top_n receive the clamped value.
    if let Some(top_n) = req.top_n {
        if top_n as usize > n_docs {
            req.top_n = Some(u32::try_from(n_docs).unwrap_or(u32::MAX));
        }
    }
    let top_n = req.top_n;
    let return_documents = req.return_documents;

    // Capture source texts for echoing only when needed (avoids an allocation
    // and a copy of potentially large documents on the common path).
    let texts: Vec<String> = if return_documents {
        req.documents.iter().map(|d| d.text().to_owned()).collect()
    } else {
        Vec::new()
    };

    let mut resp = provider.rerank(req, cancel.clone()).await?;

    // Defensive ordering: stable sort by descending score keeps upstream order
    // on ties. NaN scores (should never happen) sort as equal rather than panic.
    resp.results.sort_by(|a, b| {
        b.relevance_score
            .partial_cmp(&a.relevance_score)
            .unwrap_or(Ordering::Equal)
    });

    // Truncate for providers that ignore top_n (e.g. TEI returns everything).
    if let Some(n) = top_n {
        resp.results.truncate(n as usize);
    }

    if return_documents {
        for result in &mut resp.results {
            if let Some(text) = texts.get(result.index as usize) {
                result.document = Some(RerankResultDocument { text: text.clone() });
            }
        }
    }

    Ok(resp)
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // exact scores are set by the tests themselves
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ferrogate_core::{RerankResult, RerankUsage};

    /// A provider that returns the given results verbatim, recording the
    /// `top_n` of each call so the clamp can be asserted.
    struct StubProvider {
        results: Vec<RerankResult>,
        seen_top_n: std::sync::Mutex<Vec<Option<u32>>>,
    }

    impl StubProvider {
        fn new(results: Vec<RerankResult>) -> Self {
            Self {
                results,
                seen_top_n: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl RerankProvider for StubProvider {
        async fn rerank(
            &self,
            req: RerankRequest,
            _cancel: CancellationToken,
        ) -> Result<RerankResponse, ProviderError> {
            self.seen_top_n.lock().expect("lock").push(req.top_n);
            Ok(RerankResponse {
                results: self.results.clone(),
                usage: RerankUsage { search_units: 1 },
            })
        }
    }

    fn result(index: u32, score: f32) -> RerankResult {
        RerankResult {
            index,
            relevance_score: score,
            document: None,
        }
    }

    fn request(docs: &[&str], top_n: Option<u32>, return_documents: bool) -> RerankRequest {
        RerankRequest {
            model: "m".to_owned(),
            query: "q".to_owned(),
            documents: docs
                .iter()
                .map(|s| ferrogate_core::RerankDocument::Text((*s).to_owned()))
                .collect(),
            top_n,
            return_documents,
        }
    }

    #[tokio::test]
    async fn results_are_sorted_by_descending_score() {
        // Upstream returns them out of order.
        let provider = StubProvider::new(vec![result(0, 0.1), result(1, 0.9), result(2, 0.5)]);
        let req = request(&["a", "b", "c"], None, false);
        let resp = rerank(&provider, req, &CancellationToken::new())
            .await
            .unwrap();
        let order: Vec<u32> = resp.results.iter().map(|r| r.index).collect();
        assert_eq!(order, vec![1, 2, 0]);
        // Indices still point at the ORIGINAL positions.
        assert_eq!(resp.results[0].relevance_score, 0.9);
    }

    #[tokio::test]
    async fn top_n_larger_than_documents_is_clamped_silently() {
        let provider = StubProvider::new(vec![result(0, 0.5), result(1, 0.4)]);
        let req = request(&["a", "b"], Some(99), false);
        let _ = rerank(&provider, req, &CancellationToken::new())
            .await
            .unwrap();
        // The provider saw the clamped value, not 99.
        let seen = provider.seen_top_n.lock().unwrap();
        assert_eq!(seen.as_slice(), &[Some(2)]);
    }

    #[tokio::test]
    async fn top_n_truncates_results_for_providers_that_ignore_it() {
        // Provider ignores top_n and returns all three.
        let provider = StubProvider::new(vec![result(0, 0.1), result(1, 0.9), result(2, 0.5)]);
        let req = request(&["a", "b", "c"], Some(2), false);
        let resp = rerank(&provider, req, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(resp.results.len(), 2);
        // The two highest-scoring, in order.
        assert_eq!(resp.results[0].index, 1);
        assert_eq!(resp.results[1].index, 2);
    }

    #[tokio::test]
    async fn documents_echoed_only_when_requested() {
        let provider = StubProvider::new(vec![result(1, 0.9), result(0, 0.1)]);
        let req = request(&["first", "second"], None, true);
        let resp = rerank(&provider, req, &CancellationToken::new())
            .await
            .unwrap();
        // Echoed text matches the ORIGINAL index, not the sorted position.
        assert_eq!(resp.results[0].index, 1);
        assert_eq!(resp.results[0].document.as_ref().unwrap().text, "second");
        assert_eq!(resp.results[1].document.as_ref().unwrap().text, "first");
    }

    #[tokio::test]
    async fn documents_absent_by_default() {
        let provider = StubProvider::new(vec![result(0, 0.9)]);
        let req = request(&["only"], None, false);
        let resp = rerank(&provider, req, &CancellationToken::new())
            .await
            .unwrap();
        assert!(resp.results[0].document.is_none());
    }
}
