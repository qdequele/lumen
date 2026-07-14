//! Gateway added-latency micro-benchmarks (M7 §7.1).
//!
//! These measure the CPU work the gateway adds *per request, off the network*:
//! the M6 resilience executor wrapping a provider call (circuit-breaker admit,
//! retry loop, per-attempt timeout) and the JSON (de)serialization the OpenAI
//! surface performs. No sockets are involved — the "provider" resolves
//! instantly — so the numbers isolate gateway overhead from upstream/network
//! latency, matching the "< 1 ms added p99 off-network" target.
//!
//! Run with `cargo bench -p server`. The full head-to-head vs LiteLLM under
//! load lives in `bench/` (docker-compose + k6) and is documented in
//! `docs/perf-baseline.md`.

use std::hint::black_box;
use std::time::Duration;

use async_trait::async_trait;
use criterion::{criterion_group, criterion_main, Criterion};
use futures::stream::BoxStream;
use lumen_core::{
    ChatChoice, ChatMessage, ChatProvider, ChatRequest, ChatResponse, ProviderError, Usage,
};
use lumen_router::circuit::{BreakerConfig, CircuitBreakers};
use lumen_router::executor::{execute, ExecConfig, Link};
use lumen_router::retry::RetryPolicy;
use tokio_util::sync::CancellationToken;

/// A provider that returns a canned response with zero I/O — the constant part
/// of the gateway pipeline under test.
struct InstantProvider {
    response: ChatResponse,
}

#[async_trait]
impl ChatProvider for InstantProvider {
    async fn chat(
        &self,
        _req: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<ChatResponse, ProviderError> {
        Ok(self.response.clone())
    }

    async fn chat_stream(
        &self,
        _req: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<lumen_core::ChatChunk, ProviderError>>, ProviderError>
    {
        Ok(Box::pin(futures::stream::empty()))
    }
}

fn sample_request() -> ChatRequest {
    serde_json::from_str(
        r#"{"model":"gpt-4o","messages":[
            {"role":"system","content":"You are a helpful assistant."},
            {"role":"user","content":"Explain the CAP theorem in two sentences."}
        ],"temperature":0.7}"#,
    )
    .expect("valid request fixture")
}

fn sample_response() -> ChatResponse {
    ChatResponse {
        id: "chatcmpl-1".to_owned(),
        object: "chat.completion".to_owned(),
        created: 1,
        model: "gpt-4o".to_owned(),
        choices: vec![ChatChoice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_owned(),
                content: Some(
                    "Consistency, availability and partition tolerance cannot all be \
                     guaranteed at once. Under a partition you must trade one for another."
                        .to_owned(),
                ),
                name: None,
                extra: serde_json::Map::new(),
            },
            finish_reason: Some("stop".to_owned()),
        }],
        usage: Some(Usage {
            prompt_tokens: 20,
            completion_tokens: 30,
            total_tokens: 50,
            estimated: None,
        }),
        extra: serde_json::Map::new(),
    }
}

/// The resilience executor overhead around a zero-cost provider call.
fn bench_executor(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let provider = InstantProvider {
        response: sample_response(),
    };
    let links = vec![Link {
        provider_name: "openai".to_owned(),
        model_id: "gpt-4o".to_owned(),
    }];
    let breakers = CircuitBreakers::new(BreakerConfig::default(), None);
    let config = ExecConfig {
        retry: RetryPolicy::default(),
        first_token: Duration::from_secs(30),
        total: Duration::from_secs(600),
    };
    let cancel = CancellationToken::new();
    let req = sample_request();

    let provider = &provider;
    c.bench_function("executor_overhead_chat", |b| {
        b.to_async(&rt).iter(|| async {
            // The closure returns the provider's (borrowed) future directly, so
            // nothing is moved out of the FnMut across retries.
            let out = execute(&links, &breakers, &config, &cancel, |_i| {
                provider.chat(req.clone(), cancel.clone())
            })
            .await
            .expect("executed");
            black_box(out.model_used);
        });
    });
}

/// The JSON round-trip the OpenAI surface performs per request.
fn bench_json_roundtrip(c: &mut Criterion) {
    let raw = br#"{"model":"gpt-4o","messages":[
        {"role":"system","content":"You are a helpful assistant."},
        {"role":"user","content":"Explain the CAP theorem in two sentences."}
    ],"temperature":0.7}"#;
    let response = sample_response();

    c.bench_function("json_request_deserialize", |b| {
        b.iter(|| {
            let req: ChatRequest = serde_json::from_slice(black_box(raw)).expect("parse");
            black_box(req.model);
        });
    });
    c.bench_function("json_response_serialize", |b| {
        b.iter(|| {
            let bytes = serde_json::to_vec(black_box(&response)).expect("serialize");
            black_box(bytes.len());
        });
    });
}

criterion_group!(benches, bench_executor, bench_json_roundtrip);
criterion_main!(benches);
