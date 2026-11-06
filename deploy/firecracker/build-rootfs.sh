#!/usr/bin/env bash
# Builds the read-only root filesystem of the vs-worker microVM (deploy/firecracker/SPEC.md).
#
#   build-rootfs.sh --worker target/release/vs-worker \
#                   --pdfium /path/to/libpdfium.so \
#                   --ca-bundle /etc/ssl/certs/ca-certificates.crt \
#                   --config config/worker.toml \
#                   [--models config/models.toml] \
#                   [--busybox "$(command -v busybox)"] \
#                   [--nix-closure] [--size 512M] [--out rootfs.ext4]
#
# The image holds: /sbin/init (deploy/firecracker/init.sh), busybox and the applets init
# uses, /usr/bin/vs-worker with its shared-library closure (glibc build; a static musl
# binary cannot dlopen libpdfium.so), libpdfium.so, the CA bundle and the baked config.
# The library closure comes from `ldd` by default, or from the Nix store
# (`nix path-info -r`) with --nix-closure when the binary is a Nix build.
#
# Runs as a normal user when possible: the tree is assembled in a temp dir and written
# into the image with `mkfs.ext4 -d` (e2fsprogs >= 1.43), no loop mount, no root.
# Needs: bash, mkfs.ext4, ldd (or nix), busybox (static build preferred), cp, install.
#
# Secrets are never baked: they arrive through MMDS at boot (see init.sh).
set -euo pipefail

usage() { sed -n '2,20p' "$0" >&2; exit 2; }

WORKER=
PDFIUM=
CA_BUNDLE=
CONFIG=
MODELS=
BUSYBOX=${BUSYBOX:-$(command -v busybox || true)}
SIZE=512M
OUT=rootfs.ext4
NIX_CLOSURE=0
INIT_SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/init.sh"

while [ $# -gt 0 ]; do
    case "$1" in
        --worker) WORKER=$2; shift 2 ;;
        --pdfium) PDFIUM=$2; shift 2 ;;
        --ca-bundle) CA_BUNDLE=$2; shift 2 ;;
        --config) CONFIG=$2; shift 2 ;;
        --models) MODELS=$2; shift 2 ;;
        --busybox) BUSYBOX=$2; shift 2 ;;
        --size) SIZE=$2; shift 2 ;;
        --out) OUT=$2; shift 2 ;;
        --nix-closure) NIX_CLOSURE=1; shift ;;
        -h|--help) usage ;;
        *) echo "unknown argument: $1" >&2; usage ;;
    esac
done

for var in WORKER PDFIUM CA_BUNDLE CONFIG; do
    if [ -z "${!var}" ]; then
        echo "missing --$(echo "$var" | tr '[:upper:]_' '[:lower:]-')" >&2
        usage
    fi
    if [ ! -r "${!var}" ]; then
        echo "cannot read ${!var}" >&2
        exit 1
    fi
done
if [ -z "$BUSYBOX" ] || [ ! -x "$BUSYBOX" ]; then
    echo "busybox not found; pass --busybox (init.sh runs under busybox sh)" >&2
    exit 1
fi
command -v mkfs.ext4 >/dev/null || { echo "mkfs.ext4 (e2fsprogs) not found" >&2; exit 1; }

ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT

# ── Directory skeleton ───────────────────────────────────────────────────────
mkdir -p "$ROOT"/{bin,sbin,usr/bin,usr/lib,lib,lib64,etc/vs,etc/ssl/certs,proc,sys,dev,run,tmp,scratch,root}
chmod 1777 "$ROOT/tmp"

# ── Worker, pdfium, config, certificates ─────────────────────────────────────
install -m 0755 "$WORKER" "$ROOT/usr/bin/vs-worker"
install -m 0755 "$PDFIUM" "$ROOT/usr/lib/libpdfium.so"
install -m 0644 "$CONFIG" "$ROOT/etc/vs/worker.toml"
if [ -n "$MODELS" ]; then
    install -m 0644 "$MODELS" "$ROOT/etc/vs/models.toml"
fi
install -m 0644 "$CA_BUNDLE" "$ROOT/etc/ssl/certs/ca-certificates.crt"
ln -s ca-certificates.crt "$ROOT/etc/ssl/certs/ca-bundle.crt"
install -m 0755 "$INIT_SRC" "$ROOT/sbin/init"

