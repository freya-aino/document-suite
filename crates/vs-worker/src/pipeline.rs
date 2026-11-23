//! One document, one process, three stages.
//!
//! A task bus used to sit between these stages: ingest published one message per page,
//! any `page` worker in the fleet drained them, and the last one published a `doc`
//! message. That bought one thing — spreading a single document's pages across machines —
//! and cost a service to run, three deployable roles, a redelivery and dead-letter
//! protocol, lane fairness, and de-duplication ids. The per-page DAG it fed was always
//! executed in-process anyway (`vs-dag`), so the work itself never needed a queue.
//!
//! Now the stages are function calls. Pages still run concurrently — `buffer_unordered`
//! over this process's page slots, with the same reservation for interactive-priority
//! work that the two lanes used to provide — but they run concurrently *here*, bounded by
//! this process and by the model tier it shares with every other copy. Since a page is
//! mostly spent waiting on an HTTP call to that tier, one process saturates it much as a
//! fleet of page workers did.
//!
//! What is genuinely gone: a single large document can no longer be spread over several
//! copies, and nothing redelivers work after a crash. Recovery is the caller re-POSTing
//! the upload, which resumes rather than repeats — see [`crate::TaskError`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use vs_core::{HashKey, JobSpec, JobState, Priority, RunId};
use vs_vault::ops::{Ops, PageOutcome};

use crate::doc::DocHandler;
use crate::ingest::IngestHandler;
use crate::page::{PageHandler, PageTask};
use crate::TaskError;

/// One page's inputs, as the ingest stage left them in the object store.
#[derive(Debug, Clone)]
pub struct PageRef {
    pub page_no: u32,
    pub page_image_uri: String,
    pub text_layer_uri: Option<String>,
}

/// Which stage a span or a metric belongs to. This is what the `role` label meant when
/// the three stages were three processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Ingest,
    Page,
    Doc,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Ingest => "ingest",
            Stage::Page => "page",
            Stage::Doc => "doc",
        }
    }
}

pub struct PipelineConfig {
    /// Pages this process works on at once, across every document it is running.
    pub max_pages_in_flight: usize,
    /// Share of those slots only interactive-priority documents may take, so a large
    /// batch cannot starve a small upload. What the two queue lanes used to do.
    pub interactive_share: f32,
    /// Attempts per page before it is recorded failed. 1 disables retrying.
    pub page_attempts: u32,
    /// Who this process says it is when it claims a document. One string per process —
    /// `vs_worker::holder_id` builds one — because two copies must never look like the
    /// same lease holder.
    pub holder: String,
    /// How long a claim is good for before another copy may take the document.
    pub lease_ttl: Duration,
}

/// The three stages, and the page slots they share.
pub struct Pipeline {
    ingest: IngestHandler,
    page: PageHandler,
    doc: DocHandler,
    ops: Arc<Ops>,
    cfg: PipelineConfig,
    shared: Arc<Semaphore>,
    interactive_only: Arc<Semaphore>,
}

impl Pipeline {
    pub fn new(ingest: IngestHandler, page: PageHandler, doc: DocHandler, ops: Arc<Ops>, cfg: PipelineConfig) -> Self {
        let max = cfg.max_pages_in_flight.max(1);
        // Slots only the interactive share may use; batch work keeps at least one.
        let reserved = ((max as f32 * cfg.interactive_share).ceil() as usize).min(max.saturating_sub(1));
        Self {
            ingest,
            page,
            doc,
            ops,
            cfg,
            shared: Arc::new(Semaphore::new(max - reserved)),
            interactive_only: Arc::new(Semaphore::new(reserved)),
        }
    }

