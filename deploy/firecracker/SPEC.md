# deploy/firecracker — Specification

CPU tier: `vs-worker` processes in Firecracker microVMs. One microVM = one worker
process = one role.

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
  /usr/bin/vs-worker    static musl build (cargo build --target x86_64-unknown-linux-musl)
  /usr/lib/libpdfium.so pdfium build for rendering (ingest role only; same image for simplicity)
  /etc/vs/worker.toml   baked defaults; per-VM values via MMDS / env
  /etc/ssl/certs        CA bundle
  ```
- Scratch: optional second block device (tmpfs-like ext4) for large page renders during
  ingest; everything else is streamed to the object store (decision D8, Firecracker has no
  shared directories).
- No SSH, no inbound ports. Metrics are pushed (OTLP) or scraped on the TAP interface by
  the host agent only.

Build script location: `deploy/firecracker/build-rootfs.sh` (to be written; inputs:
worker binary, pdfium, CA bundle → `rootfs.ext4`).

## Per-VM configuration

`vm-config.example.json` (Firecracker API config): vCPU, memory, kernel args with
`VS_ROLE`, rootfs drive, TAP network interface, MMDS enabled. The worker reads
role-independent secrets (Postgres password, bucket credentials) from MMDS at start.

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
object store, HelixDB, model endpoints, OTLP collector. Rate limits per microVM with
Firecracker's built-in limiter (network and block) to protect shared services.

## Sizing (initial)

See `crates/vs-worker/SPEC.md` "Sizing guidance". Start with: ingest 2 vCPU / 2 GiB,
page 1 vCPU / 1 GiB, doc 2 vCPU / 4 GiB; adjust from load tests.
