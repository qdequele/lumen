//! What a decision target accepts (spec 7.3). Limits are data on each
//! provider kind, so a cross-vendor chain skips an incompatible target
//! before any upstream call.

use super::{ChoiceValue, DecisionRequest, QuestionKind};
use crate::error::GatewayError;

/// How a rerank remap packs documents into calls for a target (spec 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackLimits {
    /// Estimated input tokens per call (state plus questions).
    pub max_call_tokens: u64,
    /// Documents per call.
    pub max_docs_per_call: usize,
    /// Calls in flight at once.
    pub concurrency: usize,
}

/// The request shapes a decision target accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecisionLimits {
    /// Most questions per request, if capped.
    pub max_questions: Option<usize>,
    /// Fewest options of a `choice`.
    pub min_choice_options: usize,
    /// Most options of a `choice`.
    pub max_choice_options: usize,
    /// Fewest levels of a `score`.
    pub min_score_levels: usize,
    /// Most levels of a `score`.
    pub max_score_levels: usize,
    /// Whether a predicate needs `instructions` (Jev).
    pub predicate_needs_instructions: bool,
    /// Whether choices travel as object keys, so `"true"` and `true` collide.
    pub string_keyed_choices: bool,
    /// Most images per request, if capped.
    pub max_images: Option<usize>,
    /// Rerank packing.
    pub pack: PackLimits,
}

impl DecisionLimits {
    /// The TypeSafe format contract (Jev's limits without its `noul` rule),
    /// the base every TypeSafe-family profile starts from.
    pub const TYPESAFE: Self = Self {
        max_questions: None,
        min_choice_options: 1,
        max_choice_options: 255,
        min_score_levels: 1,
        max_score_levels: 10,
        predicate_needs_instructions: false,
        string_keyed_choices: true,
        max_images: None,
        pack: PackLimits {
            max_call_tokens: 48_000,
            max_docs_per_call: 100,
            concurrency: 4,
        },
    };

    /// Check `req` against these limits.
    ///
    /// # Errors
    /// [`GatewayError::InvalidRequest`] (`LM-1001`) naming the question (or
    /// the count) and `provider`.
    pub fn check(&self, req: &DecisionRequest, provider: &str) -> Result<(), GatewayError> {
        match self.violation(req) {
            None => Ok(()),
            Some(what) => Err(GatewayError::InvalidRequest(format!(
                "{what} is not supported by provider '{provider}'"
            ))),
        }
    }

    /// The first shape of `req` these limits reject, as a bare reason naming
    /// the question (or the count) and the limit, without the provider (for
    /// errors that name it themselves); `None` when `req` fits. Allocates
    /// only on a violation.
    #[must_use]
    pub fn violation(&self, req: &DecisionRequest) -> Option<String> {
        let questions = req.questions();
        if let Some(max) = self.max_questions {
            if questions.len() > max {
                return Some(format!(
                    "a request of {} questions (at most {max} questions)",
                    questions.len()
                ));
            }
        }
        if let Some(max) = self.max_images {
            let n = req.image_count();
            if n > max {
                return Some(format!("a request of {n} images (at most {max} images)"));
            }
        }
        for (i, q) in questions.iter().enumerate() {
            match &q.kind {
                QuestionKind::Predicate { .. } => {
                    if self.predicate_needs_instructions && q.instructions.is_none() {
                        return Some(format!(
                            "question {}: a predicate without instructions",
                            q.label(i)
                        ));
                    }
                }
                QuestionKind::Choice { choices } => {
                    let n = choices.len();
                    if n < self.min_choice_options || n > self.max_choice_options {
                        return Some(format!(
                            "question {}: a choice of {n} options (needs {} to {})",
                            q.label(i),
                            self.min_choice_options,
                            self.max_choice_options
                        ));
                    }
                    if self.string_keyed_choices && has_spelling_collision(choices) {
                        return Some(format!(
                            "question {}: a string and a boolean choice with the same spelling",
                            q.label(i)
                        ));
                    }
                }
                QuestionKind::Score { levels } => {
                    let n = levels.len();
                    if n < self.min_score_levels || n > self.max_score_levels {
                        return Some(format!(
                            "question {}: a score of {n} levels (needs {} to {})",
                            q.label(i),
                            self.min_score_levels,
                            self.max_score_levels
                        ));
                    }
                }
            }
        }
        None
    }
}

