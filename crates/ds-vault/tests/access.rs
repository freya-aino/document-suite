//! Leases, resume claims, revocation and erasure, against Postgres from
//! `devenv --profile stack up` (`DATABASE_URL`; the role needs CREATEDB, granted by
//! devenv). Each test gets a fresh migrated database. Run with
//! `cargo test -p ds-vault -- --ignored`.
//!
//! The access *rule* is unit-tested in `src/access.rs`, over values. What needs a
//! database is everything the rule rests on: that a claim is exclusive, that a lapsed
//! lease is reclaimable, that a resumed job is bounded, and that erasure reaches every
//! row it says it does.

use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

use ds_core::{HashKey, JobSpec, JobState, LanguageTag, Owner, Priority, Profile, RunId, TenantId};
use ds_vault::access::{GranteeKind, Reader, RevocationKind, Visibility};
use ds_vault::ops::{NewJob, Ops};

const HOLDER_A: &str = "host-a/1/aaaaaaaa";
const HOLDER_B: &str = "host-b/2/bbbbbbbb";

fn tenant() -> TenantId {
    TenantId("t1".into())
}

fn spec(owner: Option<Owner>) -> JobSpec {
    JobSpec {
        tenant: tenant(),
        language: LanguageTag::parse("en").expect("language"),
        priority: Priority::Batch,
        profile: Profile::Balanced,
        dag_list_version: None,
        source_uri: "originals/t1/abc".into(),
        external_ref: None,
        doc_type: None,
        owner,
    }
}

fn owner(subject: &str) -> Owner {
    Owner { subject: subject.into(), department: Some("research".into()), workspaces: vec!["red".into()] }
}

async fn job(ops: &Ops, owner: Option<Owner>) -> Uuid {
    let job_id = Uuid::now_v7();
    ops.create_jobs(None, &[NewJob { job_id, run_id: RunId::new(), spec: spec(owner), document_hk: None }]).await.expect("create job");
    job_id
}

