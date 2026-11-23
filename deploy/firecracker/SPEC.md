# deploy/firecracker — Specification

CPU tier: copies of the **service** in Firecracker microVMs, behind a load balancer on the
host. One microVM = one `vs-suite` process. System spec:
[`docs/SPEC.md`](../../docs/SPEC.md) — decision D1 (models are a separate GPU tier,
because Firecracker has no GPU passthrough).

> **This tier was marked superseded and is not any more.** It was specified for queue
> consumers: a microVM takes no inbound connections, so work could only be pushed to it,
> and when decision D13 removed the task bus there was no way to give a microVM a
> document. What changed is D12 — the suite became one service with an HTTP API, and a
> guest's address *is* reachable from its host across the guest's own TAP device. A load
> balancer on the host hands a microVM uploads exactly as it would any other copy, and
> `fleet.sh endpoints` is what its configuration is generated from.

## Status

Implemented: `build-rootfs.sh` (guest image with the glibc binary and its shared
libraries), `init.sh` (guest init, MMDS config fetch), `vm-config.example.json`,
`firecracker-run.sh` to boot one VM with a TAP device and NAT egress, and **`fleet.sh`**:
the host agent and the autoscaler.

`fleet.sh` does what this spec said did not exist:

| Command | What |
|---|---|
| `up --count N` / `scale --count N` | boots or retires VMs, lowest free ids first, so a TAP device and a guest address belong to one slot for as long as the fleet lives |
| `down` | SIGTERM to each (init forwards it, the service drains in-flight documents), then the TAPs and firewall chains released |
| `status` | each VM's guest address and its document-slot load, read from its own `/metrics` |
| `endpoints` | one `host:port` per line — the load balancer's configuration |
| `autoscale --min --max` | one copy at a time in either direction, on the fleet's mean document-slot load, because a copy takes seconds to become useful and up to the shutdown grace to retire |
| `snapshot --id N` | pause, write a snapshot, resume |

**Per-microVM egress allow-lists** are applied, not only specified: `--allow host:port`
(repeatable) builds one firewall chain per TAP with a default DROP, so a guest reaches
Postgres, the object store and the model endpoints and nothing else — including the other
guests. Without `--allow` the behaviour is the old one: NAT, unrestricted. Firecracker's
own rate limiters are in `vm-config.example.json`.

**`--dry-run` prints every command and touches nothing**, which is how the logic is
exercised on a host without `/dev/kvm`.

### One detail that snapshots made visible

A lease holder id is random per process, which is right for a container and wrong for a
restored snapshot — a snapshot *is* a copy of a process that already had one, so two
restores of the same snapshot would each believe they held the other's lease on a document.
The id therefore belongs to the slot rather than to the process image: `fleet.sh` passes
`VS_HOLDER_ID` through MMDS and the service prefers it over a generated one. A restored
snapshot must have its MMDS re-PUT before it resumes, for the same reason.

Remaining:
- [ ] **Nothing here runs in CI, and nothing here has been run as a fleet.**
      `.github/workflows/ci.yml` builds and tests the workspace; booting a microVM needs
      `/dev/kvm` and a TAP device, and the autoscaler needs several of them under load. So
      `fleet.sh` is exercised by its `--dry-run` and by hand, which is enough to say the
      logic is right and not enough to say the timings are.
- [ ] **Snapshot warm start is implemented and unproven.** A snapshot restores a running
      process, so its database pool and HTTP connections are dead sockets that the suite
      reconnects on its own — believable, untested, and the sub-second boot the design
      claims has never been measured.
- [ ] Sizing figures below are initial guesses pending a load test (see
      [`vs-worker`](../../crates/vs-worker/SPEC.md)): a microVM's vCPU and memory per
      concurrent document, and how many copies a host can hold before the render children
      contend.
