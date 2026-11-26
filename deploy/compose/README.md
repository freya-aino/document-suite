# The integration stack

The suite, deployed, with a generated corpus pushed through its **public API**. This is
the first thing in the repository that runs the system the way a deployment does: the
built image, over HTTP, against a real Keycloak realm.

The existing end-to-end test (`crates/ds-suite/tests/pipeline.rs`) proves the pipeline,
but it calls `Pipeline::run` in process — no image, no HTTP, no token. The `image` CI job
builds the image and never runs the system inside it. This closes that gap.

```sh
deploy/compose/stack.sh up                  # tier `fake` (the default)
eval "$(deploy/compose/stack.sh env)"       # the variables the test driver reads
deploy/compose/stack.sh logs ds-suite
deploy/compose/stack.sh down
```

## What each tier proves, and what it does not

| Tier | Model serving | Where it runs | What it may claim |
|---|---|---|---|
| `fake` | `ds-fake-models`, one container | every pull request | the system is **wired**: upload, auth, ingest, routing, the executor, both stores, read-back |
| `cpu` | a small real VLM on CPU | weekly, or a workstation | that a real model tier is reachable and the parsers accept real output; fidelity `low-trust` |
| `gpu` | the vLLM tier in `deploy/gpu` | a GPU workstation, nightly | **extraction fidelity** and platform-relative latency |

**A green pull-request badge is not evidence about extraction quality.** The `fake` tier's
answers are written by us, in `crates/ds-testkit/src/models.rs`, so a fidelity threshold
measured against it is a measurement of that file. Fidelity is measured on `gpu`, against
a model nobody here wrote.

With one real exception, worth knowing because it is useful. A hinted born-digital page
routes to `born-digital-text`, whose `blocks.merge_native` takes its text from the PDF's
**own text layer** — pdfium, no model in the path at all. So even on the `fake` tier the
`text` metric is a genuine measurement (0.95+ against mechanically exact labels) and would
catch a regression in ingest or stitching. `layout` and `tables` on that tier are
measurements of the fake and read near zero by construction. The two must not be read the
same way, which is why `tests/fidelity.rs` prints the columns separately rather than only
the composite.

The tier is chosen with `-f` composition rather than a compose profile, because
`include:` pulls in every service of an included file unconditionally and cannot put a
profile on them — so `deploy/gpu/docker-compose.yml` cannot be gated from the base file.
Compose profiles are kept for genuine extras: `--profile mcp`, `--profile replicas`.

## What is here, and what is borrowed

| | |
|---|---|
| `docker-compose.yml` | Postgres, the `ds_app` role, the migration, `ds-suite`; `ds-mcp` and a second copy behind profiles |
| `compose.fake.yml` | the fake tier: one container, six registry hostnames as aliases |
| `worker.itest.toml` | the config mounted over the image's own, supplying `[blob]`, `[postgres]` and `[qdrant]` |
| `models/fake.toml` | the registry for the fake tier — **the same model ids as every other tier** |
| `sql/ds-app-role.sql` | the runtime role, shared with CI rather than inlined twice |
| `stack.sh` | brings it all up, waits for `/readyz`, prints the driver's environment |
| `baselines/` | per-host latency baselines (see `crates/ds-itest`) |

Two entry points beyond `stack.sh`, as devenv scripts (`crates/ds-itest/devenv.nix`):
`ds-itest-up` / `ds-itest-down` / `ds-itest-env`, `ds-itest` and `ds-latency` for the
suite, `ds-baseline-write` to record this host's baseline, and `ds-fidelity` for the whole
offline chain — corpus, candidates, runs, scores, a list, and **two** reports: a screen on
`train` and a confirmation on `holdout`. `ds-fidelity` builds in release, which is not a
preference: `page.denoise` takes 2.1 s released and 36.4 s in debug, past its own timeout,
so a debug chain scores every image conditioner as a failure.

**Qdrant, Garage and Keycloak are deliberately not defined here.** They need setup after
they are up — Garage's layout and keys, Keycloak's realm import — and
`.github/scripts/services.sh` already does all of it and is shellcheck-linted in CI.
`stack.sh` runs that script with `NETWORK` set, so those three join this network under
the aliases `qdrant`, `garage` and `keycloak` while keeping their published localhost
ports for the host-side driver. One definition, two callers.

