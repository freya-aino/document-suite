# deploy/firecracker — build and boot a vs-worker microVM

One microVM runs one `vs-worker` process in one role (`ingest`, `page`, `doc`).
Design and sizing: [SPEC.md](SPEC.md). Files here:

| File | Purpose |
|------|---------|
| `build-rootfs.sh` | builds `rootfs.ext4` from the worker binary, pdfium, CA bundle and config |
| `init.sh` | the guest's `/sbin/init`: mounts, scratch disk, network, MMDS secrets, starts the worker |
| `vm-config.example.json` | Firecracker config: kernel args, drives, TAP interface, MMDS |
| `firecracker-run.sh` | local wrapper: TAP device + NAT, config substitution, MMDS secrets, boot |
| `devenv.nix` | `devenv --profile firecracker shell` with firecracker and e2fsprogs |

## 1. Build the pieces

```sh
# glibc build of the worker (the rootfs ships its shared-library closure)
cargo build --release -p vs-worker --target x86_64-unknown-linux-gnu

# a guest kernel: Firecracker's CI kernels work out of the box
curl -fLo vmlinux \
  https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/x86_64/vmlinux-6.1.102

# the root filesystem
deploy/firecracker/build-rootfs.sh \
  --worker target/x86_64-unknown-linux-gnu/release/vs-worker \
  --pdfium "$PDFIUM_DIR/libpdfium.so" \
  --ca-bundle /etc/ssl/certs/ca-certificates.crt \
  --config config/worker.toml \
  --models config/models.toml \
  --size 512M --out rootfs.ext4

# a scratch disk for the ingest role (max_upload_bytes plus headroom; see SPEC.md)
truncate -s 4G scratch-1.ext4 && mkfs.ext4 -q -F scratch-1.ext4
```

`build-rootfs.sh` copies the libraries `ldd` resolves for the worker and pdfium. For a
Nix-built worker pass `--nix-closure` and the `/nix/store` runtime closure is copied
instead (`nix path-info -r`). `busybox` is taken from `PATH` or `--busybox`; a static
busybox keeps the image small. The build needs no root: `mkfs.ext4 -d` populates the
image from a directory.

The baked `worker.toml` holds everything that is not secret (NATS, object store,
Postgres URL without password, Helix URL, model registry path). Secrets arrive via MMDS.

## 2. Boot one VM locally

```sh
sudo deploy/firecracker/firecracker-run.sh --role page --id 1 \
  --kernel vmlinux --rootfs rootfs.ext4 --mmds-env secrets.env

# ingest role with its scratch disk
sudo deploy/firecracker/firecracker-run.sh --role ingest --id 2 --vcpus 2 --mem 2048 \
  --kernel vmlinux --rootfs rootfs.ext4 --scratch scratch-1.ext4 --mmds-env secrets.env
```

`secrets.env` is one `VS__SECTION__KEY=value` per line, e.g.

```
VS__POSTGRES__PASSWORD=...
VS__BLOB__ACCESS_KEY_ID=...
VS__BLOB__SECRET_ACCESS_KEY=...
```

The wrapper creates `tap-vs-<id>` (host `172.16.<id>.1/30`, guest `.2`), NATs egress
through the default route, substitutes kernel/rootfs/TAP/sizes into a copy of
`vm-config.example.json`, boots, and PUTs the secrets into MMDS as
`{"vs": {"env": "<file contents>"}}`. The guest console is the terminal; the VM
powers off when the worker exits (`reboot=k panic=1` makes Firecracker exit).

## 3. What the guest does (init.sh)

1. mounts `/proc`, `/sys`, `/dev`, `/run`, `/tmp`; mounts (or formats and mounts) the
   second block device at `/scratch`;
2. reads `vs.role=` from the kernel command line (a bare `VS_ROLE=…` boot argument also
   works: the kernel exports unknown `KEY=VALUE` arguments to init's environment);
3. configures `eth0` from `vs.ip=`/`vs.gw=`/`vs.dns=` or the kernel's `ip=` argument;
4. fetches a MMDS V2 session token and `GET /vs/env`, exports every well-formed
   `VS__NAME=value` line (nothing is sourced as shell);
5. runs `/usr/bin/vs-worker --role $ROLE` with `VS_CONFIG=/etc/vs/worker.toml` and
   `TMPDIR=/scratch`, forwards `SIGTERM`, reaps orphaned children (the ingest role's
   render processes), and powers off when the worker exits.

Everything the worker needs is reachable from the TAP network only: NATS, PgBouncer,
object store, HelixDB, the GPU tier's endpoints and the OTLP collector. No SSH, no
inbound ports.

## Driving the API by hand

For a strict ordering (MMDS loaded before the guest boots) start Firecracker without
`--config-file` and send the pieces of `vm-config.example.json` to the API socket:

```sh
firecracker --api-sock /tmp/fc.sock &
curl --unix-socket /tmp/fc.sock -X PUT http://localhost/boot-source   -d @<(jq '."boot-source"' vm-config.json)
curl --unix-socket /tmp/fc.sock -X PUT http://localhost/drives/rootfs -d @<(jq '.drives[0]' vm-config.json)
curl --unix-socket /tmp/fc.sock -X PUT http://localhost/network-interfaces/eth0 -d @<(jq '."network-interfaces"[0]' vm-config.json)
curl --unix-socket /tmp/fc.sock -X PUT http://localhost/mmds/config    -d @<(jq '."mmds-config"' vm-config.json)
curl --unix-socket /tmp/fc.sock -X PUT http://localhost/mmds           -d '{"vs":{"env":"VS__POSTGRES__PASSWORD=..."}}'
curl --unix-socket /tmp/fc.sock -X PUT http://localhost/actions        -d '{"action_type":"InstanceStart"}'
```

Snapshots (`/snapshot/create` after the worker is idle, `/snapshot/load` for warm
starts) and per-VM egress filtering are the host agent's job (SPEC.md "Fleet management").