- [ ] The guest's own kernel is Firecracker's CI kernel in every instruction here. A
      deployment wanting a minimal custom kernel has no recipe in this repo.

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
  /sbin/init            init.sh: mounts, network, MMDS, starts the service and reaps
  /usr/bin/vs-suite     glibc build (x86_64-unknown-linux-gnu)
  /usr/lib/...          the binary's runtime closure: glibc, libgcc_s, libpdfium.so
  /etc/vs/worker.toml   baked defaults; per-VM values via MMDS / env
  /etc/ssl/certs        CA bundle
  ```
- Scratch: second block device (ext4) mounted at `/scratch`, one per VM (`fleet.sh`
  copies it from a template — two VMs sharing a writable disk would corrupt each other's
  spooled originals). The ingest stage spools each original there and renders from the
  file, so size it to `max_upload_bytes` plus headroom; everything else is streamed to the
  object store (decision D8, Firecracker has no shared directories).
- No SSH. The **one** inbound port is the API, reached by the host's load balancer across
  the TAP; metrics are pushed (OTLP) or scraped on the TAP interface by the host only.

Build script: `deploy/firecracker/build-rootfs.sh` (inputs: the service binary, pdfium, CA
bundle, config TOML, optional models TOML, size → `rootfs.ext4`; see `README.md`). The
worker is not a static binary: a fully static musl binary cannot `dlopen`
`libpdfium.so`, and linking pdfium statically means building pdfium from source. The
rootfs is a full image anyway, so it ships the binary with its shared-library closure:
by default the libraries `ldd` resolves (plus the loader), or with `--nix-closure` the
`/nix/store` runtime closure of a Nix-built binary. The image is populated with
`mkfs.ext4 -d` from a staging directory (no root, no loop mount). `init.sh` becomes
`/sbin/init`; busybox provides `sh`, `mount`, `ip`, `wget`, `mkfs.ext4`, `poweroff`.

`init.sh` (PID 1, busybox sh): mounts `/proc`, `/sys`, `/dev`, `/run`, `/tmp`; mounts
(formatting on first use) `/dev/vdb` at `/scratch`; configures `eth0` from
`vs.ip=`/`vs.gw=`/`vs.dns=` or the kernel's `ip=`; fetches `/vs/env` from MMDS (V2 session
token) and exports each well-formed `VS__SECTION__KEY=value` line, plus `VS_HOLDER_ID` and
`VS_API_ADDR`, which `fleet.sh` writes per slot; starts `vs-suite` as a child with
`VS_CONFIG=/etc/vs/worker.toml` and `TMPDIR=/scratch`, forwards SIGTERM, reaps orphans
(the render children) and powers off when the service exits, which ends the Firecracker
process on the host.

## Per-VM configuration

`vm-config.example.json` (Firecracker API config): vCPU, memory, kernel args with the
guest address, read-only rootfs drive, writable scratch drive, TAP network interface with
rate limiters, MMDS V2 on `eth0`. Secrets (Postgres password, bucket credentials) are PUT
to MMDS as `{"vs": {"env": "VS__…=…\n…"}}` and exported by init before the service starts;
the baked `worker.toml` holds everything else.

`firecracker-run.sh` boots one VM: creates `tap-vs-<id>` with NAT egress, substitutes
kernel, rootfs, scratch disk, TAP, MAC and sizes into a copy of the example config, starts
Firecracker with it and loads a `VS__…` env file into MMDS. `fleet.sh` runs several of them
and is what a host runs.

## Fleet management

`fleet.sh` is the host agent. One per machine, no daemon of its own: a state directory of
pid files, a TAP device and a guest address per slot, and the Firecracker API socket per
VM.

1. `up`/`scale` start and retire VMs, lowest free id first, each with its own scratch disk
   copied from a template — two VMs sharing a writable disk would corrupt each other's
   spooled originals.
2. Scale-in sends SIGTERM, which init forwards to the service: the API stops accepting and
   in-flight documents get `--shutdown-grace`. A document still running when the grace
   elapses is abandoned, and another copy's resume sweep claims it — which is what the
   lease is for (§5.4).
3. `snapshot` pauses, writes a snapshot and resumes. A restored snapshot reconnects its
   database pool and HTTP clients on its own and **must have its MMDS re-PUT first**: its
   lease holder id comes from there, and two restores sharing one id would each believe
   they held the other's lease.

**Autoscaler**: `autoscale --min N --max M` moves one copy at a time on the fleet's mean
**document-slot load** — `worker_stage_in_flight{stage="doc"}` over `suite_max_documents`,
read from each VM's own metrics endpoint. Document slots rather than CPU, because
`worker.max_documents` is what actually bounds a copy and a copy at its limit is the thing
a new copy relieves. One at a time in either direction, because a copy takes seconds to
become useful and up to the shutdown grace to retire, and a fleet that oscillates is worse
than one that settles slowly.

There is no queue depth to scale on any more (D13). The honest consequence: this scales on
*occupancy*, which lags demand by however long a document takes — a burst of uploads is
absorbed by the API's back-pressure (`429` with `Retry-After`) before it shows up here.

## Networking

Each microVM gets a TAP device, NAT egress, and a firewall chain of its own with a
**default DROP**: `fleet.sh --allow host:port` (repeatable) is the allow-list, resolved to
addresses once at boot so a per-guest DNS resolver is not another thing to allow. Postgres
(PgBouncer), the object store, Qdrant, Keycloak, the model endpoints and the OTLP
collector are the usual entries; a guest cannot reach the other guests. Without `--allow`
the behaviour is NAT and nothing else, which is the development case.

Rate limits per microVM use Firecracker's built-in limiter (network and block) and are in
`vm-config.example.json` — 1 Gb/s each way by default, to protect the shared services from
one noisy guest.

Inbound is the host's load balancer reaching the API port across the TAP, and nothing
else: no SSH, no published ports. `fleet.sh endpoints` generates the balancer's
configuration.

## Sizing (initial)

See `crates/vs-worker/SPEC.md` "Sizing guidance". A microVM now runs every stage rather
than one role, so the three figures collapse into one: start with **2 vCPU / 4 GiB** per
copy with `worker.max_documents = 2`, and adjust from load tests. The render children are
the memory, the model calls are the wait, and nothing here has been measured.
