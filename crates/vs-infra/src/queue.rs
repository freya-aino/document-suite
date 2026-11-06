use async_nats::jetstream::{
    self,
    consumer::{self, pull},
    stream, AckKind,
};
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
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
    pub const ALL: [Role; 3] = [Role::Ingest, Role::Page, Role::Doc];

    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Ingest => "ingest",
            Role::Page => "page",
            Role::Doc => "doc",
        }
    }

    /// How long a delivery may stay unacknowledged. Handlers send `in_progress`
    /// every `ack_wait / 3` for longer work.
    pub fn ack_wait(&self) -> Duration {
        match self {
            Role::Ingest => Duration::from_secs(10 * 60),
            Role::Page => Duration::from_secs(5 * 60),
            Role::Doc => Duration::from_secs(30 * 60),
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
    pub const ALL: [Lane; 2] = [Lane::Interactive, Lane::Batch];

    pub fn as_str(&self) -> &'static str {
        match self {
            Lane::Interactive => "interactive",
            Lane::Batch => "batch",
        }
    }
}

pub const TASKS_STREAM: &str = "VS_TASKS";
pub const DLQ_STREAM: &str = "VS_DLQ";
/// Durable copy of JetStream's max-deliveries advisories for `VS_TASKS`.
pub const ADVISORY_STREAM: &str = "VS_ADVISORIES";
const MAX_DELIVERIES_ADVISORY: &str = "$JS.EVENT.ADVISORY.CONSUMER.MAX_DELIVERIES.VS_TASKS";
/// 1 + 3 retries.
pub const MAX_DELIVER: u32 = 4;
/// Redelivery delays after the 1st, 2nd and 3rd attempt, applied by the worker's nak.
/// Deliberately **not** set as the consumer's `backoff`: with a backoff list JetStream
/// uses it instead of `ack_wait` (a 10 s ack window on the first delivery, so long tasks
/// were redelivered while still running) and ignores nak delays.
pub const BACKOFF: [Duration; 3] = [Duration::from_secs(10), Duration::from_secs(60), Duration::from_secs(300)];

pub fn subject(role: Role, lane: Lane) -> String {
    format!("vs.{}.{}", role.as_str(), lane.as_str())
}

pub fn consumer_name(role: Role, lane: Lane) -> String {
    format!("w-{}-{}", role.as_str(), lane.as_str())
}

