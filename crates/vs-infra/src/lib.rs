//! Infrastructure adapters. See `SPEC.md`.

pub mod blob;
pub mod config;
pub mod telemetry;

pub use blob::{BlobError, BlobStore, ObjectBlobStore};
pub use config::Config;
