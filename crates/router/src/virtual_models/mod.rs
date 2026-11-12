//! Virtual models (ADR 014): admin-defined public ids that carry routing
//! logic over the foundation models - `single`, `fallback`, `split` and
//! `switch` strategies, per-target overrides, chat presets and a decisions
//! rerank remap. Config is compiled once per load into a routing table; a
//! request only runs a pure decide step. An account's own virtual models
//! (ADR 022) compile into a separate table over the platform's.

pub mod condition;
pub mod config;
pub mod decide;
pub mod overrides;
pub mod table;

pub use condition::{Condition, FactSource, MAX_CONDITIONS, MAX_REGEX_LEN, REGEX_SIZE_LIMIT};
pub use config::VirtualModelConfig;
pub use decide::{Attempt, Decision};
pub use overrides::{Overridable, Overrides, Preset};
pub use table::{
    FoundationIndex, Problem, ProblemKind, RoutingConfigError, RoutingTable, VirtualModel,
    MAX_ATTEMPTS, MAX_DEPTH, MAX_REGEXES, MAX_TARGETS,
};
