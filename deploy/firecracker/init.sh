#!/bin/sh
# Tiny init for the vs-worker microVM (PID 1). Installed as /sbin/init by build-rootfs.sh;
# runs under busybox sh. Keep it POSIX sh: there is no bash in the image.
#
#   1. mounts /proc, /sys, /dev (devtmpfs), /dev/pts, /tmp and /run;
#   2. mounts the scratch disk (second virtio block device, /dev/vdb) at /scratch;
#   3. configures eth0 from the kernel command line (`ip=` or `vs.ip=`/`vs.gw=`);
#   4. reads the role from `vs.role=` (fallback: the VS_ROLE environment variable,
#      which the kernel sets from a bare `VS_ROLE=...` boot argument);
#   5. fetches secrets from MMDS (169.254.169.254, V2 session token) and exports every
#      `VS__SECTION__KEY=value` line of /vs/env as an environment variable;
#   6. starts `vs-worker --role $ROLE` as a child, forwards SIGTERM, reaps zombies
#      (orphaned render children are re-parented to PID 1), and powers the VM off when
#      the worker exits (the host sees the Firecracker process end).
#
# Everything the worker needs beyond secrets comes from /etc/vs/worker.toml and
# /etc/vs/models.toml baked into the image; VS__… variables override individual keys.

set -u

log() { echo "init: $*" >&2; }

mount -t proc proc /proc 2>/dev/null || true
mount -t sysfs sysfs /sys 2>/dev/null || true
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
mkdir -p /dev/pts /dev/shm /run /tmp /scratch
mount -t devpts devpts /dev/pts 2>/dev/null || true
mount -t tmpfs tmpfs /dev/shm 2>/dev/null || true
mount -t tmpfs tmpfs /run 2>/dev/null || true
mount -t tmpfs -o size=64m tmpfs /tmp 2>/dev/null || true

# ── Kernel command line ─────────────────────────────────────────────────────
CMDLINE=$(cat /proc/cmdline 2>/dev/null || true)
cmdline_value() {
    # cmdline_value KEY → the value of KEY=... on the command line (empty if absent)
    for word in $CMDLINE; do
        case "$word" in
            "$1="*) printf '%s' "${word#"$1="}"; return 0 ;;
        esac
    done
    return 0
}

ROLE=$(cmdline_value vs.role)
[ -n "$ROLE" ] || ROLE="${VS_ROLE:-}"
if [ -z "$ROLE" ]; then
    log "no role: set vs.role=ingest|page|doc on the kernel command line"
    exec poweroff -f
fi

# ── Scratch disk (required for the ingest role, harmless otherwise) ─────────
SCRATCH_DEV=$(cmdline_value vs.scratch)
[ -n "$SCRATCH_DEV" ] || SCRATCH_DEV=/dev/vdb
if [ -b "$SCRATCH_DEV" ]; then
    if ! mount -t ext4 -o noatime,nodev,nosuid "$SCRATCH_DEV" /scratch 2>/dev/null; then
        log "formatting scratch disk $SCRATCH_DEV"
        mkfs.ext4 -q -F "$SCRATCH_DEV" && mount -t ext4 -o noatime,nodev,nosuid "$SCRATCH_DEV" /scratch
    fi
    chmod 1777 /scratch
elif [ "$ROLE" = ingest ]; then
    log "warning: ingest role without a scratch disk ($SCRATCH_DEV); originals spool to /tmp"
fi

