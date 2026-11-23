# vs-suite — the service

One binary, one process, one deployable unit: it serves the HTTP API and processes the
documents it accepts. System spec: [`docs/SPEC.md`](../../docs/SPEC.md) §3 (architecture,
decisions D12 and D13). What a deployment must provide around it:
[`deploy/README.md`](../../deploy/README.md).

```sh
vs-suite                              # the service
vs-suite migrate --database-url …     # deploy step, as the schema owner
```

## Why one process

The suite was two binaries and the second was run three times with `--role`, with a NATS
JetStream bus between the stages. Nothing about the work required that: the per-page DAG
was always executed in-process, so the queue's only unique contribution was spreading one
document's pages across machines. It cost a service to run, three deployable roles, a
redelivery protocol, dead-lettering, lanes and de-duplication ids — and it required every
deployment embedding this project to learn the suite's internal shape before it could run
any of it.

So an accepted upload is now processed by the process that accepted it:
`vs_worker::Pipeline` runs ingest, then the pages concurrently, then Stage 2. Scaling out
is more copies, each taking whole documents.

**What that gives up**, stated here because it is the kind of thing a reader should not
have to discover: a single large document is processed by one copy rather than spread
across the fleet.

What it no longer gives up is recovery. Each document is **leased** by the copy working on
it, and `resume.rs` — this crate's sweep — claims the ones whose leases lapsed, which is
what a crashed copy leaves behind. Re-running resumes rather than repeats: rendered pages
are skipped when their signals are present and every vault write is idempotent.

## What this crate is

The CLI, the startup order, the signals, and the dispatcher — nothing else. The routes
live in [`vs-api`](../vs-api/SPEC.md) (`vs_api::router`), the stages in
[`vs-worker`](../vs-worker/SPEC.md) (`vs_worker::build_pipeline`), and both are libraries
so a test can build what this binary runs. `tests/pipeline.rs` is that test: one document
through all three stages into a live Postgres and Qdrant, driving the same
`Pipeline::run` the service calls.

`Documents` is the dispatcher. `dispatch` returns as soon as a document is accepted — the
response is a `202` and a job id — and the work then waits for a slot in a spawned task
rather than holding the request open. What stops that waiting set from growing without
limit is the API's own admission check, which reads `in_flight`.

What the process shares:

| | |
|---|---|
| Postgres pool | one, `postgres.max_connections` for the whole process |
| Metrics recorder and tracer | one per process (`vs-infra` requires it), `service="vs-suite"` |
| Page slots | `worker.max_in_flight` across every document, with `worker.interactive_share` reserved for interactive uploads |
| Document slots | `worker.max_documents` |
| Shutdown | one signal: the API stops accepting, then in-flight documents get `--shutdown-grace` |

`vs-suite render-child` is this binary re-executed by the ingest stage's render pool,
which is why the rasterizer needs no second artifact. It speaks a binary protocol on
stdout, so that path starts neither the runtime nor logging.

## Flags

| Flag | Environment | Default |
|---|---|---|
| `--config` | `VS_CONFIG` | `/etc/vs/worker.toml` |
| `--addr` | `VS_API_ADDR` | `0.0.0.0:8080` |
| `--shutdown-grace` | `VS_SHUTDOWN_GRACE` | `60` seconds |

`[auth]` is required and the process fails startup without it. So is a reachable model
tier and Qdrant: every copy runs every stage, so a missing model or a wrong embedding
dimension is a startup failure rather than a surprise at the first document that got that
far. Config keys and their `VS__*` overrides:
[`config/worker.example.toml`](../../config/worker.example.toml).

## What runs beside the pipeline

Four sweeps, all of them in every copy, because all four are idempotent and electing one
owner would need a lock for jobs whose worst case is a wasted query:

| Sweep | Every | What |
|---|---|---|
| resume (`resume.rs`) | `worker.resume_interval_s` | claims documents a crashed copy abandoned, and fails the ones past `worker.max_resumes` |
| idempotency purge | 1 h | drops expired `Idempotency-Key` rows |
| job retention | 6 h | scrubs the caller's subject from jobs finished longer ago than `api.job_retention_days` |
| object retention | `blob.retention.sweep_interval_h` | prunes derived objects past their retention; originals are kept |

The resume sweep claims nothing while this copy is already carrying
`worker.max_documents`: a claim it cannot start is a lease held over a document nobody is
working on, which is worse than leaving it for a copy with room.

## Known limitations

- **Admission control is per copy.** `api.interactive_max_pending` counts this process's
  documents, so a load balancer can get a `429` from one copy while another is idle. The
  same is true of `api.tenant_rate_per_s`: a fleet of N copies allows N × the rate.
- **The resume sweep scales on occupancy, not demand.** It notices a document nobody is
  working on; it cannot notice one nobody has uploaded yet. A burst is absorbed by
  back-pressure at the API, not here.
- **A stage that fails at startup fails the process.** Deliberate — a service that
  accepted uploads it could not finish would be worse — but it does mean the API will not
  serve while Stage 2's dependencies are down.