fn advisory_consumer_name(role: Role) -> String {
    format!("adv-{}", role.as_str())
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

/// A task JetStream stopped redelivering after `MAX_DELIVER` attempts without an ack,
/// nak or term: the worker crashed or hung on every attempt, so no handler ever ran its
/// give-up path. Exactly one of `resolve`, `retry_later` must be called.
#[async_trait]
pub trait Exhausted: Send {
    /// `None` if the task message is already gone (e.g. handled before).
    fn task(&self) -> Option<&Task>;
    /// Removes the task message from `VS_TASKS` and acknowledges the advisory.
    async fn resolve(self: Box<Self>) -> Result<(), QueueError>;
    async fn retry_later(self: Box<Self>, delay: Duration) -> Result<(), QueueError>;
}

#[async_trait]
pub trait TaskBus: Send + Sync {
    /// `dedup_id` becomes the message-id header (JetStream de-duplication).
    async fn publish(&self, role: Role, lane: Lane, task: &Task, dedup_id: &str) -> Result<(), QueueError>;
    async fn subscribe(&self, role: Role, lane: Lane) -> Result<BoxStream<'static, Box<dyn Delivery>>, QueueError>;
    async fn dead_letter(&self, role: Role, task: &Task, reason: &str) -> Result<(), QueueError>;
    /// Tasks of this role that ran out of deliveries.
    async fn subscribe_exhausted(&self, role: Role) -> Result<BoxStream<'static, Box<dyn Exhausted>>, QueueError>;
    /// Messages waiting for the consumer of (role, lane), not yet delivered (JetStream
    /// `num_pending`). Used for API back-pressure and autoscaling. Implementations
    /// without a notion of backlog report 0.
    async fn pending(&self, _role: Role, _lane: Lane) -> Result<u64, QueueError> {
        Ok(0)
    }
}

/// NATS JetStream implementation.
pub struct NatsBus {
    pub js: jetstream::Context,
    /// Messages fetched per pull request. Fetched messages count against `ack_wait`
    /// while they wait in the client, so this stays small; workers bound concurrency
    /// by pulling only when a slot is free.
    pub pull_batch: usize,
}

impl NatsBus {
    pub async fn connect(url: &str) -> Result<Self, QueueError> {
        let client = async_nats::connect(url).await.map_err(|e| QueueError::Connection(e.to_string()))?;
        Ok(Self { js: jetstream::new(client), pull_batch: 1 })
    }

    /// Creates or updates the `VS_TASKS` / `VS_DLQ` streams and the durable consumers.
    /// Idempotent; called by every worker and the API at startup.
    pub async fn ensure_topology(&self) -> Result<(), QueueError> {
        let conn = |e: &dyn std::fmt::Display| QueueError::Connection(e.to_string());
        let tasks = stream::Config {
            name: TASKS_STREAM.into(),
            subjects: vec!["vs.ingest.*".into(), "vs.page.*".into(), "vs.doc.*".into()],
            retention: stream::RetentionPolicy::WorkQueue,
            storage: stream::StorageType::File,
            duplicate_window: Duration::from_secs(120),
            ..Default::default()
        };
        let dlq = stream::Config {
            name: DLQ_STREAM.into(),
            subjects: vec!["vs.dlq.*".into()],
            retention: stream::RetentionPolicy::Limits,
            storage: stream::StorageType::File,
            max_age: Duration::from_secs(14 * 24 * 3600),
            ..Default::default()
        };
        // JetStream publishes max-deliveries advisories on core NATS only; capturing
        // them in a stream means none is lost while no worker of the role is running.
        let advisories = stream::Config {
            name: ADVISORY_STREAM.into(),
            subjects: vec![format!("{MAX_DELIVERIES_ADVISORY}.*")],
            retention: stream::RetentionPolicy::WorkQueue,
            storage: stream::StorageType::File,
            max_age: Duration::from_secs(14 * 24 * 3600),
            ..Default::default()
        };
        for cfg in [tasks, dlq, advisories] {
            // get_or_create does not apply changed settings to an existing stream.
            self.js.get_or_create_stream(cfg.clone()).await.map_err(|e| conn(&e))?;
            self.js.update_stream(cfg).await.map_err(|e| conn(&e))?;
        }

        let stream = self.js.get_stream(TASKS_STREAM).await.map_err(|e| conn(&e))?;
        for role in Role::ALL {
            for lane in Lane::ALL {
                let name = consumer_name(role, lane);
                // create_consumer creates or updates.
                stream
                    .create_consumer(pull::Config {
                        durable_name: Some(name),
                        filter_subject: subject(role, lane),
                        ack_policy: consumer::AckPolicy::Explicit,
                        ack_wait: role.ack_wait(),
                        max_deliver: MAX_DELIVER as i64,
                        ..Default::default()
                    })
                    .await
                    .map_err(|e| conn(&e))?;
            }
        }

        let adv = self.js.get_stream(ADVISORY_STREAM).await.map_err(|e| conn(&e))?;
        for role in Role::ALL {
            adv.create_consumer(pull::Config {
                durable_name: Some(advisory_consumer_name(role)),
                filter_subjects: Lane::ALL.iter().map(|l| format!("{MAX_DELIVERIES_ADVISORY}.{}", consumer_name(role, *l))).collect(),
                ack_policy: consumer::AckPolicy::Explicit,
                ack_wait: Duration::from_secs(60),
                ..Default::default()
            })
            .await
            .map_err(|e| conn(&e))?;
        }
        Ok(())
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

    async fn subscribe(&self, role: Role, lane: Lane) -> Result<BoxStream<'static, Box<dyn Delivery>>, QueueError> {
        let conn = |e: &dyn std::fmt::Display| QueueError::Connection(e.to_string());
        let stream = self.js.get_stream(TASKS_STREAM).await.map_err(|e| conn(&e))?;
        let consumer: consumer::PullConsumer = stream.get_consumer(&consumer_name(role, lane)).await.map_err(|e| conn(&e))?;
        let messages = consumer.stream().max_messages_per_batch(self.pull_batch).messages().await.map_err(|e| conn(&e))?;
        Ok(messages
            .filter_map(move |item| async move {
                let msg = match item {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(role = role.as_str(), lane = lane.as_str(), error = %e, "pull error");
                        return None;
                    }
                };
                match serde_json::from_slice::<Task>(&msg.payload) {
                    Ok(task) => {
                        let attempt = msg.info().map(|i| i.delivered.max(1) as u32).unwrap_or(1);
                        Some(Box::new(NatsDelivery { msg, task, attempt }) as Box<dyn Delivery>)
                    }
                    Err(e) => {
                        // Undecodable messages never become decodable: drop them for good.
                        tracing::error!(subject = %msg.subject, error = %e, "undecodable task, terminating");
                        let _ = msg.ack_with(AckKind::Term).await;
                        None
                    }
                }
            })
            .boxed())
    }

    async fn subscribe_exhausted(&self, role: Role) -> Result<BoxStream<'static, Box<dyn Exhausted>>, QueueError> {
        let conn = |e: &dyn std::fmt::Display| QueueError::Connection(e.to_string());
        let tasks = self.js.get_stream(TASKS_STREAM).await.map_err(|e| conn(&e))?;
        let adv = self.js.get_stream(ADVISORY_STREAM).await.map_err(|e| conn(&e))?;
        let consumer: consumer::PullConsumer = adv.get_consumer(&advisory_consumer_name(role)).await.map_err(|e| conn(&e))?;
        let messages = consumer.stream().max_messages_per_batch(1).messages().await.map_err(|e| conn(&e))?;
        Ok(messages
            .filter_map(move |item| {
                let tasks = tasks.clone();
                async move {
                    let advisory = item.map_err(|e| tracing::warn!(error = %e, "advisory pull error")).ok()?;
                    let Ok(a) = serde_json::from_slice::<MaxDeliveriesAdvisory>(&advisory.payload) else {
                        tracing::error!("undecodable max-deliveries advisory, dropping");
                        let _ = advisory.ack_with(AckKind::Term).await;
                        return None;
                    };
                    let task = match tasks.get_raw_message(a.stream_seq).await {
                        Ok(m) => serde_json::from_slice::<Task>(&m.payload).ok(),
                        Err(e) if matches!(e.kind(), stream::RawMessageErrorKind::NoMessageFound) => None,
                        Err(e) => {
                            tracing::warn!(seq = a.stream_seq, error = %e, "could not load exhausted task; retrying");
                            let _ = advisory.ack_with(AckKind::Nak(Some(Duration::from_secs(30)))).await;
                            return None;
                        }
                    };
                    Some(Box::new(NatsExhausted { advisory, tasks, seq: a.stream_seq, task }) as Box<dyn Exhausted>)
                }
            })
            .boxed())
    }

    async fn pending(&self, role: Role, lane: Lane) -> Result<u64, QueueError> {
        let conn = |e: &dyn std::fmt::Display| QueueError::Connection(e.to_string());
        let stream = self.js.get_stream(TASKS_STREAM).await.map_err(|e| conn(&e))?;
        let info = stream.consumer_info(consumer_name(role, lane)).await.map_err(|e| conn(&e))?;
        Ok(info.num_pending)
    }

    async fn dead_letter(&self, role: Role, task: &Task, reason: &str) -> Result<(), QueueError> {
        let payload = serde_json::json!({ "reason": reason, "task": task });
        self.js
            .publish(format!("vs.dlq.{}", role.as_str()), payload.to_string().into())
            .await
            .map_err(|e| QueueError::Publish(e.to_string()))?
            .await
            .map_err(|e| QueueError::Publish(e.to_string()))?;
        Ok(())
    }
}

