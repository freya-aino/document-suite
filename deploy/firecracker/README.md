# deploy/firecracker — build and boot a ds-suite microVM

One microVM runs one copy of the **service**, and a load balancer on the host hands it
uploads — the suite is one HTTP service (D12), and a guest is reachable across its own TAP
device. Design and sizing: [SPEC.md](SPEC.md). Files here:

| File | Purpose |
|------|---------|
| `build-rootfs.sh` | builds `rootfs.ext4` from the worker binary, pdfium, CA bundle and config |
| `init.sh` | the guest's `/sbin/init`: mounts, scratch disk, network, MMDS secrets, starts the worker |
| `vm-config.example.json` | Firecracker config: kernel args, drives, TAP interface, MMDS |
| `firecracker-run.sh` | one VM: TAP device + NAT, config substitution, MMDS secrets, boot |
| `fleet.sh` | several VMs on one host: boot, scale, status, endpoints, egress allow-lists, snapshots, autoscaling |
| `devenv.nix` | `devenv --profile firecracker shell` with firecracker and e2fsprogs |

## 1. Build the pieces

```sh
# glibc build of the service (the rootfs ships its shared-library closure)
cargo build --release -p ds-suite --target x86_64-unknown-linux-gnu

# a guest kernel: Firecracker's CI kernels work out of the box
curl -fLo vmlinux \
  https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/x86_64/vmlinux-6.1.102

# the root filesystem
deploy/firecracker/build-rootfs.sh \
  --service target/x86_64-unknown-linux-gnu/release/ds-suite \
  --pdfium "$PDFIUM_DIR/libpdfium.so" \
  --ca-bundle /etc/ssl/certs/ca-certificates.crt \
  --config config/worker.toml \
  --models config/models.toml \
  --size 512M --out rootfs.ext4

# a scratch disk for the originals the ingest stage spools (max_upload_bytes plus
# headroom; see SPEC.md)
truncate -s 4G scratch-1.ext4 && mkfs.ext4 -q -F scratch-1.ext4
```

`build-rootfs.sh` copies the libraries `ldd` resolves for the worker and pdfium. For a
Nix-built worker pass `--nix-closure` and the `/nix/store` runtime closure is copied
instead (`nix path-info -r`). `busybox` is taken from `PATH` or `--busybox`; a static
busybox keeps the image small. The build needs no root: `mkfs.ext4 -d` populates the
image from a directory.

The baked `worker.toml` holds everything that is not secret (object store, Postgres URL
without password, Qdrant URL, model registry path, `[auth]`). Secrets arrive via MMDS,
along with the slot's own `DS_HOLDER_ID` and `DS_API_ADDR`.

## 2. Boot one VM locally

```sh
sudo deploy/firecracker/firecracker-run.sh --id 1 \
  --kernel vmlinux --rootfs rootfs.ext4 --mmds-env secrets.env

# with a scratch disk, which every copy wants: each one ingests
sudo deploy/firecracker/firecracker-run.sh --id 2 --vcpus 2 --mem 2048 \
  --kernel vmlinux --rootfs rootfs.ext4 --scratch scratch-1.ext4 --mmds-env secrets.env
```

`secrets.env` is one `DS__SECTION__KEY=value` per line, e.g.

```
DS__POSTGRES__PASSWORD=...
DS__BLOB__ACCESS_KEY_ID=...
DS__BLOB__SECRET_ACCESS_KEY=...
```

The wrapper creates `tap-ds-<id>` (host `172.16.<id>.1/30`, guest `.2`), NATs egress
through the default route, substitutes kernel/rootfs/TAP/sizes into a copy of
`vm-config.example.json`, boots, and PUTs the secrets into MMDS as
`{"ds": {"env": "<file contents>"}}`. The guest console is the terminal; the VM
powers off when the worker exits (`reboot=k panic=1` makes Firecracker exit).

## 3. What the guest does (init.sh)

1. mounts `/proc`, `/sys`, `/dev`, `/run`, `/tmp`; mounts (or formats and mounts) the
   second block device at `/scratch`;
2. (there are no roles to select: the service runs every stage);
3. configures `eth0` from `ds.ip=`/`ds.gw=`/`ds.dns=` or the kernel's `ip=` argument;
4. fetches a MMDS V2 session token and `GET /ds/env`, exports every well-formed
   `DS__NAME=value` line (nothing is sourced as shell);
5. runs `/usr/bin/ds-suite` with `DS_CONFIG=/etc/ds/worker.toml` and `TMPDIR=/scratch`,
   forwards `SIGTERM`, reaps orphaned children (the render processes), and powers off when
   the service exits.

What the guest reaches *out* to — PgBouncer, the object store, Qdrant, Keycloak, the GPU
tier's endpoints and the OTLP collector — is restricted per VM by `fleet.sh --allow`,
which builds a firewall chain per TAP with a default DROP. What reaches *in* is the host's
load balancer, on the API port and nothing else: no SSH, and no port published beyond the
TAP network.

## Several VMs on one host (fleet.sh)

```sh
# four copies, each with its own scratch disk, reachable only where it needs to be
./fleet.sh up --count 4 --kernel vmlinux --rootfs rootfs.ext4 \
    --scratch-template scratch.ext4 --mmds-env secrets.env \
    --allow pg.internal:6432 --allow s3.internal:443 --allow qdrant.internal:6334 \
    --allow sso.internal:443 --allow doc-parse.gpu.internal:8000

./fleet.sh status                 # id, guest address, document-slot load
./fleet.sh endpoints              # 172.16.1.2:8080 … — feed this to the load balancer
./fleet.sh scale --count 6
./fleet.sh autoscale --min 2 --max 8 --target-load 0.7 &
./fleet.sh down
```

`--dry-run` prints every command it would run and starts nothing, which is how to read
what it does on a host without `/dev/kvm`.

## Driving the API by hand

For a strict ordering (MMDS loaded before the guest boots) start Firecracker without
`--config-file` and send the pieces of `vm-config.example.json` to the API socket:

```sh
firecracker --api-sock /tmp/fc.sock &
curl --unix-socket /tmp/fc.sock -X PUT http://localhost/boot-source   -d @<(jq '."boot-source"' vm-config.json)
curl --unix-socket /tmp/fc.sock -X PUT http://localhost/drives/rootfs -d @<(jq '.drives[0]' vm-config.json)
curl --unix-socket /tmp/fc.sock -X PUT http://localhost/network-interfaces/eth0 -d @<(jq '."network-interfaces"[0]' vm-config.json)
curl --unix-socket /tmp/fc.sock -X PUT http://localhost/mmds/config    -d @<(jq '."mmds-config"' vm-config.json)
curl --unix-socket /tmp/fc.sock -X PUT http://localhost/mmds           -d '{"ds":{"env":"DS__POSTGRES__PASSWORD=..."}}'
curl --unix-socket /tmp/fc.sock -X PUT http://localhost/actions        -d '{"action_type":"InstanceStart"}'
```

Snapshots and per-VM egress filtering are `fleet.sh`'s job: `fleet.sh snapshot --id N`
pauses, writes `snapshot-N.vmstate` and `snapshot-N.mem`, and resumes. A restored snapshot
needs its MMDS re-PUT before it resumes — its lease holder id comes from there, and two
restores sharing one id would each believe they held the other's lease on a document
(SPEC.md).
