//! Workflow scoring and selection. See `SPEC.md`.

pub mod select;

pub use select::{select, Objective, PageEval, Trust, Weights};
