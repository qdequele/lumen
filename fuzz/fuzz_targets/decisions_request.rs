#![no_main]
//! Fuzz the decisions client-input boundary (ADR 017): format detection and
//! parsing (auto-detected and TypeSafe-forced, as `/v1/systemone` does), the
//! token estimate, per-target limits, both upstream encoders, and both edge
//! renderers over a synthesized response must never panic.
use lumen_core::decisions::format::{self, Format};
use lumen_core::decisions::{
    Answer, DecisionLimits, DecisionRequest, DecisionResponse, QuestionKind,
};
use lumen_providers::decisions::{family, openai};
use libfuzzer_sys::fuzz_target;

/// One answer per question, matching its kind.
fn synthesize(req: &DecisionRequest) -> DecisionResponse {
    let answers = req
        .questions()
        .iter()
        .map(|q| match &q.kind {
            QuestionKind::Predicate { .. } => Answer::Predicate { probability: 0.5 },
            QuestionKind::Choice { choices } => match choices.first() {
                Some(first) => Answer::Choice {
                    choice: first.value.clone(),
                    probabilities: choices.iter().map(|c| (c.value.clone(), 0.5)).collect(),
                    confidence: Some(0.5),
                },
                None => Answer::Refusal,
            },
            QuestionKind::Score { levels } => Answer::Score {
                score: 0.0,
                probabilities: (0..levels.len()).map(|i| (i, 0.5)).collect(),
                confidence: None,
            },
        })
        .collect();
    DecisionResponse {
        model: req.model.clone(),
        answers,
        usage: None,
        upstream: None,
    }
}

fn exercise(req: &DecisionRequest) {
    let _ = lumen_core::tokens::estimate_decisions(req);
    let _ = DecisionLimits::TYPESAFE.check(req, "typesafe");
    let _ = openai::LIMITS.check(req, "openai");
    let ids = family::wire_ids(req.questions());
    for profile in [
        family::FamilyProfile::typesafe(true),
        family::FamilyProfile::perplexity(),
        family::FamilyProfile::ollama(),
        family::FamilyProfile::cloudflare(),
    ] {
        let _ = family::encode(req, "up", &profile, &ids);
    }
    let _ = openai::encode(req, "up");
    let resp = synthesize(req);
    let _ = format::render(Format::TypeSafe, &resp, req);
    let _ = format::render(Format::OpenAi, &resp, req);
}

fuzz_target!(|data: &[u8]| {
    for forced in [None, Some(Format::TypeSafe)] {
        if let Ok((_, req)) = format::parse(data, forced) {
            exercise(&req);
        }
    }
});