    /// Takes one document from its upload to both stores.
    ///
    /// On failure the job row is left `failed` with the reason, because nothing else will
    /// pick it up: this process is the only thing that knows about the document.
    pub async fn run(&self, job_id: uuid::Uuid, run_id: RunId, spec: &JobSpec) -> Result<(), TaskError> {
        // Claim the document before any work on it. This is what stops two copies from
        // processing the same document after a resume sweep: the writes are idempotent
        // either way (§5.4), but the model calls are the expensive part and they are not.
        if !self.ops.take_lease(job_id, &self.cfg.holder, self.cfg.lease_ttl).await? {
            tracing::info!(%job_id, "another copy holds this document; leaving it alone");
            metrics::counter!("worker_lease_contended_total").increment(1);
            return Ok(());
        }
        // Renewed for as long as this document is being worked on, so the lease lapses
        // only when the process stops — which is what a crash looks like from outside.
        let _lease = Lease::renewing(self.ops.clone(), job_id, self.cfg.holder.clone(), self.cfg.lease_ttl);
        match self.stages(job_id, run_id, spec).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let reason = format!("{:#}", e.error);
                if let Err(set) = self.ops.set_state(job_id, JobState::Failed, Some(&reason)).await {
                    tracing::error!(%job_id, error = %set, "could not mark job failed");
                }
                Err(e)
            }
        }
    }

    async fn stages(&self, job_id: uuid::Uuid, run_id: RunId, spec: &JobSpec) -> Result<(), TaskError> {
        let started = Instant::now();
        let ingested = self.timed(Stage::Ingest, self.ingest.ingest(job_id, run_id, spec)).await?;
        let crate::ingest::Ingested { document_hk, pages, doc_type } = ingested;
        tracing::info!(%job_id, %document_hk, pages = pages.len(), doc_type = ?doc_type.map(|d| d.doc_type), "ingested; running pages");

        // Pages are independent, and a failed one is recorded rather than propagated: the
        // doc stage decides whether too many failed (`worker.max_failed_page_fraction`)
        // or whether the document is merely `partial`.
        let slots = self.cfg.max_pages_in_flight.max(1);
        futures::stream::iter(pages)
            .map(|page| self.one_page(job_id, run_id, &document_hk, page, spec, doc_type.map(|d| d.doc_type)))
            .buffer_unordered(slots)
            .collect::<Vec<()>>()
            .await;

        self.timed(Stage::Doc, self.doc.process(job_id, run_id, &document_hk, spec)).await?;
        tracing::info!(%job_id, %document_hk, elapsed_s = started.elapsed().as_secs_f64(), "document finished");
        Ok(())
    }

    /// One page, retried while the error is transient.
    async fn one_page(
        &self,
        job_id: uuid::Uuid,
        run_id: RunId,
        document_hk: &HashKey,
        page: PageRef,
        spec: &JobSpec,
        doc_type: Option<vs_core::DocType>,
    ) {
        let _permit = acquire(spec.priority, &self.shared, &self.interactive_only).await;
        let task = PageTask {
            job_id,
            run_id,
            document_hk,
            page_no: page.page_no,
            page_image_uri: &page.page_image_uri,
            text_layer_uri: page.text_layer_uri.as_deref(),
            spec,
            // Settled by the ingest stage, which is the one place that sees the whole
            // document before the pages fan out.
            doc_type,
        };

        // Borrowed, not moved: the closure is `FnMut` and may run more than once.
        let task = &task;
        let outcome = retrying(self.cfg.page_attempts, |attempt| async move {
            let started = Instant::now();
            in_flight(Stage::Page, 1.0);
            let result = self.page.process(task).await;
            in_flight(Stage::Page, -1.0);
            record(Stage::Page, if result.is_ok() { "ok" } else { "error" }, started);
            if let Err(e) = &result {
                tracing::warn!(%job_id, page_no = page.page_no, attempt, permanent = e.permanent, error = %format!("{:#}", e.error), "page attempt failed");
            }
            result
        })
        .await;

        if let Err(e) = outcome {
            let reason = format!("{:#}", e.error);
            tracing::error!(%job_id, page_no = page.page_no, permanent = e.permanent, error = %reason, "page failed");
            // Recorded, not propagated: the document can still finish without this page,
            // and the doc stage decides whether too many are missing.
            if let Err(set) = self.ops.page_finished(job_id, page.page_no, PageOutcome::Failed, None, None, Some(&reason)).await {
                tracing::error!(%job_id, page_no = page.page_no, error = %set, "could not mark page failed");
            }
        }
    }

    /// Times and counts a stage, under a span carrying the job.
    async fn timed<T>(&self, stage: Stage, work: impl std::future::Future<Output = Result<T, TaskError>>) -> Result<T, TaskError> {
        let started = Instant::now();
        in_flight(stage, 1.0);
        let result = work.await;
        in_flight(stage, -1.0);
        record(stage, if result.is_ok() { "ok" } else { "error" }, started);
        result
    }
}

/// Holds a document's lease, renewing it until dropped.
///
/// A guard rather than a pair of calls because every exit from the document — finished,
/// failed, or a panic in a stage — has to release it. Dropping it aborts the renewal and
/// releases the lease, so the next sweep can pick the document up immediately instead of
/// waiting out the TTL.
struct Lease {
    renewer: tokio::task::JoinHandle<()>,
    ops: Arc<Ops>,
    job_id: uuid::Uuid,
    holder: String,
}

impl Lease {
    fn renewing(ops: Arc<Ops>, job_id: uuid::Uuid, holder: String, ttl: Duration) -> Self {
        // A third of the TTL: two renewals may be lost — a slow query, a paused
        // scheduler — before anything else considers the document abandoned.
        let period = (ttl / 3).max(Duration::from_secs(1));
        let renewer = tokio::spawn({
            let (ops, holder) = (ops.clone(), holder.clone());
            async move {
                let mut tick = tokio::time::interval(period);
                tick.tick().await;
                loop {
                    tick.tick().await;
                    match ops.renew_lease(job_id, &holder, ttl).await {
                        Ok(true) => {}
                        // Taken by another copy: this one's writes are still idempotent,
                        // so the work is not wrong, only duplicated. Worth a warning
                        // because it means a lease expired while work was in progress.
                        Ok(false) => {
                            tracing::warn!(%job_id, "lost the lease on a document still being processed");
                            return;
                        }
                        Err(e) => tracing::warn!(%job_id, error = %e, "could not renew the document lease"),
                    }
                }
            }
        });
        Self { renewer, ops, job_id, holder }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.renewer.abort();
        // Fire-and-forget: `Drop` cannot await, and a release that fails only means the
        // lease expires on its own.
        let (ops, job_id, holder) = (self.ops.clone(), self.job_id, self.holder.clone());
        tokio::spawn(async move {
            if let Err(e) = ops.release_lease(job_id, &holder).await {
                tracing::debug!(%job_id, error = %e, "could not release the document lease; it will expire");
            }
        });
    }
}

