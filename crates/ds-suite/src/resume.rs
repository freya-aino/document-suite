//! The resume sweep: what replaced redelivery.
//!
//! Removing the task bus (D13) took the redelivery protocol with it, and left one real
//! gap behind: a copy that died abandoned the documents it was carrying, and nothing
//! picked them up — their job rows stayed mid-flight until the caller noticed and
//! re-POSTed the upload. This is the piece that closes it without bringing a queue back.
//!
//! It is a lease, a sweep and a bound:
//!
//! - a **lease** on each job, taken by the process working on it and renewed while it
//!   works (`ds_worker::Pipeline::run`). A lease that stops being renewed is what a
//!   crash looks like from the outside, and `FOR UPDATE SKIP LOCKED` on the claim is what
//!   keeps two sweeps from taking the same job;
//! - a **sweep**, here, which claims abandoned jobs and hands them back to the pipeline.
//!   Re-running a document resumes rather than repeats: rendered pages are skipped when
//!   their signals are present and every vault write is idempotent (§5.4), so a document
//!   that died in Stage 2 does not re-render 300 pages;
//! - a **bound** (`worker.max_resumes`), because a document that kills the process
//!   reading it would otherwise take down the fleet one copy at a time. Past it the job
//!   is failed with a reason the caller can read.
//!
//! What this does *not* restore is spreading one document's pages across machines. A
//! resumed document is processed by one copy, like every other.

use std::sync::Arc;
use std::time::Duration;

use ds_api::Dispatch;
use ds_vault::ops::Ops;

/// How the sweep runs.
pub struct ResumeConfig {
    /// This process's lease holder id, the same one the pipeline claims with.
    pub holder: String,
    pub lease_ttl: Duration,
    /// Time between sweeps. Zero disables them.
    pub interval: Duration,
    /// Jobs claimed per sweep.
    pub batch: i64,
    pub max_resumes: i32,
    /// Documents this copy runs at once (`worker.max_documents`). The sweep claims
    /// nothing while the copy is already full: a claim it cannot start is a lease held
    /// over a document nobody is working on, which is worse than leaving it for a copy
    /// that has room.
    pub max_documents: u64,
}

/// Claims abandoned documents and hands them to the pipeline, for as long as the process
/// lives.
///
/// Runs in every copy. A copy that comes up after a fleet-wide outage therefore starts
/// recovering immediately, and `batch` is what stops it taking every orphaned document
/// in one go.
pub fn spawn(ops: Arc<Ops>, dispatch: Arc<dyn Dispatch>, cfg: ResumeConfig) {
    if cfg.interval.is_zero() {
        tracing::warn!("worker.resume_interval_s = 0: nothing will pick up documents abandoned by a crashed copy");
        return;
    }
    tokio::spawn(async move {
        // Deliberately not on the first tick: a copy that has just come up is competing
        // with however many others are also coming up, and a few seconds of settling
        // means the first sweep claims jobs whose leases have genuinely lapsed rather
        // than ones a restarting copy is about to renew.
        tokio::time::sleep(cfg.interval.min(Duration::from_secs(15))).await;
        let mut tick = tokio::time::interval(cfg.interval);
        loop {
            sweep_once(&ops, dispatch.as_ref(), &cfg).await;
            tick.tick().await;
        }
    });
}

/// One sweep. Separated out so a test can run exactly one.
pub async fn sweep_once(ops: &Ops, dispatch: &dyn Dispatch, cfg: &ResumeConfig) -> usize {
    let in_flight = dispatch.in_flight();
    if in_flight >= cfg.max_documents {
        tracing::debug!(in_flight, "copy is full; claiming no abandoned documents this sweep");
        return 0;
    }
    // First the ones that cannot be finished, so they stop being claimed and the caller
    // is told, rather than being retried until the end of time.
    match ops.fail_exhausted_jobs(cfg.max_resumes).await {
        Ok(0) => {}
        Ok(n) => {
            metrics::counter!("suite_jobs_abandoned_total").increment(n);
            tracing::warn!(jobs = n, max_resumes = cfg.max_resumes, "failed jobs that could not be finished");
        }
        Err(e) => tracing::warn!(error = %e, "could not fail exhausted jobs"),
    }

    // Never more than the room that is actually free.
    let room = (cfg.max_documents - in_flight).min(cfg.batch.max(0) as u64) as i64;
    let claimed = match ops.claim_abandoned_jobs(&cfg.holder, room, cfg.lease_ttl, cfg.max_resumes).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "resume sweep could not claim jobs");
            return 0;
        }
    };
    let mut resumed = 0;
    for job in claimed {
        tracing::info!(job_id = %job.job_id, resumes = job.resumes, tenant = job.spec.tenant.as_str(), "resuming an abandoned document");
        match dispatch.dispatch(job.job_id, job.run_id, job.spec).await {
            Ok(()) => {
                resumed += 1;
                metrics::counter!("suite_jobs_resumed_total").increment(1);
            }
            Err(e) => {
                // This copy is full. The lease it just took is released so another copy
                // — or this one, next sweep — can have the job, rather than holding it
                // for a TTL while nothing works on it.
                tracing::warn!(job_id = %job.job_id, error = %e, "could not resume; releasing the lease");
                if let Err(e) = ops.release_lease(job.job_id, &cfg.holder).await {
                    tracing::warn!(job_id = %job.job_id, error = %e, "could not release the lease of a job this copy declined");
                }
            }
        }
    }
    resumed
}
