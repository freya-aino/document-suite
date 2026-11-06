//! Against a running NATS (`devenv up`). Run with `cargo test -p vs-infra -- --ignored`.

use std::time::Duration;

use futures::StreamExt;
use vs_core::{JobSpec, LanguageTag, Priority, Profile, RunId, Task, TenantId};
use vs_infra::queue::{Lane, NatsBus, Role, TaskBus};

fn nats_url() -> String {
    std::env::var("VS__BUS__NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into())
}

fn ingest_task(job_id: uuid::Uuid) -> Task {
    Task::Ingest {
        job_id,
        run_id: RunId::new(),
        spec: JobSpec {
            tenant: TenantId("it".into()),
            language: LanguageTag::parse("en").unwrap(),
            priority: Priority::Interactive,
            profile: Profile::Balanced,
            dag_list_version: None,
            source_uri: "s3://visual-suite/originals/it/x".into(),
            external_ref: None,
        },
    }
}

fn job_of(t: &Task) -> uuid::Uuid {
    match t {
        Task::Ingest { job_id, .. } | Task::Page { job_id, .. } | Task::Doc { job_id, .. } => *job_id,
    }
}

/// Pulls until our job's task arrives; other tasks (left over from earlier runs) are acked.
async fn next_for(
    s: &mut futures::stream::BoxStream<'static, Box<dyn vs_infra::queue::Delivery>>,
    job: uuid::Uuid,
) -> Box<dyn vs_infra::queue::Delivery> {
    loop {
        let d = tokio::time::timeout(Duration::from_secs(10), s.next()).await.expect("timed out").expect("stream ended");
        if job_of(d.task()) == job {
            return d;
        }
        d.ack().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "needs NATS from `devenv up`"]
async fn publish_dedup_redeliver_ack() {
    let bus = NatsBus::connect(&nats_url()).await.unwrap();
    bus.ensure_topology().await.unwrap();
    bus.ensure_topology().await.unwrap(); // idempotent

    let job = uuid::Uuid::now_v7();
    let task = ingest_task(job);
    let dedup = format!("{job}:ingest");
    bus.publish(Role::Ingest, Lane::Interactive, &task, &dedup).await.unwrap();
    bus.publish(Role::Ingest, Lane::Interactive, &task, &dedup).await.unwrap(); // de-duplicated

    let mut s = bus.subscribe(Role::Ingest, Lane::Interactive).await.unwrap();
    let d = next_for(&mut s, job).await;
    assert_eq!(d.task(), &task);
    assert_eq!(d.attempt(), 1);
    d.in_progress().await.unwrap();
    d.nak(Duration::ZERO).await.unwrap();

    let d = next_for(&mut s, job).await;
    assert_eq!(d.attempt(), 2);
    d.ack().await.unwrap();

    // The duplicate publish must not show up.
    let extra = tokio::time::timeout(Duration::from_millis(500), s.next()).await;
    assert!(extra.is_err() || extra.unwrap().map(|d| job_of(d.task()) != job).unwrap_or(true));

    bus.dead_letter(Role::Ingest, &task, "test").await.unwrap();
}
