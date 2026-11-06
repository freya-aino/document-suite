//! Common loop (SPEC.md): pull from both lanes, run the role handler with heartbeats,
//! ack / retry / dead-letter, drain on shutdown.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tracing::Instrument;

use vs_core::Task;
use vs_infra::queue::{Delivery, Exhausted, Lane, Role, TaskBus, BACKOFF, MAX_DELIVER};

/// Handler failure. Transient errors are retried with backoff; permanent ones (bad
/// input, unsupported format) go straight to the dead-letter queue.
#[derive(Debug)]
pub struct TaskError {
    pub permanent: bool,
    pub error: anyhow::Error,
}

impl TaskError {
    pub fn permanent(error: impl Into<anyhow::Error>) -> Self {
        Self { permanent: true, error: error.into() }
    }
}

/// Anything `?`-converted is transient unless marked otherwise.
impl<E: Into<anyhow::Error>> From<E> for TaskError {
    fn from(e: E) -> Self {
        Self { permanent: false, error: e.into() }
    }
}

#[async_trait]
pub trait Handler: Send + Sync + 'static {
    async fn handle(&self, task: &Task) -> Result<(), TaskError>;
    /// Called once when a task is given up (permanent error or last attempt).
    async fn give_up(&self, task: &Task, error: &str);
}

pub struct LoopConfig {
    pub max_in_flight: usize,
    pub interactive_share: f32,
    pub shutdown_grace: Duration,
}

pub fn job_id(task: &Task) -> uuid::Uuid {
    match task {
        Task::Ingest { job_id, .. } | Task::Page { job_id, .. } | Task::Doc { job_id, .. } => *job_id,
    }
}

/// Runs until `shutdown` turns true, then waits up to `shutdown_grace` for in-flight
/// tasks. Unfinished tasks are redelivered by JetStream after `ack_wait`.
pub async fn run_loop(
    role: Role,
    bus: Arc<dyn TaskBus>,
    handler: Arc<dyn Handler>,
    cfg: LoopConfig,
    shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let max = cfg.max_in_flight.max(1);
    // Slots only the interactive lane may use; batch keeps at least one.
    let reserved = ((max as f32 * cfg.interactive_share).ceil() as usize).min(max.saturating_sub(1));
    let shared = Arc::new(Semaphore::new(max - reserved));
    let interactive_only = Arc::new(Semaphore::new(reserved));
    tracing::info!(role = role.as_str(), max, reserved, "worker loop starting");

    let mut pullers = Vec::new();
    for lane in Lane::ALL {
        let stream = bus.subscribe(role, lane).await?;
        let (bus, handler, shared, interactive_only, mut shutdown) =
            (bus.clone(), handler.clone(), shared.clone(), interactive_only.clone(), shutdown.clone());
        pullers.push(tokio::spawn(async move {
            let mut stream = stream;
            loop {
                let permit = tokio::select! {
                    p = acquire(lane, &shared, &interactive_only) => p,
                    _ = shutdown.changed() => break,
                };
                let delivery = tokio::select! {
                    d = stream.next() => match d { Some(d) => d, None => break },
                    _ = shutdown.changed() => break,
                };
                tokio::spawn(process(role, bus.clone(), handler.clone(), delivery, permit));
            }
        }));
    }
    // Tasks that ran out of deliveries without any handler result (crash or hang on
    // every attempt) still get the role's give-up path.
    {
        let mut exhausted = bus.subscribe_exhausted(role).await?;
        let (bus, handler, mut shutdown) = (bus.clone(), handler.clone(), shutdown.clone());
        pullers.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    e = exhausted.next() => match e {
                        Some(e) => handle_exhausted(role, bus.as_ref(), handler.as_ref(), e).await,
                        None => break,
                    },
                    _ = shutdown.changed() => break,
                }
            }
        }));
    }
    for p in pullers {
        p.await?;
    }

    // All permits back = all in-flight tasks finished.
    tracing::info!(role = role.as_str(), "draining in-flight tasks");
    let drained = async {
        let _a = shared.acquire_many((max - reserved) as u32).await;
        let _b = interactive_only.acquire_many(reserved as u32).await;
    };
    if tokio::time::timeout(cfg.shutdown_grace, drained).await.is_err() {
        tracing::warn!(role = role.as_str(), "shutdown grace exceeded; remaining tasks will be redelivered");
    }
    Ok(())
}