# ── Network ─────────────────────────────────────────────────────────────────
# Either the kernel's `ip=client:server:gw:netmask:hostname:iface:off` (also handled by
# the kernel itself when built with CONFIG_IP_PNP) or our own `vs.ip=CIDR vs.gw=ADDR`.
IP_CIDR=$(cmdline_value vs.ip)
GW=$(cmdline_value vs.gw)
DNS=$(cmdline_value vs.dns)
KIP=$(cmdline_value ip)
if [ -z "$IP_CIDR" ] && [ -n "$KIP" ]; then
    IP_CIDR=$(printf '%s' "$KIP" | cut -d: -f1)
    [ -n "$GW" ] || GW=$(printf '%s' "$KIP" | cut -d: -f3)
    MASK=$(printf '%s' "$KIP" | cut -d: -f4)
    if [ -n "$MASK" ] && [ "${IP_CIDR#*/}" = "$IP_CIDR" ]; then
        # dotted netmask → prefix length
        bits=0
        for octet in $(printf '%s' "$MASK" | tr . ' '); do
            case "$octet" in
                255) bits=$((bits + 8)) ;; 254) bits=$((bits + 7)) ;; 252) bits=$((bits + 6)) ;;
                248) bits=$((bits + 5)) ;; 240) bits=$((bits + 4)) ;; 224) bits=$((bits + 3)) ;;
                192) bits=$((bits + 2)) ;; 128) bits=$((bits + 1)) ;; *) ;;
            esac
        done
        IP_CIDR="$IP_CIDR/$bits"
    fi
fi
ip link set lo up 2>/dev/null || true
ip link set eth0 up
if [ -n "$IP_CIDR" ]; then
    ip addr add "$IP_CIDR" dev eth0 2>/dev/null || true
    [ -z "$GW" ] || ip route add default via "$GW" dev eth0 2>/dev/null || true
fi
if [ -n "$DNS" ]; then
    printf 'nameserver %s\n' "$DNS" > /etc/resolv.conf
fi
# MMDS is link-local on the same interface.
ip route add 169.254.169.254/32 dev eth0 2>/dev/null || true

# ── Secrets from MMDS ───────────────────────────────────────────────────────
# Host side (Firecracker API):  PUT /mmds  {"vs": {"env": "VS__POSTGRES__PASSWORD=…\nVS__BLOB__ACCESS_KEY=…"}}
# Guest side (V2): fetch a session token, then GET /vs/env as text. Only well-formed
# `VS__NAME=value` lines are exported; nothing is sourced as shell.
MMDS=http://169.254.169.254
TOKEN=$(wget -q -O - --method=PUT --header 'X-metadata-token-ttl-seconds: 60' "$MMDS/latest/api/token" 2>/dev/null || true)
if [ -n "$TOKEN" ]; then
    ENV_FILE=/run/vs-env
    if wget -q -O "$ENV_FILE" --header "X-metadata-token: $TOKEN" "$MMDS/vs/env" 2>/dev/null; then
        while IFS= read -r line || [ -n "$line" ]; do
            case "$line" in
                VS__[A-Z0-9_]*=*) export "$line" ;;
                "") ;;
                *) log "ignoring malformed MMDS line" ;;
            esac
        done < "$ENV_FILE"
        rm -f "$ENV_FILE"
    else
        log "MMDS has no /vs/env; running with baked config only"
    fi
else
    log "MMDS unreachable; running with baked config only"
fi

# ── Worker ──────────────────────────────────────────────────────────────────
export VS_CONFIG="${VS_CONFIG:-/etc/vs/worker.toml}"
export VS_ROLE="$ROLE"
export SSL_CERT_FILE="${SSL_CERT_FILE:-/etc/ssl/certs/ca-certificates.crt}"
if [ -w /scratch ]; then export TMPDIR=/scratch; else export TMPDIR=/tmp; fi
export RUST_LOG="${RUST_LOG:-info}"
hostname "vs-$ROLE" 2>/dev/null || true

WORKER_PID=
on_term() {
    log "SIGTERM: stopping the worker"
    [ -z "$WORKER_PID" ] || kill -TERM "$WORKER_PID" 2>/dev/null
}
trap on_term TERM INT

log "starting vs-worker --role $ROLE"
/usr/bin/vs-worker --role "$ROLE" &
WORKER_PID=$!

# `wait` as PID 1 reaps every child, including orphans re-parented to us. It returns
# early when a signal arrives, so loop until the worker itself is gone.
STATUS=0
while kill -0 "$WORKER_PID" 2>/dev/null; do
    wait "$WORKER_PID"
    STATUS=$?
done
log "vs-worker exited with status $STATUS; powering off"
sync
exec poweroff -f