## The one subtle thing: the issuer

Keycloak stamps `iss` from the request it answered. A token the host fetches over the
published port would say `http://127.0.0.1:57081/realms/koki`, while `ds-suite` inside
the network is configured for `http://keycloak:8080/realms/koki` — and every request
would be refused for a wrong issuer, which reads like a key problem for a long time.

So in network mode `services.sh` pins Keycloak's frontend URL (`KC_HOSTNAME`), making
`iss` the in-network one however the token was asked for. The host mints tokens the
containers accept. Only in network mode: with the frontend URL pinned, the discovery
document advertises `keycloak:8080`, which a host-side client cannot resolve — and that
is exactly what `ds-api`'s own `a_token_from_the_development_realm_verifies` does over
the published port in the normal CI path.

## Upload the corpus with its `doc_type` hint, or routing never engages

Found while bringing this up the first time, and it shapes what the suite can claim.

`classify.skip_vision_for_text_only_digital` is `true` (as it is in the example and dev
configs): a born-digital, text-only page skips the vision classifier, because paying a VLM
to classify a page whose text you already have is waste. The page then has no determined
document type, so `combine` reports `unknown_doc_type_confidence` (0.3), which is below
the routing table's `min_classifier_confidence` (0.6) — and every page takes the
**low-confidence DAG**. `router_low_confidence_total` counts it.

The generated corpus is entirely born-digital, so without a hint the router's *rule* path
is never exercised at all and "content-aware routing works" would be untested while every
document still processed successfully.

`ds-adapt dataset` already writes `doc_type_hint` into each manifest line, and a real
caller sends it. Pass it as `doc_type` in the upload's `options` part and confidence
becomes `rules_only_confidence` (0.85), above the threshold, and the rules engage —
verified here: `routed dag: born-digital-text@1, rule: balanced/digital=true/images=false`.

So the suite uploads most documents **with** the hint, and at least one **without**, to
cover both paths deliberately rather than by accident.

## Ports

All published to `127.0.0.1` only, and all in the 5700x range so the stack can run beside
`devenv --profile stack up` and any other project stack already holding the obvious
numbers. Every one is overridable, and `stack.sh up` fails rather than half-starting if
one is taken.

| Port | Service | Override |
|---|---|---|
| 57080 | `ds-suite` HTTP API | `DS_ITEST_API_PORT` |
| 57464 | `ds-suite` metrics, **unauthenticated** | `DS_ITEST_METRICS_PORT` |
| 57432 | Postgres | `DS_ITEST_PG_PORT` |
| 57334 / 57333 | Qdrant gRPC / REST | `QDRANT_GRPC` / `QDRANT_REST` |
| 57900 | Garage (S3) | `S3_PORT` |
| 57081 | Keycloak | `KEYCLOAK_PORT` |
| 57082 | `ds-mcp` (`--profile mcp`) | `DS_ITEST_MCP_PORT` |

`GET /metrics` on 8080 is behind the admin role on purpose — it publishes request volumes
and error rates to anyone who asks. The 9464 port exists so the harness can scrape the
latency histograms without a token, and it is bound inside the compose network and
published to localhost only.

## Why the service is configured one document at a time

`worker.itest.toml` sets `max_documents = 1` and `max_in_flight = 1`. The latency pass
needs queueing *outside* the measurement: `RunRecord::latency_ms` is already polluted by
`ds-adapt`'s own semaphore, and a suite that measures contention on a shared CI runner is
measuring the runner. Throughput at real concurrency is a separate figure, reported and
never gated.

## Troubleshooting

- **`missing field qdrant`** (or `blob`, or `postgres`) — the service came up without
  `worker.itest.toml` mounted. That failure is deliberate: the image's baked config omits
  those sections so a misconfigured container stops instead of running pointed at nothing.
- **Every request is 401** — see *the issuer* above.
- **`ds-suite` exits at boot** — it refuses to start without a reachable realm, a
  migrated schema, a registry whose every model resolves, and reachable Qdrant and S3.
  `stack.sh` checks the migration's exit code and then polls `/readyz`; `stack.sh logs
  ds-suite` has the rest.
- **`/readyz` is 200 but uploads fail** — `/readyz` probes Postgres and the blob store
  only. It says nothing about Qdrant or the model tier.
