//! Virtual models (ADR 014): admin-defined public ids that carry routing
//! logic over the foundation models - `single`, `fallback`, `split` and
//! `switch` strategies, per-target overrides, chat presets and a SystemOne
//! rerank remap. Config is compiled once per load into a routing table; a
//! request only runs a pure decide step.

pub mod condition;
pub mod config;
pub mod decide;
pub mod overrides;
pub mod table;

pub use condition::{Condition, FactSource};
pub use config::VirtualModelConfig;
pub use overrides::{Overridable, Overrides, Preset};
pub use table::{FoundationIndex, RoutingConfigError, RoutingTable, VirtualModel, MAX_DEPTH};
