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
attempt cannot leave its job stuck. A role that is not
implemented refuses to start instead of consuming its tasks (currently `page` and
`doc`).

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

## Role `page` (Stage 1)

```
load page image + text layer + signals (blob)
profile  = classifier.classify(language, signals, thumbnail, known_doc_type)
sel      = router.select(profile, spec.profile)   (pinned list if spec.dag_list_version)
out      = executor.run(sel.dag, ctx, page_image, text_layer)
blocks   = drafts_to_blocks(out)                  (index, block_hk, provenance by node)
write PageResult → blob; VaultWriter::write_page
UPDATE ops.jobs SET pages_done = pages_done + 1 … RETURNING pages_done, pages_failed, pages_total
if pages_done + pages_failed == pages_total → publish Task::Doc (dedup id <job_id>:doc)
```

The completion check is an atomic counter in Postgres, so exactly one worker enqueues
the doc task (and JetStream de-dup covers retries).

A page that exhausts retries increments `pages_failed`; the same completion check runs.

## Role `doc` (Stage 2)

```
load all PageResults of the run (blob)
if pages_failed / pages_total > max_failed_page_fraction → job failed
stitched = vs_stitch::stitch(pages, pages_total)
ext      = extractor.extract(stitched, profile)
resolve entities against corpus (HelixResolver) → entity_hk
graph    = vs_graph::build(stitched, ext); graph.validate()
vectors  = embedder over graph.embed_targets (batched)
VaultWriter::write_stage2; GraphStore::write_run
write doc.json (blob); job state done | partial
```

Vault before Helix: the vault is the system of record; if the Helix write fails the
task is retried and the vault writes are idempotent.

## Sizing guidance (to validate in load tests)

| Role | vCPU | RAM | `max_in_flight` |
|------|------|-----|-----------------|
| ingest | 2 | 2 GiB | 4 (pdfium rendering is CPU-bound) |
| page | 1–2 | 1 GiB | 32 |
| doc | 2 | 4 GiB | 4 (large documents in memory) |
