//! Against a running NATS (`devenv up`). Run with `cargo test -p vs-infra -- --ignored`.

use std::time::Duration;

use futures::StreamExt;
use vs_core::{JobSpec, LanguageTag, Priority, Profile, RunId, Task, TenantId};
use vs_infra::queue::{Lane, NatsBus, Role, TaskBus, TASKS_STREAM};

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
        Task::Ingest { job_id, .. } | Task::Page { job_id, .. } | Task::Doc { job_id, .. } => {
            *job_id
        }
    }
}

/// Pulls until our job's task arrives; other tasks (left over from earlier runs) are acked.
async fn next_for(
    s: &mut futures::stream::BoxStream<'static, Box<dyn vs_infra::queue::Delivery>>,
    job: uuid::Uuid,
) -> Box<dyn vs_infra::queue::Delivery> {
    loop {
        let d = tokio::time::timeout(Duration::from_secs(10), s.next())
            .await
            .expect("timed out")
            .expect("stream ended");
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
    bus.publish(Role::Ingest, Lane::Interactive, &task, &dedup)
        .await
        .unwrap();
    bus.publish(Role::Ingest, Lane::Interactive, &task, &dedup)
        .await
        .unwrap(); // de-duplicated

    let mut s = bus
        .subscribe(Role::Ingest, Lane::Interactive)
        .await
        .unwrap();
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
    assert!(
        extra.is_err()
            || extra
                .unwrap()
                .map(|d| job_of(d.task()) != job)
                .unwrap_or(true)
    );

    bus.dead_letter(Role::Ingest, &task, "test").await.unwrap();
}

#[tokio::test]
#[ignore = "needs NATS from `devenv up`"]
async fn exhausted_task_is_reported_and_removed() {
    use vs_infra::queue::MAX_DELIVER;
    let bus = NatsBus::connect(&nats_url()).await.unwrap();
    bus.ensure_topology().await.unwrap();

    let job = uuid::Uuid::now_v7();
    let task = ingest_task(job);
    bus.publish(Role::Ingest, Lane::Batch, &task, &format!("{job}:ingest"))
        .await
        .unwrap();

    // Never ack: nak without delay until deliveries run out.
    let mut s = bus.subscribe(Role::Ingest, Lane::Batch).await.unwrap();
    for attempt in 1..=MAX_DELIVER {
        let d = next_for(&mut s, job).await;
        assert_eq!(d.attempt(), attempt);
        d.nak(Duration::from_millis(10)).await.unwrap();
    }

    // JetStream notices the exhausted message on its next delivery attempt, which needs
    // an outstanding pull (workers always have one); keep pulling and record whether our
    // task is ever delivered again.
    let redelivered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = redelivered.clone();
    let drain = tokio::spawn(async move {
        while let Some(d) = s.next().await {
            if job_of(d.task()) == job {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            let _ = d.ack().await;
        }
    });

    let mut ex = bus.subscribe_exhausted(Role::Ingest).await.unwrap();
    let e = loop {
        let e = tokio::time::timeout(Duration::from_secs(10), ex.next())
            .await
            .expect("no advisory")
            .unwrap();
        if e.task().map(job_of) == Some(job) {
            break e;
        }
        e.resolve().await.unwrap(); // leftovers from earlier runs
    };
    assert_eq!(e.task(), Some(&task));
    e.resolve().await.unwrap();

    tokio::time::sleep(Duration::from_millis(300)).await;
    drain.abort();
    assert!(
        !redelivered.load(std::sync::atomic::Ordering::SeqCst),
        "exhausted task was delivered again"
    );
}

#[tokio::test]
#[ignore = "needs NATS from `devenv up`"]
async fn consumers_use_ack_wait_not_backoff() {
    let bus = NatsBus::connect(&nats_url()).await.unwrap();
    bus.ensure_topology().await.unwrap();
    let stream = bus.js.get_stream(TASKS_STREAM).await.unwrap();
    for role in Role::ALL {
        for lane in Lane::ALL {
            let mut c: async_nats::jetstream::consumer::PullConsumer = stream
                .get_consumer(&vs_infra::queue::consumer_name(role, lane))
                .await
                .unwrap();
            let info = c.info().await.unwrap();
            assert!(info.config.backoff.is_empty(), "{}: backoff set", info.name);
            assert_eq!(info.config.ack_wait, role.ack_wait());
        }
    }
}