async fn handle_exhausted(role: Role, bus: &dyn TaskBus, handler: &dyn Handler, e: Box<dyn Exhausted>) {
    if let Some(task) = e.task().cloned() {
        let reason = format!("no result after {MAX_DELIVER} deliveries (worker crashed or hung on every attempt)");
        tracing::error!(role = role.as_str(), job_id = %job_id(&task), "{reason}");
        handler.give_up(&task, &reason).await;
        if let Err(dl) = bus.dead_letter(role, &task, &reason).await {
            tracing::error!(error = %dl, "dead-letter publish failed; retrying later");
            let _ = e.retry_later(Duration::from_secs(30)).await;
            return;
        }
    }
    if let Err(err) = e.resolve().await {
        tracing::error!(error = %err, "could not resolve exhausted task");
    }
}

async fn acquire(lane: Lane, shared: &Arc<Semaphore>, interactive_only: &Arc<Semaphore>) -> OwnedSemaphorePermit {
    let closed = "worker semaphores are never closed";
    match lane {
        Lane::Batch => shared.clone().acquire_owned().await.expect(closed),
        Lane::Interactive => tokio::select! {
            p = interactive_only.clone().acquire_owned() => p.expect(closed),
            p = shared.clone().acquire_owned() => p.expect(closed),
        },
    }
}

