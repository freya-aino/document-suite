//! Infrastructure adapters. See `SPEC.md`.

pub mod blob;
pub mod config;
pub mod queue;
pub mod telemetry;

pub use blob::{BlobError, BlobStore, ObjectBlobStore};
pub use config::Config;
pub use queue::{Delivery, Exhausted, Lane, NatsBus, QueueError, Role, TaskBus};