# ── Shared-library closure ───────────────────────────────────────────────────
# copy_lib SRC: copies a library (following symlinks) to the same path inside the image
# and keeps a symlink at the original name when it was one.
copy_lib() {
    local src=$1 real dest
    real=$(readlink -f "$src")
    dest="$ROOT$real"
    if [ ! -e "$dest" ]; then
        mkdir -p "$(dirname "$dest")"
        cp -p "$real" "$dest"
    fi
    if [ "$real" != "$src" ] && [ ! -e "$ROOT$src" ]; then
        mkdir -p "$(dirname "$ROOT$src")"
        ln -s "$real" "$ROOT$src"
    fi
}

# ldd_closure BINARY...: every library ldd resolves for the given files, plus the loader.
ldd_closure() {
    local f line path
    for f in "$@"; do
        # Lines look like "libm.so.6 => /lib/x86_64-linux-gnu/libm.so.6 (0x...)" or
        # "/lib64/ld-linux-x86-64.so.2 (0x...)"; "statically linked" and "not found" are skipped.
        while IFS= read -r line; do
            case "$line" in
                *"not found"*) echo "unresolved dependency of $f: $line" >&2; exit 1 ;;
                *"=>"*) path=${line#*=> }; path=${path%% (*} ;;
                /*) path=${line%% (*} ;;
                *) continue ;;
            esac
            [ -n "$path" ] && [ -e "$path" ] && echo "$path"
        done < <(ldd "$f" 2>/dev/null || true)
    done | sort -u
}

if [ "$NIX_CLOSURE" = 1 ]; then
    command -v nix >/dev/null || { echo "--nix-closure needs nix" >&2; exit 1; }
    # The runtime closure of the binary and of pdfium (all /nix/store paths they reference).
    while IFS= read -r store_path; do
        [ -n "$store_path" ] || continue
        mkdir -p "$ROOT$(dirname "$store_path")"
        cp -a "$store_path" "$ROOT$store_path"
    done < <(nix path-info -r "$(readlink -f "$WORKER")" "$(readlink -f "$PDFIUM")")
    # The binary's interpreter is a store path too; make it reachable where ldd expects it.
    interp=$(ldd "$WORKER" | awk '/ld-linux/ {print $1; exit}')
    if [ -n "$interp" ] && [ ! -e "$ROOT$interp" ]; then
        mkdir -p "$(dirname "$ROOT$interp")"
        ln -s "$(readlink -f "$interp")" "$ROOT$interp"
    fi
else
    while IFS= read -r lib; do
        copy_lib "$lib"
    done < <(ldd_closure "$WORKER" "$PDFIUM")
    # pdfium is dlopen'ed from the working directory or LD_LIBRARY_PATH; make it findable.
    echo "/usr/lib" > "$ROOT/etc/ld.so.conf"
fi

# ── busybox and the applets init.sh uses ─────────────────────────────────────
install -m 0755 "$BUSYBOX" "$ROOT/bin/busybox"
if ! file -b "$BUSYBOX" 2>/dev/null | grep -q 'statically linked'; then
    # Dynamically linked busybox: its libraries have to be in the image as well.
    while IFS= read -r lib; do
        copy_lib "$lib"
    done < <(ldd_closure "$BUSYBOX")
fi
for applet in sh mount umount mkdir chmod cat cut tr printf echo rm sync kill sleep hostname \
              ip wget mkfs.ext4 poweroff reboot ls ps; do
    case "$applet" in
        poweroff|reboot|mkfs.ext4) ln -sf ../bin/busybox "$ROOT/sbin/$applet" ;;
        *) ln -sf busybox "$ROOT/bin/$applet" ;;
    esac
done

# ── /etc ─────────────────────────────────────────────────────────────────────
cat > "$ROOT/etc/passwd" <<'PASSWD'
root:x:0:0:root:/root:/bin/sh
nobody:x:65534:65534:nobody:/:/bin/sh
PASSWD
cat > "$ROOT/etc/group" <<'GROUP'
root:x:0:
nogroup:x:65534:
GROUP
echo "vs-worker" > "$ROOT/etc/hostname"
printf '127.0.0.1 localhost\n' > "$ROOT/etc/hosts"
# DNS: replaced by vs.dns= on the kernel command line, if given.
printf 'nameserver 10.0.0.1\n' > "$ROOT/etc/resolv.conf"
cat > "$ROOT/etc/nsswitch.conf" <<'NSS'
hosts: files dns
NSS

# ── Image ────────────────────────────────────────────────────────────────────
rm -f "$OUT"
# -d populates the filesystem from the directory without mounting (no root needed);
# -E root_owner makes everything root-owned regardless of who runs the build.
mkfs.ext4 -q -F -L vs-rootfs -E root_owner=0:0 -d "$ROOT" "$OUT" "$SIZE"
echo "wrote $OUT ($SIZE) from $(du -sh "$ROOT" | cut -f1) of files"
