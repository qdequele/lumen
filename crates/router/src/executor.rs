//! The resilience execution layer (M6, ADR 005).
//!
//! [`execute`] runs one capability call across a resolved fallback **chain**
//! (the requested model followed by its configured fallbacks), applying — per
//! link — the circuit-breaker gate, a first-token timeout on each attempt, and
//! the retry loop; and — across links — fallback when a link is exhausted or
//! its breaker is open. The whole call is bounded by the total timeout. It is
//! generic over a closure that performs the actual typed call for a given link
//! index, so one implementation serves chat (streaming *open* and non-streaming),
//! embeddings and reranking alike.
//!
//! Streaming reuses this unchanged: the closure *opens* the upstream byte
//! stream, so a retry/fallback can only happen before the first byte reaches
//! the client (spec 6.1/6.2). Once the stream opens, the caller's frame guards
//! own the rest and never retry.

use std::future::Future;
use std::time::Duration;

use ferrogate_core::{GatewayError, ProviderError};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::circuit::{Admission, CircuitBreakers};
use crate::retry::{retry, RetryPolicy};

/// One link of a fallback chain: the metadata the executor keys on. The actual
/// call is supplied by the caller's closure, indexed by position.
#[derive(Debug, Clone)]
pub struct Link {
    /// Provider instance name (breaker key + upstream-error attribution).
    pub provider_name: String,
    /// Client-facing model id (breaker key + `x-ferrogate-model-used`).
    pub model_id: String,
}

/// Resilience knobs for one execution.
#[derive(Debug, Clone, Copy)]
pub struct ExecConfig {
    /// Retry policy applied per link.
    pub retry: RetryPolicy,
    /// Per-attempt first-token deadline (whole call for non-streaming; time to
    /// the opened stream for streaming).
    pub first_token: Duration,
    /// Absolute cap on the whole call (all retries and fallbacks together).
    pub total: Duration,
}

/// A successful execution plus which link actually served it.
#[derive(Debug)]
pub struct Executed<T> {
    /// The successful value.
    pub value: T,
    /// The client-facing model id that served the request (may be a fallback).
    pub model_used: String,
    /// The provider that served it.
    pub provider_used: String,
}

/// Execute `call` across `links` with retries, fallback, circuit breaking and
/// the total timeout.
///
/// `call(i)` must return a *fresh* future performing the call for link `i` each
/// time it is invoked (retries and fallbacks call it again). A first-token
/// timeout wraps each attempt; the total timeout wraps the whole chain.
///
/// # Errors
///
/// The last upstream error encountered (already mapped to a [`GatewayError`]
/// naming the provider), `FG-3013` if the total timeout elapsed, `FG-3020` if
/// every remaining link's breaker was open, or the immediate error for a hard
/// client fault (which no fallback can fix).
pub async fn execute<T, F, Fut>(
    links: &[Link],
    breakers: &CircuitBreakers,
    config: &ExecConfig,
    cancel: &CancellationToken,
    mut call: F,
) -> Result<Executed<T>, GatewayError>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    let Some(primary) = links.first() else {
        return Err(GatewayError::Internal("empty provider chain".to_owned()));
    };
    let primary_provider = primary.provider_name.clone();

    let deadline = Instant::now() + config.total;
    let chain = run_chain(links, breakers, config, cancel, &mut call);
    match tokio::time::timeout_at(deadline, chain).await {
        Ok(result) => result,
        // The whole request outran the total timeout (spec 6.4). Attribute it
        // to the request's intended target.
        Err(_) => Err(GatewayError::UpstreamTotalTimeout {
            provider: primary_provider,
        }),
    }
}