/// Fields of `io.nats.jetstream.advisory.v1.max_deliver` that we use.
#[derive(Deserialize)]
struct MaxDeliveriesAdvisory {
    stream_seq: u64,
}

struct NatsExhausted {
    advisory: jetstream::Message,
    tasks: stream::Stream,
    seq: u64,
    task: Option<Task>,
}

#[async_trait]
impl Exhausted for NatsExhausted {
    fn task(&self) -> Option<&Task> {
        self.task.as_ref()
    }

    async fn resolve(self: Box<Self>) -> Result<(), QueueError> {
        if self.task.is_some() {
            self.tasks.delete_message(self.seq).await.map_err(|e| QueueError::Publish(e.to_string()))?;
        }
        self.advisory.double_ack().await.map_err(|e| QueueError::Publish(e.to_string()))
    }

    async fn retry_later(self: Box<Self>, delay: Duration) -> Result<(), QueueError> {
        self.advisory.ack_with(AckKind::Nak(Some(delay))).await.map_err(|e| QueueError::Publish(e.to_string()))
    }
}

struct NatsDelivery {
    msg: jetstream::Message,
    task: Task,
    attempt: u32,
}

#[async_trait]
impl Delivery for NatsDelivery {
    fn task(&self) -> &Task {
        &self.task
    }

    fn attempt(&self) -> u32 {
        self.attempt
    }

    async fn ack(self: Box<Self>) -> Result<(), QueueError> {
        // Double ack waits for the server's confirmation, so a crash right after
        // acking cannot lead to a silent redelivery of finished work.
        self.msg.double_ack().await.map_err(|e| QueueError::Publish(e.to_string()))
    }

    async fn nak(self: Box<Self>, delay: Duration) -> Result<(), QueueError> {
        self.msg.ack_with(AckKind::Nak(Some(delay))).await.map_err(|e| QueueError::Publish(e.to_string()))
    }

    async fn term(self: Box<Self>) -> Result<(), QueueError> {
        self.msg.ack_with(AckKind::Term).await.map_err(|e| QueueError::Publish(e.to_string()))
    }

    async fn in_progress(&self) -> Result<(), QueueError> {
        self.msg.ack_with(AckKind::Progress).await.map_err(|e| QueueError::Publish(e.to_string()))
    }
}