fn has_spelling_collision(choices: &[super::ChoiceOption]) -> bool {
    choices.iter().any(|a| {
        matches!(a.value, ChoiceValue::Bool(_))
            && choices.iter().any(|b| {
                matches!(b.value, ChoiceValue::Str(_)) && b.value.wire_key() == a.value.wire_key()
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decisions::*;
    use crate::error::GatewayError;

    fn choice(values: &[ChoiceValue]) -> Question {
        Question {
            name: Some("pick".into()),
            instructions: Some(Text::Plain("q".into())),
            kind: QuestionKind::Choice {
                choices: values
                    .iter()
                    .map(|v| ChoiceOption {
                        value: v.clone(),
                        description: None,
                    })
                    .collect(),
            },
            raw: None,
        }
    }

    fn req(questions: Vec<Question>) -> DecisionRequest {
        DecisionRequest::new("m".into(), Input::Text("x".into()), questions)
    }

    fn message(r: Result<(), GatewayError>) -> String {
        match r {
            Err(GatewayError::InvalidRequest(m)) => m,
            other => panic!("expected LM-1001, got {other:?}"),
        }
    }

    #[test]
    fn a_choice_of_one_is_rejected_where_two_are_needed() {
        let limits = DecisionLimits {
            min_choice_options: 2,
            ..DecisionLimits::TYPESAFE
        };
        let m = message(limits.check(
            &req(vec![choice(&[ChoiceValue::Str("a".into())])]),
            "openai",
        ));
        assert!(m.contains("`pick`") && m.contains("openai"), "{m}");
        assert!(DecisionLimits::TYPESAFE
            .check(
                &req(vec![choice(&[ChoiceValue::Str("a".into())])]),
                "typesafe"
            )
            .is_ok());
    }

    #[test]
    fn string_and_bool_with_the_same_spelling_collide_on_string_keyed_wires() {
        let q = choice(&[ChoiceValue::Str("true".into()), ChoiceValue::Bool(true)]);
        let m = message(DecisionLimits::TYPESAFE.check(&req(vec![q.clone()]), "perplexity"));
        assert!(m.contains("same spelling"), "{m}");
        let typed = DecisionLimits {
            string_keyed_choices: false,
            ..DecisionLimits::TYPESAFE
        };
        assert!(typed.check(&req(vec![q]), "openai").is_ok());
    }

    #[test]
    fn question_and_image_counts_are_capped() {
        let limits = DecisionLimits {
            max_questions: Some(1),
            ..DecisionLimits::TYPESAFE
        };
        let two = vec![
            choice(&[ChoiceValue::Str("a".into())]),
            choice(&[ChoiceValue::Str("b".into())]),
        ];
        assert!(message(limits.check(&req(two), "cf")).contains("at most 1 question"));
        let limits = DecisionLimits {
            max_images: Some(1),
            ..DecisionLimits::TYPESAFE
        };
        let img = || {
            Part::Image(Image {
                data_url: "data:x".into(),
                detail: None,
            })
        };
        let r = DecisionRequest::new(
            "m".into(),
            Input::Messages(vec![img(), img()]),
            vec![choice(&[ChoiceValue::Str("a".into())])],
        );
        assert!(message(limits.check(&r, "cf")).contains("at most 1 image"));
    }

    #[test]
    fn jev_needs_predicate_instructions() {
        let q = Question {
            name: Some("crit".into()),
            instructions: None,
            kind: QuestionKind::Predicate {
                criteria: Some(PredicateCriteria {
                    when_true: Some(Text::Plain("y".into())),
                    when_false: None,
                }),
            },
            raw: None,
        };
        let jev = DecisionLimits {
            predicate_needs_instructions: true,
            ..DecisionLimits::TYPESAFE
        };
        assert!(message(jev.check(&req(vec![q.clone()]), "typesafe")).contains("instructions"));
        assert!(DecisionLimits::TYPESAFE
            .check(&req(vec![q]), "perplexity")
            .is_ok());
    }

    #[test]
    fn score_level_bounds_apply() {
        let q = Question {
            name: None,
            instructions: Some(Text::Plain("q".into())),
            kind: QuestionKind::Score {
                levels: (0..27)
                    .map(|i| Level {
                        label: Text::Plain(i.to_string()),
                        description: None,
                    })
                    .collect(),
            },
            raw: None,
        };
        let ollama = DecisionLimits {
            max_score_levels: 26,
            ..DecisionLimits::TYPESAFE
        };
        assert!(message(ollama.check(&req(vec![q]), "ollama")).contains("#0"));
    }
}
