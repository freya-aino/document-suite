# vs-suite — the service

One binary, one process, one deployable unit: it serves the HTTP API and runs all three
worker roles. System spec: [`docs/SPEC.md`](../../docs/SPEC.md) §3 (architecture, decision
D12). What a deployment must provide around it:
[`deploy/README.md`](../../deploy/README.md).

```sh
vs-suite                              # api + ingest + page + doc
vs-suite --roles api                  # a copy that only serves requests
vs-suite --roles ingest,page,doc      # a copy that only consumes the queue
vs-suite migrate --database-url …     # deploy step, as the schema owner
```

## Why one process

The suite was two binaries, and the second was run three times with `--role`. Nothing
about the work required that: the roles hand tasks to each other over NATS, so whether
they sit in one process or four changes the deployment and nothing else. It did require
every deployment embedding this project to learn the suite's internal shape before it
could run any of it.

So the roles are a concurrency decision again. Scaling out is more copies of this
service, which share the queue exactly as separate processes did. `--roles` narrows a copy
when a deployment wants the halves scaled separately — goal G8 stays available, as that
deployment's choice rather than a shape imposed on it. Two places use it already: the
Firecracker CPU tier runs worker roles only, because a microVM takes no inbound
connections, and `--roles api` is how you put request handling behind its own autoscaler.

## What this crate is

The CLI, the startup order, the signals, and nothing else — the same arrangement
[`vs-worker`](../vs-worker/SPEC.md) already had internally, now covering the API too.
The routes live in [`vs-api`](../vs-api/SPEC.md) (`vs_api::router`), the roles in
[`vs-worker`](../vs-worker/SPEC.md) (`vs_worker::build_handler`), and both are libraries
so a test can build what this binary runs. `tests/pipeline.rs` is that test: one document
through all three roles into a live Postgres and Qdrant.

What the process shares between its roles:

| | |
|---|---|
| Postgres pool | one, `postgres.max_connections` for the whole process rather than per role |
| NATS connection | one, with the stream topology ensured once at startup |
| Metrics recorder and tracer | one per process (`vs-infra` requires it), labelled by which roles this copy runs |
| Shutdown | one signal: the API stops accepting and the role loops drain together, within `--shutdown-grace` |

`vs-suite render-child` is this binary re-executed by the ingest role's render pool, which
is why the rasterizer needs no second artifact. It speaks a binary protocol on stdout, so
that path starts neither the runtime nor logging.

## Flags

| Flag | Environment | Default |
|---|---|---|
| `--roles` | `VS_ROLES` | `api,ingest,page,doc` |
| `--config` | `VS_CONFIG` | `/etc/vs/worker.toml` |
| `--addr` | `VS_API_ADDR` | `0.0.0.0:8080` (ignored without the `api` role) |
| `--shutdown-grace` | `VS_SHUTDOWN_GRACE` | `60` seconds |

The `api` role requires an `[auth]` section and fails startup without one; a copy running
no API does not need a realm at all. Config keys and their `VS__*` overrides:
[`config/worker.example.toml`](../../config/worker.example.toml).

## Known limitations

- **`worker.max_in_flight` is per role, not per process.** A copy running all three roles
  can have three times that many tasks in flight, which is the right default for a queue
  that is mostly idle in two of its three subjects and the wrong one for a small VM. There
  is no process-wide budget.
- **A role that fails at startup fails the process.** A missing model, an unreachable
  Qdrant or a wrong embedding dimension stops the whole service rather than the one role
  that needed it — deliberate, since a service silently missing a stage is worse, but it
  does mean the API will not serve while Stage 2's dependencies are down.
- **`--roles` is not visible in the metrics beyond the `service` label.** Two copies with
  different subsets are distinguishable, but there is no per-role `up` series.
