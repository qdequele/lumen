//! ADR 014: the decide phase must stay under 1 us for a 3-level plan with a
//! regex rule, and a foundation id called directly (decide plus the retain
//! pass every handler runs) must stay close to free.

use criterion::{criterion_group, criterion_main, Criterion};
use lumen_core::Capability;
use lumen_router::virtual_models::{FactSource, FoundationIndex, RoutingTable, VirtualModelConfig};
use serde_json::Value;

struct Facts;

impl FactSource for Facts {
    fn group(&self) -> Option<&str> {
        Some("tenant-42")
    }
    fn metadata(&self, _: &str) -> Option<&Value> {
        None
    }
    fn has_images(&self) -> bool {
        false
    }
    fn has_tools(&self) -> bool {
        false
    }
    fn stream(&self) -> bool {
        false
    }
    fn input_tokens(&self) -> u64 {
        1000
    }
    fn documents(&self) -> Option<u64> {
        None
    }
}

fn table() -> RoutingTable {
    #[derive(serde::Deserialize)]
    struct Doc {
        virtual_models: Vec<VirtualModelConfig>,
    }
    let doc: Doc = toml::from_str(
        r#"
        [[virtual_models]]
        id = "acme/chat"
        capability = "chat"
        strategy = "switch"
        targets = [{ when = { group = { regex = "^tenant-[0-9]+$" } }, model = "acme/tenant" }, { model = "a" }]

        [[virtual_models]]
        id = "acme/tenant"
        capability = "chat"
        strategy = "fallback"
        targets = [{ model = "acme/split" }, { model = "c" }]

        [[virtual_models]]
        id = "acme/split"
        capability = "chat"
        strategy = "split"
        targets = [{ model = "a", weight = 80 }, { model = "b", weight = 20 }]
        "#,
    )
    .expect("bench config parses");
    let mut foundation = FoundationIndex::default();
    for id in ["a", "b", "c"] {
        foundation.insert(id, vec![Capability::Chat], vec!["text".into()]);
    }
    RoutingTable::compile(&doc.virtual_models, &foundation)
        .expect("bench config compiles")
        .0
}

fn bench(c: &mut Criterion) {
    let table = table();
    let mut n = 0u64;
    c.bench_function("decide_3_levels_with_regex", |b| {
        b.iter(|| {
            let mut rng = || {
                n = n.wrapping_add(7);
                n
            };
            std::hint::black_box(
                table
                    .decide(Capability::Chat, "acme/chat", &Facts, &mut rng)
                    .expect("decides"),
            )
        });
    });
}

fn bench_direct(c: &mut Criterion) {
    let table = table();
    let mut rng = || 0;
    c.bench_function("decide_direct_foundation_id", |b| {
        b.iter(|| {
            let mut decision = table
                .decide(
                    Capability::Chat,
                    std::hint::black_box("a"),
                    &Facts,
                    &mut rng,
                )
                .expect("decides");
            decision.retain_mask(&[true]);
            std::hint::black_box(decision)
        });
    });
}

criterion_group!(benches, bench, bench_direct);
criterion_main!(benches);
