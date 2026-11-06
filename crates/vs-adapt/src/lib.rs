//! Workflow generation, scoring and selection. See `SPEC.md`.

pub mod generate;
pub mod metrics;
pub mod records;
pub mod report;
pub mod run;
pub mod score;
pub mod select;

pub use generate::{generate, Generated, SearchSpace, Template};
pub use records::{read_jsonl, write_jsonl, AdaptPageResult, RunRecord};
pub use score::{Judge, MetricWeights, ScoreOptions};
pub use select::{select, Metrics, Objective, PageEval, Trust, Weights};
