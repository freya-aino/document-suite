use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use vs_core::{Priority, Task};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Ingest,
    Page,
    Doc,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Ingest => "ingest",
            Role::Page => "page",
            Role::Doc => "doc",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    Interactive,
    Batch,
}

impl From<Priority> for Lane {
    fn from(p: Priority) -> Self {
        match p {
            Priority::Interactive => Lane::Interactive,
            Priority::Batch => Lane::Batch,
        }
    }
}

impl Lane {
    pub fn as_str(&self) -> &'static str {
        match self {
            Lane::Interactive => "interactive",
            Lane::Batch => "batch",
        }
    }
}

pub fn subject(role: Role, lane: Lane) -> String {
    format!("vs.{}.{}", role.as_str(), lane.as_str())
}

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("bus connection: {0}")]
    Connection(String),
    #[error("publish failed: {0}")]
    Publish(String),
    #[error("bad message: {0}")]
    Decode(String),
}

/// One received task. Exactly one of `ack`, `nak`, `term` must be called.
#[async_trait]
pub trait Delivery: Send {
    fn task(&self) -> &Task;
    /// Number of times this message has been delivered (1 = first).
    fn attempt(&self) -> u32;
    async fn ack(self: Box<Self>) -> Result<(), QueueError>;
    async fn nak(self: Box<Self>, delay: Duration) -> Result<(), QueueError>;
    /// Give up on the message (after publishing it to the DLQ).
    async fn term(self: Box<Self>) -> Result<(), QueueError>;
    async fn in_progress(&self) -> Result<(), QueueError>;
}

#[async_trait]
pub trait TaskBus: Send + Sync {
    /// `dedup_id` becomes the message-id header (JetStream de-duplication).
    async fn publish(&self, role: Role, lane: Lane, task: &Task, dedup_id: &str) -> Result<(), QueueError>;
    async fn subscribe(&self, role: Role, lane: Lane) -> Result<BoxStream<'static, Box<dyn Delivery>>, QueueError>;
    async fn dead_letter(&self, role: Role, task: &Task, reason: &str) -> Result<(), QueueError>;
}

/// NATS JetStream implementation.
pub struct NatsBus {
    pub js: async_nats::jetstream::Context,
}

impl NatsBus {
    pub async fn connect(url: &str) -> Result<Self, QueueError> {
        let client = async_nats::connect(url).await.map_err(|e| QueueError::Connection(e.to_string()))?;
        Ok(Self { js: async_nats::jetstream::new(client) })
    }

    /// Creates or updates `VS_TASKS` / `VS_DLQ` streams and the durable consumers.
    pub async fn ensure_topology(&self) -> Result<(), QueueError> {
        todo!("create streams and consumers as described in SPEC.md")
    }
}

#[async_trait]
impl TaskBus for NatsBus {
    async fn publish(&self, role: Role, lane: Lane, task: &Task, dedup_id: &str) -> Result<(), QueueError> {
        let payload = serde_json::to_vec(task).map_err(|e| QueueError::Publish(e.to_string()))?;
        let mut headers = async_nats::HeaderMap::new();
        headers.insert("Nats-Msg-Id", dedup_id);
        self.js
            .publish_with_headers(subject(role, lane), headers, payload.into())
            .await
            .map_err(|e| QueueError::Publish(e.to_string()))?
            .await
            .map_err(|e| QueueError::Publish(e.to_string()))?;
        Ok(())
    }

    async fn subscribe(&self, _role: Role, _lane: Lane) -> Result<BoxStream<'static, Box<dyn Delivery>>, QueueError> {
        todo!("pull consumer w-<role>-<lane>; wrap messages in a Delivery impl")
    }

    async fn dead_letter(&self, role: Role, task: &Task, reason: &str) -> Result<(), QueueError> {
        let payload = serde_json::json!({ "reason": reason, "task": task });
        self.js
            .publish(format!("vs.dlq.{}", role.as_str()), payload.to_string().into())
            .await
            .map_err(|e| QueueError::Publish(e.to_string()))?;
        Ok(())
    }
}