/// The fallback loop, run inside the total-timeout envelope.
async fn run_chain<T, F, Fut>(
    links: &[Link],
    breakers: &CircuitBreakers,
    config: &ExecConfig,
    cancel: &CancellationToken,
    call: &mut F,
) -> Result<Executed<T>, GatewayError>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    let mut last_error: Option<GatewayError> = None;

    for (index, link) in links.iter().enumerate() {
        let breaker = breakers.get(&link.provider_name, &link.model_id);
        if let Admission::Rejected { retry_after } = breaker.admit(Instant::now()) {
            // Circuit open: skip this link entirely (never touches the upstream)
            // and fall through to the next fallback.
            last_error = Some(GatewayError::CircuitOpen {
                provider: link.provider_name.clone(),
                retry_after: Some(retry_after),
            });
            continue;
        }

        let first_token = config.first_token;
        let outcome = retry(&config.retry, cancel, || {
            // Invoke the call synchronously so the returned future is owned and
            // does not hold a borrow of `call` across the await (which an FnMut
            // closure cannot let escape).
            let attempt = call(index);
            let provider = link.provider_name.clone();
            async move {
                match tokio::time::timeout(first_token, attempt).await {
                    Ok(result) => result,
                    Err(_) => Err(ProviderError::FirstTokenTimeout { provider }),
                }
            }
        })
        .await;

        match outcome {
            Ok(value) => {
                breaker.on_success();
                return Ok(Executed {
                    value,
                    model_used: link.model_id.clone(),
                    provider_used: link.provider_name.clone(),
                });
            }
            Err(error) => {
                if error.is_provider_fault() {
                    breaker.on_failure(Instant::now());
                }
                let mapped = GatewayError::from_provider(&link.provider_name, error.clone());
                // A hard client/deterministic fault (bad request, cancellation,
                // schema mismatch) is not helped by a fallback → return now.
                if !error.is_retryable() && !error.is_provider_fault() {
                    return Err(mapped);
                }
                last_error = Some(mapped);
            }
        }
    }

    Err(
        last_error.unwrap_or_else(|| GatewayError::UpstreamUnavailable {
            provider: links
                .first()
                .map_or_else(|| "unknown".to_owned(), |l| l.provider_name.clone()),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::circuit::BreakerConfig;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn links(pairs: &[(&str, &str)]) -> Vec<Link> {
        pairs
            .iter()
            .map(|(p, m)| Link {
                provider_name: (*p).to_owned(),
                model_id: (*m).to_owned(),
            })
            .collect()
    }

    fn config(max_attempts: u32) -> ExecConfig {
        ExecConfig {
            retry: RetryPolicy {
                max_attempts,
                base: Duration::from_millis(200),
                max: Duration::from_secs(5),
            },
            first_token: Duration::from_secs(30),
            total: Duration::from_secs(600),
        }
    }

    fn breakers() -> CircuitBreakers {
        CircuitBreakers::new(BreakerConfig::default(), None)
    }

    fn upstream_500() -> ProviderError {
        ProviderError::Upstream {
            provider: String::new(),
            status: 500,
            retryable: true,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retries_then_succeeds_on_the_primary() {
        let chain = links(&[("openai", "gpt-4o")]);
        let cb = breakers();
        let cancel = CancellationToken::new();
        let calls = AtomicUsize::new(0);
        let out = execute(&chain, &cb, &config(3), &cancel, |_i| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    Err(upstream_500())
                } else {
                    Ok::<_, ProviderError>("ok")
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(out.value, "ok");
        assert_eq!(out.model_used, "gpt-4o");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn falls_back_after_primary_exhausts_retries() {
        let chain = links(&[("openai", "gpt-4o"), ("anthropic", "claude")]);
        let cb = breakers();
        let cancel = CancellationToken::new();
        let calls = AtomicUsize::new(0);
        let out = execute(&chain, &cb, &config(2), &cancel, |i| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if i == 0 {
                    Err(upstream_500()) // primary always fails
                } else {
                    Ok::<_, ProviderError>("from-fallback")
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(out.value, "from-fallback");
        assert_eq!(out.model_used, "claude");
        assert_eq!(out.provider_used, "anthropic");
        // 2 attempts on the primary + 1 on the fallback.
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn open_circuit_skips_the_link_without_calling_it() {
        let chain = links(&[("openai", "gpt-4o"), ("anthropic", "claude")]);
        let cb = breakers();
        // Force the primary's breaker open.
        let primary = cb.get("openai", "gpt-4o");
        let now = Instant::now();
        for _ in 0..BreakerConfig::default().failure_threshold {
            primary.on_failure(now);
        }
        let cancel = CancellationToken::new();
        let primary_calls = AtomicUsize::new(0);
        let out = execute(&chain, &cb, &config(3), &cancel, |i| {
            if i == 0 {
                primary_calls.fetch_add(1, Ordering::SeqCst);
            }
            async move { Ok::<_, ProviderError>(i) }
        })
        .await
        .unwrap();
        assert_eq!(out.model_used, "claude", "should skip straight to fallback");
        assert_eq!(
            primary_calls.load(Ordering::SeqCst),
            0,
            "primary never called"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn all_links_open_yields_circuit_open_fg_3020() {
        let chain = links(&[("openai", "gpt-4o")]);
        let cb = breakers();
        let primary = cb.get("openai", "gpt-4o");
        let now = Instant::now();
        for _ in 0..BreakerConfig::default().failure_threshold {
            primary.on_failure(now);
        }
        let cancel = CancellationToken::new();
        let err = execute(&chain, &cb, &config(3), &cancel, |_i| async {
            Ok::<_, ProviderError>(())
        })
        .await
        .unwrap_err();
        assert_eq!(err.code(), "FG-3020");
        assert!(err.retry_after().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn hard_client_fault_returns_immediately_without_fallback() {
        let chain = links(&[("openai", "gpt-4o"), ("anthropic", "claude")]);
        let cb = breakers();
        let cancel = CancellationToken::new();
        let fallback_calls = AtomicUsize::new(0);
        let err = execute(&chain, &cb, &config(3), &cancel, |i| {
            if i == 1 {
                fallback_calls.fetch_add(1, Ordering::SeqCst);
            }
            async move {
                Err::<(), _>(ProviderError::Upstream {
                    provider: "openai".to_owned(),
                    status: 400,
                    retryable: false,
                })
            }
        })
        .await
        .unwrap_err();
        assert_eq!(err.code(), "FG-3003"); // upstream error status, 502
        assert_eq!(
            fallback_calls.load(Ordering::SeqCst),
            0,
            "4xx must not fall back"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn total_timeout_yields_fg_3013() {
        let chain = links(&[("openai", "gpt-4o")]);
        let cb = breakers();
        let cancel = CancellationToken::new();
        let cfg = ExecConfig {
            retry: RetryPolicy::never(),
            first_token: Duration::from_secs(30),
            total: Duration::from_millis(100),
        };
        let err = execute(&chain, &cb, &cfg, &cancel, |_i| async {
            // Never resolves within the total window.
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok::<_, ProviderError>(())
        })
        .await
        .unwrap_err();
        assert_eq!(err.code(), "FG-3013");
    }

    #[tokio::test(start_paused = true)]
    async fn first_token_timeout_per_attempt_surfaces_fg_3011() {
        let chain = links(&[("openai", "gpt-4o")]);
        let cb = breakers();
        let cancel = CancellationToken::new();
        let cfg = ExecConfig {
            retry: RetryPolicy::never(),
            first_token: Duration::from_millis(50),
            total: Duration::from_secs(600),
        };
        let err = execute(&chain, &cb, &cfg, &cancel, |_i| async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok::<_, ProviderError>(())
        })
        .await
        .unwrap_err();
        assert_eq!(err.code(), "FG-3011");
    }

    #[tokio::test(start_paused = true)]
    async fn success_after_a_failure_streak_closes_the_breaker() {
        let chain = links(&[("openai", "gpt-4o")]);
        let cb = breakers();
        let cancel = CancellationToken::new();
        // One request whose primary fails all retries records a breaker failure.
        let _ = execute(&chain, &cb, &config(2), &cancel, |_i| async {
            Err::<(), _>(upstream_500())
        })
        .await;
        // A later success closes/keeps the breaker closed.
        let out = execute(&chain, &cb, &config(2), &cancel, |_i| async {
            Ok::<_, ProviderError>("ok")
        })
        .await
        .unwrap();
        assert_eq!(out.value, "ok");
        assert_eq!(
            cb.get("openai", "gpt-4o").state(),
            crate::circuit::CircuitState::Closed
        );
    }
}
