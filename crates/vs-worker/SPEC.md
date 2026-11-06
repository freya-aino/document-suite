# vs-worker — Specification

One binary, three roles (decision D2). Runs inside Firecracker microVMs, CPU only; all
model work goes over HTTP to the GPU tier. One microVM runs one worker process with one
role (set via `--role` / `VS_ROLE`, passed by the fleet manager through the kernel
command line or MMDS).

## Common loop

```
connect bus, blob, postgres, helix; load model registry; build StepRegistry
load active DAG list → Router (refresh on registry change notification / every 60 s)
loop:
  pull from interactive lane (up to interactive_share of slots), then batch lane
  run handler under a per-task tracing span; heartbeat in_progress every ack_wait/3
  Ok  → ack
  Err(transient) and attempt < max  → nak(backoff)
  Err(permanent) or attempt == max  → dead_letter + term; mark page/job failed
```

Implementation (`src/runtime.rs`): one pull loop per lane; a loop pulls only after it
holds a slot. `ceil(max_in_flight × interactive_share)` slots are reserved for the
interactive lane (batch always keeps at least one); interactive tasks also use the
shared slots. Handler errors are transient unless marked permanent (bad input,
unsupported format); transient errors are `nak`ed with the bus backoff
(10 s / 60 s / 300 s) until the last attempt, then dead-lettered like permanent ones,
and the handler's `give_up` marks the job or page failed. A third loop consumes the role's exhausted tasks (vs-infra SPEC: max-deliveries
advisories) and runs the same give-up path, so a task whose worker crashed on every
attempt cannot leave its job stuck.

Startup per role (`src/main.rs`): all roles connect bus, blob and Postgres. `page` and
`doc` additionally load the model registry (`models.registry`; a missing file is a
startup error for these roles only — `ingest` calls no model), build the
`OpenAiClient` and check at startup that every configured `[stage2]` model exists in
the registry. `page` builds `vs_steps::production_registry`; `doc` builds the
`HelixStore`, the `OpenAiEmbedder` and the `HelixResolver`.

`max_in_flight` concurrent tasks per process (default 32 for `page`; most of the time
is spent waiting on model calls, so this is much larger than the vCPU count).

Graceful shutdown on SIGTERM: stop pulling, finish in-flight tasks within
`shutdown_grace` (default 60 s), `nak` the rest. Needed for scale-in and snapshotting.

## Role `ingest`

`Task::Ingest` → `vs-ingest` flow (see its SPEC): render pages, write text layers and
signals, `VaultWriter::write_document`, fan out `Task::Page`.

Implemented in `src/ingest.rs`. Pages whose `signals.json` already exists are not
re-rendered (signals are written last, so their presence means the page is complete).
The original is spooled to `scratch_dir` and rendered by the render-child pool
(vs-ingest SPEC "Render children"); pages render in parallel and are uploaded as they
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
**No active list** and **a list that does not validate against this worker's step
registry** are *transient* errors: the tasks wait with the bus backoff (and are
dead-lettered only after `MAX_DELIVER` attempts) while an operator activates a usable
list via `vs-api`, instead of being dead-lettered immediately. There is no fallback to
`config/dag-list.example.json`: production routing must come from the registry.

## Role `page` (Stage 1)

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
progress = ops.page_finished(job_id, page_no, Done, dag_ref, rule_id)
if progress.all_finished() → publish Task::Doc on the spec's lane (dedup id <job_id>:doc)
```

`page_finished` moves the page row to its final state and bumps the job counters in one
statement; only the first call per page counts, so redeliveries cannot double count.
The doc task is published whenever the counters show every page final
(`all_finished()`), not only by the call that counted the last page
(`completed_job()`): a page task redelivered after its earlier attempt counted the
page but died before publishing must still publish, otherwise the job would stay in
`pages_processing` forever. The duplicate publish of a healthy retry is absorbed by the
JetStream de-dup id; beyond the de-dup window Stage 2 is idempotent per run.

No doc-level smoothing of the classifier in v1: `known_doc_type` is always `None`.
Without `stage2.classifier_model` pages are classified by rules only (tier 0); the
worker logs a warning at startup.

Error classes: missing or unparseable page assets (signals, image, text layer, page
image header), invalid DAGs, bad step params and unparseable model output are
permanent; model calls, I/O, node timeouts, blob/Postgres/bus failures are transient.

`give_up` (permanent error or last attempt) calls `page_finished(.. Failed, error)` and
runs the same completion check, so a failed last page still enqueues the doc task.

## Role `doc` (Stage 2)

Implemented in `src/doc.rs` (`DocHandler`; the pure pipeline is `stage2()` over the
`Extractor`, `CorpusResolver` and `Embedder` traits and is unit-tested with fakes).

```
job = ops.get_job; pages_total from the job row;            state Stitching
pages = blob results/<doc>/<run>/page-<n>.json for n in 1..=pages_total
        (a missing object is a failed page)
if pages_failed / pages_total > worker.max_failed_page_fraction → permanent error → job Failed
stitched = vs_stitch::stitch(pages, pages_total)            state DocProcessing
extractor = LlmExtractor(ExtractConfig from [stage2]; language = spec.language;
            doc_profile from the dominant PageProfile.doc_type:
            scientific_paper → Scientific, contract → Legal, invoice|receipt → Financial, else Generic)
            with EmbeddingTypeNormalizer(PgOntologyStore, embedder, type_match_threshold)
ext      = extractor.extract(stitched, spec.profile)
for every DocEntity (8 concurrent): HelixResolver::resolve(tenant, entity, context)
            context = text of the block of the entity's first mention; sets entity_hk;
            RunInfo::note_resolution collects the New entities
graph    = vs_graph::build(stitched, ext, title); graph.validate()
vectors  = embedder over graph.embed_targets in batches of stage2.embed_batch_size → Vectors
                                                             state Writing
final    = Partial if any page failed else Done
VaultWriter::write_stage2(RunMeta{run_id, dag_list_version, started_at = job.created_at}, spec, stitched, ext, final)
GraphStore::write_run(RunInfo, graph, vectors)
write doc.json → blob results/<doc>/<run>/doc.json;  ops.set_state(final)
```

`doc.json` (`DocResult`, `schema_version` 1) carries the stitched document plus
`state`, `pages_total`, `pages_failed`, `missing_pages` and an `extraction` summary
(mentions, entities, new/linked entities, assertions, verified assertions, dropped
counts, graph node/edge counts, vectors).

`RunMeta.dag_list_version` is the pinned version, else the tenant's or global active
version at doc time, else `none` (the same rule ingest applies; the two can differ only
if a list was activated between ingest and doc, which the `sat_run` history then shows).

Vault before Helix: the vault is the system of record; if the Helix write fails the
task is retried. Every step is idempotent on retry: page results are read-only, the
vault writer upserts, `write_run` drops a partial earlier attempt of the same run
first, and `doc.json` is overwritten.

Error classes: too many failed pages, unparseable page results, stitch errors (no
pages / mixed runs), extraction schema violations and graph invariant violations are
permanent; model, embedder, ontology-store, Helix, blob and Postgres failures are
transient. `give_up` sets the job to `Failed` with the error.

## Sizing guidance (to validate in load tests)

| Role | vCPU | RAM | `max_in_flight` |
|------|------|-----|-----------------|
| ingest | 2 | 2 GiB | 4 (pdfium rendering is CPU-bound) |
| page | 1–2 | 1 GiB | 32 |
| doc | 2 | 4 GiB | 4 (large documents in memory) |