async fn process(role: Role, bus: Arc<dyn TaskBus>, handler: Arc<dyn Handler>, d: Box<dyn Delivery>, _permit: OwnedSemaphorePermit) {
    let task = d.task().clone();
    let attempt = d.attempt();
    let span = tracing::info_span!("task", role = role.as_str(), job_id = %job_id(&task), attempt);
    async move {
        let heartbeat = role.ack_wait() / 3;
        let result = {
            let work = handler.handle(&task);
            tokio::pin!(work);
            let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + heartbeat, heartbeat);
            loop {
                tokio::select! {
                    r = &mut work => break r,
                    _ = tick.tick() => {
                        if let Err(e) = d.in_progress().await {
                            tracing::warn!(error = %e, "heartbeat failed");
                        }
                    }
                }
            }
        };
        let outcome = match result {
            Ok(()) => d.ack().await,
            Err(e) if e.permanent || attempt >= MAX_DELIVER => {
                let reason = format!("{:#}", e.error);
                tracing::error!(permanent = e.permanent, error = %reason, "giving up on task");
                handler.give_up(&task, &reason).await;
                if let Err(dl) = bus.dead_letter(role, &task, &reason).await {
                    tracing::error!(error = %dl, "dead-letter publish failed");
                }
                d.term().await
            }
            Err(e) => {
                let delay = BACKOFF[(attempt as usize).saturating_sub(1).min(BACKOFF.len() - 1)];
                tracing::warn!(error = %format!("{:#}", e.error), ?delay, "task failed, retrying");
                d.nak(delay).await
            }
        };
        if let Err(e) = outcome {
            tracing::error!(error = %e, "ack/nak failed; JetStream will redeliver after ack_wait");
        }
    }
    .instrument(span)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::BoxStream;
    use std::sync::Mutex;
    use vs_infra::queue::QueueError;

    #[derive(Default)]
    struct Log(Mutex<Vec<String>>);
    impl Log {
        fn push(&self, s: impl Into<String>) {
            self.0.lock().unwrap().push(s.into());
        }
        fn all(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    struct FakeDelivery {
        task: Task,
        attempt: u32,
        log: Arc<Log>,
    }

    #[async_trait]
    impl Delivery for FakeDelivery {
        fn task(&self) -> &Task {
            &self.task
        }
        fn attempt(&self) -> u32 {
            self.attempt
        }
        async fn ack(self: Box<Self>) -> Result<(), QueueError> {
            self.log.push(format!("ack {}", self.attempt));
            Ok(())
        }
        async fn nak(self: Box<Self>, delay: Duration) -> Result<(), QueueError> {
            self.log.push(format!("nak {} {}s", self.attempt, delay.as_secs()));
            Ok(())
        }
        async fn term(self: Box<Self>) -> Result<(), QueueError> {
            self.log.push(format!("term {}", self.attempt));
            Ok(())
        }
        async fn in_progress(&self) -> Result<(), QueueError> {
            Ok(())
        }
    }

    struct FakeBus(Arc<Log>);

    #[async_trait]
    impl TaskBus for FakeBus {
        async fn publish(&self, _: Role, _: Lane, _: &Task, _: &str) -> Result<(), QueueError> {
            Ok(())
        }
        async fn subscribe(&self, _: Role, _: Lane) -> Result<BoxStream<'static, Box<dyn Delivery>>, QueueError> {
            Ok(futures::stream::empty().boxed())
        }
        async fn dead_letter(&self, _: Role, _: &Task, reason: &str) -> Result<(), QueueError> {
            self.0.push(format!("dlq {reason}"));
            Ok(())
        }
        async fn subscribe_exhausted(&self, _: Role) -> Result<BoxStream<'static, Box<dyn Exhausted>>, QueueError> {
            Ok(futures::stream::empty().boxed())
        }
    }

    struct FakeExhausted(Option<Task>, Arc<Log>);

    #[async_trait]
    impl Exhausted for FakeExhausted {
        fn task(&self) -> Option<&Task> {
            self.0.as_ref()
        }
        async fn resolve(self: Box<Self>) -> Result<(), QueueError> {
            self.1.push("resolve");
            Ok(())
        }
        async fn retry_later(self: Box<Self>, _: Duration) -> Result<(), QueueError> {
            self.1.push("retry_later");
            Ok(())
        }
    }

    /// Returns `f(attempt)`.
    struct Attempt(Arc<Log>, fn(u32) -> Result<(), TaskError>, u32);

    #[async_trait]
    impl Handler for Attempt {
        async fn handle(&self, _: &Task) -> Result<(), TaskError> {
            (self.1)(self.2)
        }
        async fn give_up(&self, _: &Task, error: &str) {
            self.0.push(format!("give_up {error}"));
        }
    }

    fn task() -> Task {
        Task::Doc {
            job_id: uuid::Uuid::nil(),
            run_id: vs_core::RunId::new(),
            document_hk: vs_core::HashKey::of(&["d"]),
            spec: serde_json::from_value(serde_json::json!({
                "tenant": "t", "language": "en", "source_uri": "k"
            }))
            .unwrap(),
        }
    }

    async fn run_attempt(f: fn(u32) -> Result<(), TaskError>, attempt: u32) -> Vec<String> {
        let log = Arc::new(Log::default());
        let d = Box::new(FakeDelivery { task: task(), attempt, log: log.clone() });
        let permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        process(Role::Doc, Arc::new(FakeBus(log.clone())), Arc::new(Attempt(log.clone(), f, attempt)), d, permit).await;
        log.all()
    }

    #[tokio::test]
    async fn ack_on_success() {
        assert_eq!(run_attempt(|_| Ok(()), 1).await, vec!["ack 1"]);
    }

    #[tokio::test]
    async fn transient_errors_back_off_then_dead_letter() {
        let fail = |_| Err(TaskError::from(anyhow::anyhow!("flaky")));
        assert_eq!(run_attempt(fail, 1).await, vec!["nak 1 10s"]);
        assert_eq!(run_attempt(fail, 3).await, vec!["nak 3 300s"]);
        assert_eq!(run_attempt(fail, MAX_DELIVER).await, vec!["give_up flaky", "dlq flaky", "term 4"]);
    }

    #[tokio::test]
    async fn exhausted_tasks_get_give_up_and_dead_letter() {
        let log = Arc::new(Log::default());
        let h = Attempt(log.clone(), |_| Ok(()), 4);
        let e = Box::new(FakeExhausted(Some(task()), log.clone()));
        handle_exhausted(Role::Doc, &FakeBus(log.clone()), &h, e).await;
        let all = log.all();
        assert!(all[0].starts_with("give_up no result after 4 deliveries"), "{all:?}");
        assert!(all[1].starts_with("dlq no result"));
        assert_eq!(all[2], "resolve");

        // Already gone: just acknowledge.
        let log = Arc::new(Log::default());
        handle_exhausted(Role::Doc, &FakeBus(log.clone()), &h, Box::new(FakeExhausted(None, log.clone()))).await;
        assert_eq!(log.all(), vec!["resolve"]);
    }

    #[tokio::test]
    async fn permanent_errors_dead_letter_immediately() {
        let fail = |_| Err(TaskError::permanent(anyhow::anyhow!("bad input")));
        assert_eq!(run_attempt(fail, 1).await, vec!["give_up bad input", "dlq bad input", "term 1"]);
    }
}