fn reader(subject: &str, department: Option<&str>, workspaces: &[&str]) -> Reader {
    Reader {
        subject: subject.into(),
        department: department.map(str::to_string),
        workspaces: workspaces.iter().map(|w| w.to_string()).collect(),
        admin: false,
    }
}

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn a_lease_is_exclusive_until_it_lapses(pool: PgPool) {
    let ops = Ops { pool: pool.clone() };
    let job_id = job(&ops, None).await;
    let ttl = Duration::from_secs(60);

    assert!(ops.take_lease(job_id, HOLDER_A, ttl).await.expect("take"), "an unclaimed job");
    assert!(!ops.take_lease(job_id, HOLDER_B, ttl).await.expect("take"), "another copy took a claimed job");
    // Re-entrant for the holder: the resume sweep claims a job and then the pipeline
    // claims it again on the way in, with the same holder id.
    assert!(ops.take_lease(job_id, HOLDER_A, ttl).await.expect("take"), "the holder cannot re-take its own lease");

    assert!(ops.renew_lease(job_id, HOLDER_A, ttl).await.expect("renew"));
    assert!(!ops.renew_lease(job_id, HOLDER_B, ttl).await.expect("renew"), "a copy renewed a lease it does not hold");

    // What a crash looks like: the holder stops renewing, so the lease lapses.
    sqlx::query("UPDATE ops.jobs SET lease_expires_at = now() - interval '1 second' WHERE job_id = $1")
        .bind(job_id)
        .execute(&pool)
        .await
        .expect("expire");
    assert!(ops.take_lease(job_id, HOLDER_B, ttl).await.expect("take"), "a lapsed lease must be reclaimable");

    // Released rather than left to expire, so the next sweep can have it at once.
    ops.release_lease(job_id, HOLDER_B).await.expect("release");
    assert!(ops.take_lease(job_id, HOLDER_A, ttl).await.expect("take"));
}

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn an_abandoned_job_is_claimed_once_and_a_finished_one_never(pool: PgPool) {
    let ops = Ops { pool: pool.clone() };
    let abandoned = job(&ops, None).await;
    let finished = job(&ops, None).await;
    let held = job(&ops, None).await;
    ops.set_state(finished, JobState::Done, None).await.expect("done");
    assert!(ops.take_lease(held, HOLDER_B, Duration::from_secs(600)).await.expect("take"), "held by another copy");

    let ttl = Duration::from_secs(60);
    let claimed = ops.claim_abandoned_jobs(HOLDER_A, 10, ttl, 3).await.expect("claim");
    let ids: Vec<Uuid> = claimed.iter().map(|j| j.job_id).collect();
    assert_eq!(ids, vec![abandoned], "claimed {ids:?}: a finished or leased job is not abandoned");
    assert_eq!(claimed[0].resumes, 1, "a claim counts as a resume, or the bound could never be reached");
    assert_eq!(claimed[0].spec.source_uri, "originals/t1/abc", "the spec must come back whole; it is what the document is re-run from");

    // Claimed once: the lease the claim took is what keeps the next sweep off it.
    assert!(ops.claim_abandoned_jobs(HOLDER_B, 10, ttl, 3).await.expect("claim").is_empty(), "two copies claimed one job");
}

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn a_document_that_cannot_be_finished_stops_being_resumed(pool: PgPool) {
    // Without the bound, a document that kills the process reading it takes down the
    // fleet one copy at a time.
    let ops = Ops { pool: pool.clone() };
    let job_id = job(&ops, None).await;
    let ttl = Duration::from_secs(0); // every claim lapses immediately

    for attempt in 1..=3 {
        let claimed = ops.claim_abandoned_jobs(HOLDER_A, 10, ttl, 3).await.expect("claim");
        assert_eq!(claimed.len(), 1, "attempt {attempt} claimed nothing");
    }
    assert!(ops.claim_abandoned_jobs(HOLDER_A, 10, ttl, 3).await.expect("claim").is_empty(), "a fourth resume past max_resumes = 3");

    assert_eq!(ops.fail_exhausted_jobs(3).await.expect("fail"), 1);
    let (state, error): (String, Option<String>) =
        sqlx::query_as("SELECT state, error FROM ops.jobs WHERE job_id = $1").bind(job_id).fetch_one(&pool).await.expect("row");
    assert_eq!(state, "failed");
    assert!(error.expect("a reason").contains("re-upload"), "the caller is told what to do about it");
    // And it is not failed twice, nor claimed again now that it is final.
    assert_eq!(ops.fail_exhausted_jobs(3).await.expect("fail"), 0);
}

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn ownership_is_recorded_once_and_survives_a_second_run(pool: PgPool) {
    let ops = Ops { pool: pool.clone() };
    let hk = HashKey::document(&tenant(), &"a".repeat(64));

    assert!(ops.record_document_owner(&tenant(), &hk, Some(&owner("alice")), Visibility::Private).await.expect("record"));
    ops.set_document_visibility(&tenant(), &hk, Visibility::Workspace).await.expect("share");

    // Re-running a document is not a change of ownership, and must not undo a share.
    assert!(!ops.record_document_owner(&tenant(), &hk, Some(&owner("bob")), Visibility::Private).await.expect("record again"));
    let a = ops.document_access(&tenant(), &hk).await.expect("access").expect("row");
    assert_eq!(a.owner_subject.as_deref(), Some("alice"));
    assert_eq!(a.visibility, Visibility::Workspace);
    assert_eq!(a.workspaces, vec!["red"]);
}

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn the_listing_predicate_and_the_rule_agree(pool: PgPool) {
    // `readable_documents` narrows in SQL and decides in Rust, so the two can only
    // disagree by the SQL being *narrower* — a document that is readable and missing from
    // the listing. That is what this checks, case by case.
    let ops = Ops { pool: pool.clone() };
    let t = tenant();
    let cases = [
        (Visibility::Private, "private"),
        (Visibility::Department, "department"),
        (Visibility::Workspace, "workspace"),
        (Visibility::Organization, "organization"),
    ];
    let mut keys = Vec::new();
    for (i, (v, _)) in cases.iter().enumerate() {
        let hk = HashKey::document(&t, &format!("{i}").repeat(64));
        ops.record_document_owner(&t, &hk, Some(&owner("alice")), *v).await.expect("record");
        keys.push(hk);
    }

    let readers = [
        ("alice", reader("alice", None, &[]), 4),              // the owner reads all of their own
        ("colleague", reader("bob", Some("research"), &[]), 2), // department + organization
        ("workspace-mate", reader("bob", None, &["red"]), 2),   // workspace + organization
        ("stranger", reader("bob", None, &[]), 1),              // organization only
    ];
    for (who, r, expected) in readers {
        let listed = ops.readable_documents(&t, &r, 0, 100).await.expect("list");
        assert_eq!(listed.len(), expected, "{who} listed {:?}", listed.iter().map(|d| d.visibility).collect::<Vec<_>>());
        // The listing is a filter over the same rule, so everything in it must pass the
        // rule when asked directly, one document at a time.
        for d in &listed {
            let hk = ds_vault::hash_key_from_hex(&d.document_hk).expect("hash key");
            let access = ops.document_access(&t, &hk).await.expect("access");
            assert!(r.may_read(access.as_ref()), "{who} was listed a document the rule refuses");
        }
    }

    // A grant is the one case the SQL can admit that `may_read` cannot see without the
    // grants loaded, so it is the case most likely to diverge.
    ops.grant_document(&t, &keys[0], GranteeKind::Subject, "bob", Some("alice")).await.expect("grant");
    let listed = ops.readable_documents(&t, &reader("bob", None, &[]), 0, 100).await.expect("list");
    assert_eq!(listed.len(), 2, "a granted private document is missing from the listing");
    assert!(ops.revoke_document_grant(&t, &keys[0], GranteeKind::Subject, "bob").await.expect("revoke"));
    assert_eq!(ops.readable_documents(&t, &reader("bob", None, &[]), 0, 100).await.expect("list").len(), 1);

    // Pagination, because a listing without it is a listing that falls over on a real
    // corpus.
    assert_eq!(ops.readable_documents(&t, &reader("alice", None, &[]), 0, 2).await.expect("list").len(), 2);
    assert_eq!(ops.readable_documents(&t, &reader("alice", None, &[]), 1, 2).await.expect("list").len(), 2);
    assert!(ops.readable_documents(&t, &reader("alice", None, &[]), 2, 2).await.expect("list").is_empty());
}

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn a_revocation_lives_until_its_tokens_have_expired(pool: PgPool) {
    let ops = Ops { pool: pool.clone() };
    let now = chrono::Utc::now();
    ops.revoke_token(RevocationKind::Sid, "sess-1", Some(&tenant()), now, now + chrono::TimeDelta::hours(1), Some("logout"))
        .await
        .expect("revoke");
    // Already past: it matches nothing, so it must not be served to a verifier.
    ops.revoke_token(RevocationKind::Jti, "old", None, now - chrono::TimeDelta::hours(2), now - chrono::TimeDelta::hours(1), None)
        .await
        .expect("revoke");

    let live = ops.live_revocations().await.expect("live");
    assert_eq!(live.len(), 1, "an expired revocation was served: {live:?}");
    assert_eq!(live[0].id, "sess-1");

    // Revoking the same thing again moves the instant forward rather than failing.
    let later = now + chrono::TimeDelta::minutes(5);
    ops.revoke_token(RevocationKind::Sid, "sess-1", Some(&tenant()), later, later + chrono::TimeDelta::hours(1), None)
        .await
        .expect("re-revoke");
    let live = ops.live_revocations().await.expect("live");
    assert_eq!(live.len(), 1);
    // A microsecond of slack: `timestamptz` keeps microseconds and `DateTime<Utc>` keeps
    // nanoseconds, so a value that goes through Postgres comes back truncated, not equal.
    assert!(
        live[0].not_before >= later - chrono::TimeDelta::microseconds(1),
        "re-revoking moved the instant backwards: {} < {later}",
        live[0].not_before
    );

    assert_eq!(ops.purge_revocations().await.expect("purge"), 1, "only the expired row goes");
    assert_eq!(ops.live_revocations().await.expect("live").len(), 1);
}

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn erasure_and_retention_reach_every_row_that_names_a_person(pool: PgPool) {
    let ops = Ops { pool: pool.clone() };
    let t = tenant();
    let alice = job(&ops, Some(owner("alice"))).await;
    let bob = job(&ops, Some(owner("bob"))).await;
    // The denormalised column, which is what scoping and erasure both query.
    let subject: Option<String> =
        sqlx::query_scalar("SELECT owner_subject FROM ops.jobs WHERE job_id = $1").bind(alice).fetch_one(&pool).await.expect("row");
    assert_eq!(subject.as_deref(), Some("alice"), "create_jobs did not lift the subject out of the spec");

    let hk = HashKey::document(&t, &"a".repeat(64));
    ops.record_document_owner(&t, &hk, Some(&owner("alice")), Visibility::Private).await.expect("record");
    let other = HashKey::document(&t, &"b".repeat(64));
    ops.record_document_owner(&t, &other, Some(&owner("bob")), Visibility::Private).await.expect("record");
    ops.grant_document(&t, &other, GranteeKind::Subject, "alice", Some("bob")).await.expect("grant to alice");
    ops.grant_document(&t, &hk, GranteeKind::Department, "research", Some("alice")).await.expect("granted by alice");

    let erased = ops.erase_owner(&t, "alice").await.expect("erase");
    assert_eq!(erased.jobs, 1);
    assert_eq!(erased.documents, 1);
    assert_eq!(erased.grants, 2, "both the grant to alice and the one she made");

    // Bob is untouched: an erasure is one person's.
    let bobs: Option<String> =
        sqlx::query_scalar("SELECT owner_subject FROM ops.jobs WHERE job_id = $1").bind(bob).fetch_one(&pool).await.expect("row");
    assert_eq!(bobs.as_deref(), Some("bob"));

    // Retention: the subject is scrubbed from finished jobs that are old enough, and
    // only those.
    ops.set_state(bob, JobState::Done, None).await.expect("done");
    assert_eq!(ops.scrub_old_job_owners(Duration::from_secs(3600)).await.expect("scrub"), 0, "a job that just finished is not old");
    sqlx::query("UPDATE ops.jobs SET updated_at = now() - interval '100 days' WHERE job_id = $1")
        .bind(bob)
        .execute(&pool)
        .await
        .expect("age it");
    assert_eq!(ops.scrub_old_job_owners(Duration::from_secs(90 * 86_400)).await.expect("scrub"), 1);
    let (subject, in_spec, erased_at): (Option<String>, Option<String>, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT owner_subject, spec->'owner'->>'subject', owner_erased_at FROM ops.jobs WHERE job_id = $1")
            .bind(bob)
            .fetch_one(&pool)
            .await
            .expect("row");
    assert_eq!(subject, None);
    assert_eq!(in_spec, None, "the subject survived inside the spec, where nothing looks for it");
    assert!(erased_at.is_some(), "nothing records that this row was scrubbed, so it would be scrubbed again every sweep");
    // Idempotent: a second sweep finds nothing left to do.
    assert_eq!(ops.scrub_old_job_owners(Duration::from_secs(90 * 86_400)).await.expect("scrub"), 0);
}
