# deploy/firecracker — Specification

CPU tier: `vs-worker` processes in Firecracker microVMs. One microVM = one worker
process = one role. System spec: [`docs/SPEC.md`](../../docs/SPEC.md) — decision D1.

## Status

Implemented: `build-rootfs.sh` (guest image with the glibc worker and its shared
libraries), `init.sh` (guest init, MMDS config fetch), `vm-config.example.json`, and
`firecracker-run.sh` to boot one VM locally with a TAP device and NAT egress.

**This is a single-VM development path, not a deployable fleet.** Nothing here has been
run against real hardware in CI or staging, because there is no CI.

Remaining:
- [ ] **No host agent**, so there is no way to run more than one microVM per host on
      purpose. Deliberately out of scope for this repo (options: flintlock,
      firecracker-containerd, or a small agent over the Firecracker API), but it means the
      CPU tier is not deployable as specified until it exists.
- [ ] **No autoscaler.** "Fleet management" below specifies the target-count formula and
      the GPU-latency cap on page workers; nothing computes either.
- [ ] No snapshot-based warm start, so the sub-second boot the design relies on is
      unproven.
- [ ] Per-microVM network egress allow-lists and Firecracker rate limiters are specified
      under "Networking" but not configured in `vm-config.example.json`.
- [ ] The guest init is settled as `init.sh`; "Guest image" below still presents it as an
      open either/or with running the worker as PID 1.
- [ ] Sizing figures are initial guesses pending a load test (see `vs-worker`).

## Assumptions this component rests on

| ID | Assumption |
|----|-----------|
| A4 | Model servers run outside Firecracker (bare metal / Kubernetes), because Firecracker has no GPU passthrough. Cloud Hypervisor with VFIO is the option if GPU workloads must be VM-isolated. |

## Why Firecracker here (and not for models)

Firecracker gives strong isolation per worker (one guest kernel per microVM), boots in
~125 ms, and adds only ~5 MiB of memory overhead per microVM, so thousands of workers fit on a
host. It has **no GPU/PCIe passthrough**, so model servers run elsewhere (`deploy/gpu`).
Workers only need network, a root disk and CPU.

## Guest image

- Kernel: minimal Linux guest kernel (Firecracker CI config), virtio-net + virtio-block.
- Rootfs: read-only ext4, built from scratch (no distro package manager in the image):
  ```
  /sbin/init            tiny init (or the worker binary itself as PID 1 via a shim that reaps)
  /usr/bin/vs-worker    glibc build (x86_64-unknown-linux-gnu)
  /usr/lib/...          the binary's runtime closure: glibc, libgcc_s, libpdfium.so
                        (pdfium for the ingest role; same image for simplicity)
  /etc/vs/worker.toml   baked defaults; per-VM values via MMDS / env
  /etc/ssl/certs        CA bundle
  ```
- Scratch: second block device (ext4) mounted at `/scratch`; **required for the ingest
  role**, which spools each original there and renders from the file (sized to
  `max_upload_bytes` plus headroom); everything else is streamed to the object store (decision D8, Firecracker has no
  shared directories).
- No SSH, no inbound ports. Metrics are pushed (OTLP) or scraped on the TAP interface by
  the host agent only.

Build script: `deploy/firecracker/build-rootfs.sh` (inputs: worker binary, pdfium, CA
bundle, config TOML, optional models TOML, size → `rootfs.ext4`; see `README.md`). The
worker is not a static binary: a fully static musl binary cannot `dlopen`
`libpdfium.so`, and linking pdfium statically means building pdfium from source. The
rootfs is a full image anyway, so it ships the binary with its shared-library closure:
by default the libraries `ldd` resolves (plus the loader), or with `--nix-closure` the
`/nix/store` runtime closure of a Nix-built worker. The image is populated with
`mkfs.ext4 -d` from a staging directory (no root, no loop mount). `init.sh` becomes
`/sbin/init`; busybox provides `sh`, `mount`, `ip`, `wget`, `mkfs.ext4`, `poweroff`.

`init.sh` (PID 1, busybox sh): mounts `/proc`, `/sys`, `/dev`, `/run`, `/tmp`; mounts
(formatting on first use) `/dev/vdb` at `/scratch`; configures `eth0` from
`vs.ip=`/`vs.gw=`/`vs.dns=` or the kernel's `ip=`; reads the role from `vs.role=`
(fallback `VS_ROLE` from the environment, which the kernel fills from a bare
`VS_ROLE=…` boot argument); fetches `/vs/env` from MMDS (V2 session token) and exports
each well-formed `VS__SECTION__KEY=value` line; starts `vs-worker --role $ROLE` as a
child with `VS_CONFIG=/etc/vs/worker.toml` and `TMPDIR=/scratch`, forwards SIGTERM,
reaps orphans (render children of the ingest role) and powers off when the worker exits,
which ends the Firecracker process on the host.

## Per-VM configuration

`vm-config.example.json` (Firecracker API config): vCPU, memory, kernel args with
`vs.role=` and the guest address, read-only rootfs drive, writable scratch drive, TAP
network interface with rate limiters, MMDS V2 on `eth0`. Secrets (Postgres password,
bucket credentials) are PUT to MMDS as `{"vs": {"env": "VS__…=…\n…"}}` and exported
by init before the worker starts; the baked `worker.toml` holds everything else.

`firecracker-run.sh` boots one VM locally: creates `tap-vs-<id>` with NAT egress,
substitutes kernel, rootfs, scratch disk, TAP, MAC and sizes into a copy of the example
config, starts Firecracker with it and loads a `VS__…` env file into MMDS. It is a
development wrapper; the host agent below replaces it in production.

## Fleet management

A host agent per machine (out of scope for this repo; options: flintlock,
firecracker-containerd, or a small custom agent using the Firecracker API) that:

1. reads desired counts per (role) from the autoscaler;
2. starts microVMs from a **snapshot** of a booted, idle worker (warm start, sub-second);
3. sends SIGTERM through the guest agent for scale-in and waits for `shutdown_grace`.

**Autoscaler**: target count per role = `ceil(num_pending / tasks_per_worker_target)`,
bounded by min/max, computed from JetStream consumer info per role and lane. GPU
capacity is the real limit for `page` workers: the autoscaler also caps page workers
when model latency p95 rises above `model_latency_ceiling` (adding workers would only
add queueing on the GPUs).

## Networking

Each microVM gets a TAP device with egress allowed only to: NATS, Postgres (PgBouncer),
object store, Qdrant, model endpoints, OTLP collector. Rate limits per microVM with
Firecracker's built-in limiter (network and block) to protect shared services.

## Sizing (initial)

See `crates/vs-worker/SPEC.md` "Sizing guidance". Start with: ingest 2 vCPU / 2 GiB,
page 1 vCPU / 1 GiB, doc 2 vCPU / 4 GiB; adjust from load tests.