/// Runs `work`, retrying while it fails *transiently* and attempts remain.
///
/// This is what the queue's `max_deliver` and its `[10s, 60s, 300s]` backoff used to do,
/// shrunk to the scale of a process: the delays are seconds rather than minutes because
/// nothing is waiting on a redelivery, and a permanent error — bad page data, an
/// unsupported format — is not retried at all.
async fn retrying<F, Fut>(attempts: u32, mut work: F) -> Result<(), TaskError>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<(), TaskError>>,
{
    let attempts = attempts.max(1);
    let mut last = None;
    for attempt in 1..=attempts {
        match work(attempt).await {
            Ok(()) => return Ok(()),
            Err(e) if !e.permanent && attempt < attempts => {
                tokio::time::sleep(backoff(attempt)).await;
                last = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.expect("the loop only exits here after a failure"))
}

/// 2 s, 4 s, 8 s, … capped at 16 s.
fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1 << attempt.min(4))
}

/// An interactive page may take a reserved slot or a shared one, whichever frees first; a
/// batch page may only take a shared one.
async fn acquire(priority: Priority, shared: &Arc<Semaphore>, interactive_only: &Arc<Semaphore>) -> OwnedSemaphorePermit {
    let shared = shared.clone();
    match priority {
        Priority::Batch => shared.acquire_owned().await.expect("page semaphore is never closed"),
        Priority::Interactive => {
            let reserved = interactive_only.clone();
            tokio::select! {
                p = shared.acquire_owned() => p.expect("page semaphore is never closed"),
                p = reserved.acquire_owned() => p.expect("page semaphore is never closed"),
            }
        }
    }
}

fn record(stage: Stage, outcome: &'static str, started: Instant) {
    metrics::counter!("worker_stage_total", "stage" => stage.as_str(), "outcome" => outcome).increment(1);
    metrics::histogram!("worker_stage_duration_seconds", "stage" => stage.as_str()).record(started.elapsed().as_secs_f64());
}

fn in_flight(stage: Stage, delta: f64) {
    let g = metrics::gauge!("worker_stage_in_flight", "stage" => stage.as_str());
    if delta > 0.0 {
        g.increment(delta);
    } else {
        g.decrement(-delta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn transient() -> TaskError {
        TaskError::from(anyhow::anyhow!("model endpoint busy"))
    }

    #[tokio::test(start_paused = true)]
    async fn a_transient_failure_is_retried_and_can_then_succeed() {
        let calls = AtomicU32::new(0);
        let result = retrying(3, |_| async {
            if calls.fetch_add(1, Ordering::Relaxed) < 2 {
                Err(transient())
            } else {
                Ok(())
            }
        })
        .await;
        assert!(result.is_ok(), "the third attempt succeeded and should have counted");
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn attempts_are_bounded_and_the_last_error_is_returned() {
        let calls = AtomicU32::new(0);
        let err = retrying(3, |_| async {
            calls.fetch_add(1, Ordering::Relaxed);
            Err(transient())
        })
        .await
        .expect_err("it never succeeds");
        assert_eq!(calls.load(Ordering::Relaxed), 3, "a bounded number of attempts, not a loop");
        assert!(!err.permanent);
    }

    #[tokio::test(start_paused = true)]
    async fn a_permanent_failure_is_not_retried() {
        // Bad page data does not become good on a second read, and retrying it only
        // delays the document.
        let calls = AtomicU32::new(0);
        let err = retrying(5, |_| async {
            calls.fetch_add(1, Ordering::Relaxed);
            Err(TaskError::permanent(anyhow::anyhow!("unsupported format")))
        })
        .await
        .expect_err("permanent");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(err.permanent);
    }

    #[tokio::test(start_paused = true)]
    async fn one_attempt_means_no_retrying() {
        let calls = AtomicU32::new(0);
        let _ = retrying(1, |_| async {
            calls.fetch_add(1, Ordering::Relaxed);
            Err(transient())
        })
        .await;
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn batch_work_cannot_take_the_slots_reserved_for_interactive() {
        // What the two queue lanes did: a batch of a thousand documents must not be able
        // to fill every slot and leave a single interactive upload waiting behind it.
        let shared = Arc::new(Semaphore::new(1));
        let reserved = Arc::new(Semaphore::new(1));
        let batch = acquire(Priority::Batch, &shared, &reserved).await;
        // The only shared slot is taken, so a second batch page must wait...
        assert!(shared.try_acquire().is_err());
        // ...while an interactive page still gets in, on the reserved one.
        let interactive = acquire(Priority::Interactive, &shared, &reserved).await;
        drop((batch, interactive));
    }
}
