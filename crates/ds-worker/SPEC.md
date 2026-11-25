# ds-worker — Specification

Three **stages**, as a library: ingest, pages, doc. They were three deployable roles
pulling from a task bus; decision D13 removed the bus, so they are now function calls
behind [`Pipeline::run`](src/pipeline.rs) — one document, one process, the process that
accepted the upload. CPU only here: all model work goes over HTTP to the GPU tier.

The CLI, the signals and the service that calls this are
[`ds-suite`](../ds-suite/README.md)'s. System spec:
[`docs/SPEC.md`](../../docs/SPEC.md) §5.2, §5.3 and §5.4.

## Status

Implemented: all three stages, the pipeline that runs them (pages concurrently, with the
interactive reservation and bounded per-page retries), the cached DAG-list provider,
**document leases with a resume sweep**, **the document-type pass before the fan-out**,
and **page re-rendering for `crop_dpi`**. The stages live in `lib.rs` behind
`build_pipeline`, so a test builds what a deployment runs.

17 unit tests pass, with Stage 2 covered over fakes for the extractor, resolver and
embedder and the retry bound covered directly, plus `ds-suite`'s `tests/pipeline.rs`: one
document through **all three stages** into a live Postgres and Qdrant, asserting exact row
and point counts. That test substitutes only the model tier — a fake OpenAI-compatible
server on a real socket, answering from the prompt it was sent so grounding and
verification actually run. There is nothing else left to fake between the stages.

**A crashed copy no longer abandons its documents.** Each job is leased by the process
**carrying** it — from the moment the document is accepted, not from when a slot frees up
— and renewed for as long as it holds it (a third of the TTL, so two lost renewals are
survivable); a lease that stops being renewed is what a crash looks like from outside, and
any copy's resume sweep claims it (`FOR UPDATE SKIP LOCKED`) and runs the document again.
Leasing at acceptance is the difference between "waiting for a slot" and "abandoned",
which are otherwise the same thing seen from another copy: a wait longer than
`worker.lease_ttl_s` used to be claimed as a crash, and `worker.max_resumes` of that
failed a healthy document nobody had started. [`Pipeline::lease`] is that claim, and
`ds-suite`'s dispatcher takes it before it queues the work.

[`Pipeline::lease`]: src/pipeline.rs
Re-running resumes rather than repeats: rendered pages are skipped when their signals are
present and every vault write is idempotent. `worker.max_resumes` bounds it, so a document
that kills the process reading it cannot take the fleet down one copy at a time — past the
bound the job is failed with a reason the caller can read. The sweep lives in
[`ds-suite`](../ds-suite/README.md), which is the half that has a dispatcher.

**The document type is settled before the pages fan out.** When
`stage2.doc_classifier_model` is configured, the ingest stage shows the opening thumbnails
to the document classifier once per document, and every page is then classified knowing
what document it belongs to. This is the cross-page smoothing `ds-classify` asked for, in
the only place it can go: the page stage cannot do it (pages fan out with no shared state)
and Stage 2 is after routing. A failure there is not fatal — the pages classify themselves,
as they did before.

Remaining:
- [ ] Sizing guidance below is a starting guess, never validated by a load test, and it is
      a guess about one process doing everything rather than three doing one thing each.
      The numbers that matter — pages per copy before the model tier saturates, memory per
      concurrent document, how `max_documents` interacts with the render pool — need a
      real corpus on real hardware.
- [ ] Nothing samples *within* a stage. `telemetry.sample_ratio` applies to whole traces
      and is enough for volume, but a deployment that wants "every failed document, one in
      a thousand successful ones" needs a tail-based sampler in the collector rather than
      a head-based ratio here.

## The pipeline

```
startup: connect blob, postgres, vector store; load model registry; build StepRegistry;
         build all three stages (a missing model or an unreachable Qdrant fails here)
per accepted document:
  ingest  → render pages, write the document, return one PageRef per page
  pages   → buffer_unordered over the process's page slots:
              classify → route → run DAG → write page, blocks, provenance
              transient failure → retry (page_attempts, 2/4/8 s) ; then record failed
  doc     → stitch → extract → graph → Postgres + Qdrant, and the final job state
```

Implementation: `src/pipeline.rs`. Pages are independent, so a failed page is *recorded*
rather than propagated — `ops.page_tasks` holds its state and error — and the doc stage
decides from `worker.max_failed_page_fraction` whether the document is `done`, `partial`
or `failed`. A failure in the ingest or doc stage fails the job, because without them
there is nothing to finish.

`worker.max_in_flight` bounds pages in flight across every document this process is
running (default 32; most of a page is spent waiting on a model call, so it is much larger
than the vCPU count). `ceil(max_in_flight × interactive_share)` of those slots are
reserved for documents uploaded with `priority = interactive`, which is what the two queue
lanes used to do; batch work keeps at least one slot and may also use the shared ones.
`worker.max_documents` (default 4) bounds how many documents are in flight at once, since
each holds a render child and a spooled original through its ingest stage.

Retries are in-process and bounded: `worker.page_attempts` (default 3) with a 2/4/8 s
backoff, for transient errors only. A permanent error — bad page data, an unsupported
format — is not retried, because it will not become valid on a second read. This replaces
the queue's `max_deliver = 4` and its 10 s/60 s/300 s redelivery, which were sized for a
different mechanism: nothing is waiting to redeliver now.

Graceful shutdown on SIGTERM: stop accepting requests, then wait up to `shutdown_grace`
(default 60 s) for the documents in flight. Past the grace they are abandoned mid-document
and their job rows stay unfinished — see the status list.

## Stage `ingest`

`Task::Ingest` → `ds-ingest` flow (see its SPEC): render pages, write text layers and
signals, `VaultWriter::write_document`, fan out `Task::Page`.

Implemented in `src/ingest.rs`. Pages whose `signals.json` already exists are not
re-rendered (signals are written last, so their presence means the page is complete).
The original is spooled to `scratch_dir` and rendered by the render-child pool
(ds-ingest SPEC "Render children"); pages render in parallel and are uploaded as they
arrive. A page that hangs or crashes the renderer fails the document permanently. The DAG-list
version recorded for the run is the pinned one, else the tenant's or global active
list, else `none`.

## DAG-list provider (`src/dags.rs`)

`DagLists::router_for(tenant, pinned)` turns a task into a validated `Router`:

- `pinned` (`JobSpec::dag_list_version`): `Ops::dag_list_get`; an unknown version is a
  permanent error (bad request).
- otherwise the tenant's active list, else the global one (`Ops::active_dag_list`).
  The tenant → active-version mapping is cached for `stage2.dag_list_refresh_s`
  (default 60 s) and re-read on the first access after that; there is no background
  task. The pure cache (`ActiveCache`) takes `now` as an argument and is unit-tested
  with a fake clock.
- routers are cached per version (`RwLock<HashMap<version, Arc<Router>>>`); a version
  is immutable once uploaded, so the cache never expires.

Every list is validated with `Router::load(list, &production_registry)` on first use.
**No active list** and **a list that does not validate against this process's step
registry** are *transient* errors, so a page retries `page_attempts` times while an
operator activates a usable list via the API. That window is now seconds rather than the
queue's minutes, which is the honest consequence of removing the redelivery: a document
uploaded into a misconfigured deployment fails rather than waiting. There is no fallback
to `config/dag-list.example.json` — production routing must come from the registry.

## Stage `page` (Stage 1)

Implemented in `src/page.rs` (`PageHandler`).

```
ops.page_started(job_id, page_no)
router   = dags.router_for(tenant, spec.dag_list_version)
signals  = blob signals.json; image = blob page_image_uri (dimensions from the PNG header)
text     = blob text_layer_uri if given (missing object → empty layer, warning)
profile  = tier0 rules; + VlmPageClassifier(thumbnail) when stage2.classifier_model is set
           and the rules ask for vision; combine(language, signals, t0, vision, None, cfg)
sel      = router.select(profile, spec.profile)
out      = Executor::run(sel.dag, StepContext{..}, page_image, text_layer)
blocks   = drafts_to_blocks(out)                  (index, block_hk, provenance by node)
write PageResult → blob results/<doc>/<run>/page-<n>.json; VaultWriter::write_page(result, rule_id)
ops.page_finished(job_id, page_no, Done, dag_ref, rule_id)   (status + the doc stage's
                                                              failure threshold)
```

Stage 2 is idempotent per run, so running it twice for the same run writes nothing new.

No doc-level smoothing of the classifier in v1: `known_doc_type` is always `None`.
Without `stage2.classifier_model` pages are classified by rules only (tier 0); the
worker logs a warning at startup.

Error classes: missing or unparseable page assets (signals, image, text layer, page
image header), invalid DAGs, bad step params and unparseable model output are
permanent; model calls, I/O, node timeouts and blob or Postgres failures are transient.

A page that exhausts its attempts has `page_finished(.. Failed, error)` recorded for it by
the pipeline, which
runs the same completion check, so a failed last page still enqueues the doc task.

## Stage `doc` (Stage 2)

Implemented in `src/doc.rs` (`DocHandler`; the pure pipeline is `stage2()` over the
`Extractor`, `CorpusResolver` and `Embedder` traits and is unit-tested with fakes).

```
job = ops.get_job; pages_total from the job row;            state Stitching
pages = blob results/<doc>/<run>/page-<n>.json for n in 1..=pages_total
        (a missing object is a failed page)
if pages_failed / pages_total > worker.max_failed_page_fraction → permanent error → job Failed
stitched = ds_stitch::stitch(pages, pages_total)            state DocProcessing
extractor = LlmExtractor(ExtractConfig from [stage2]; language = spec.language;
            doc_profile from the dominant PageProfile.doc_type:
            scientific_paper → Scientific, contract → Legal, invoice|receipt → Financial, else Generic)
            with EmbeddingTypeNormalizer(PgOntologyStore, embedder, type_match_threshold)
ext      = extractor.extract(stitched, spec.profile)
for every DocEntity (8 concurrent): PgCorpusResolver::resolve(tenant, entity, context)
            context = text of the block of the entity's first mention; sets entity_hk;
            RunInfo::note_resolution collects the New entities
graph    = ds_graph::build(stitched, ext, title); graph.validate()
vectors  = embedder over graph.embed_targets in batches of stage2.embed_batch_size → Vectors
                                                             state Writing
final    = Partial if any page failed else Done
VaultWriter::write_stage2(RunMeta{run_id, dag_list_version, started_at = job.created_at},
            spec, stitched, ext, graph, run_info.doc_type)   -- one transaction: Data Vault
            rows and the graph layer together; sat_run = writing
VectorStore::write_run(RunInfo, graph, vectors)
VaultWriter::finish_run(RunMeta, spec, final)                -- sat_run = done | partial
write doc.json → blob results/<doc>/<run>/doc.json;  ops.set_state(final)
```

`doc.json` (`DocResult`, `schema_version` 1) carries the stitched document plus
`state`, `pages_total`, `pages_failed`, `missing_pages` and an `extraction` summary
(mentions, entities, new/linked entities, assertions, verified assertions, dropped
counts, graph node/edge counts, vectors).

`RunMeta.dag_list_version` is the pinned version, else the tenant's or global active
version at doc time, else `none` (the same rule ingest applies; the two can differ only
if a list was activated between ingest and doc, which the `sat_run` history then shows).

Postgres before the vector store: the vault is the system of record; if the vector write
fails the task is retried. Every step is idempotent on retry: page results are read-only,
the vault writer upserts, `VectorStore::write_run` drops a partial earlier attempt of the
same run first, and `doc.json` is overwritten.

**Why the run's final state is a third call.** Nothing can span two stores, so there is
always a window in which Postgres has the run and Qdrant does not. Writing `done` in the
same transaction as the rows put the lie inside the system of record: a vector-store
failure, or a worker killed in between, left `sat_run` claiming a complete run over a
document with nothing to search, indistinguishable from a real one. `write_stage2` now
leaves the run `writing` and `finish_run` closes it, so the vault's own record is true at
every point and a reader can tell "SQL in, vectors pending" from "complete".

Error classes: too many failed pages, unparseable page results, stitch errors (no
pages / mixed runs), extraction schema violations and graph invariant violations are
permanent; model, embedder, ontology-store, vector-store, blob and Postgres failures are
transient. Either way the pipeline sets the job to `Failed` with the error, because a
document whose Stage 2 did not run is not a document anyone can read.

## Metrics

Served per `ds-infra`'s telemetry section; every series also carries `service="ds-suite"`.

| Series | Kind | Notes |
|---|---|---|
| `worker_stage_total{stage,outcome}` | counter | `outcome` is `ok` or `error`, per attempt — so a retried page is counted once per attempt, which is the cost being measured |
| `worker_stage_duration_seconds{stage}` | histogram | one attempt of one stage |
| `worker_stage_in_flight{stage}` | gauge | pages against `max_in_flight`, documents against `max_documents` |

These replace `worker_tasks_*{role}`. The label is `stage` because that is what the three
things are now, and the old `outcome` values that described deliveries (`retried`,
`attempts_exhausted`, `no_result`) went with the queue that produced them.

The steps the page stage runs report themselves: see
[`ds-dag`](../ds-dag/README.md)'s and [`ds-models`](../ds-models/SPEC.md)'s metrics.

## Sizing guidance (to validate in load tests)

One process runs all three stages, so the figures add up rather than being chosen per
role. A copy sized for a few documents at a time:

| | |
|---|---|
| vCPU | 4 — rasterising is CPU-bound and runs in render children; the rest waits on the model tier |
| RAM | 6 GiB — Stage 2 holds a whole document, and `max_documents` of them may be in flight |
| `max_in_flight` | 32 pages |
| `max_documents` | 4 |

The old per-role table (ingest 2 vCPU/2 GiB, page 1–2/1 GiB, doc 2/4 GiB) is what these
were before one process did all three; it is kept here only as the provenance of the
numbers. All of it is still a guess: no load test has run.
