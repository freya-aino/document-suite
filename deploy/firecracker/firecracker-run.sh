#!/usr/bin/env bash
# Starts one vs-worker microVM from vm-config.example.json with a TAP device.
#
#   firecracker-run.sh [--role page] [--id 1] [--kernel vmlinux] [--rootfs rootfs.ext4]
#                      [--scratch scratch.ext4] [--vcpus 1] [--mem 1024]
#                      [--mmds-env secrets.env] [--config vm-config.example.json]
#
# Host prerequisites (root or CAP_NET_ADMIN for the TAP setup): firecracker on PATH,
# /dev/kvm, jq, iproute2. The TAP device tap-vs-<id> gets 172.16.<id>.1/30 on the host
# and the guest 172.16.<id>.2/30; egress is NATed through the default interface.
# For production the host agent owns networking and egress filtering (SPEC.md).
set -euo pipefail

ROLE=page
ID=1
KERNEL=vmlinux
ROOTFS=rootfs.ext4
SCRATCH=
VCPUS=
MEM=
MMDS_ENV=
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
CONFIG=$HERE/vm-config.example.json

while [ $# -gt 0 ]; do
    case "$1" in
        --role) ROLE=$2; shift 2 ;;
        --id) ID=$2; shift 2 ;;
        --kernel) KERNEL=$2; shift 2 ;;
        --rootfs) ROOTFS=$2; shift 2 ;;
        --scratch) SCRATCH=$2; shift 2 ;;
        --vcpus) VCPUS=$2; shift 2 ;;
        --mem) MEM=$2; shift 2 ;;
        --mmds-env) MMDS_ENV=$2; shift 2 ;;
        --config) CONFIG=$2; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
for f in "$KERNEL" "$ROOTFS" "$CONFIG"; do
    [ -r "$f" ] || { echo "cannot read $f" >&2; exit 1; }
done
command -v firecracker >/dev/null || { echo "firecracker not on PATH" >&2; exit 1; }
command -v jq >/dev/null || { echo "jq not found" >&2; exit 1; }

TAP=tap-vs-$ID
HOST_IP=172.16.$ID.1
GUEST_IP=172.16.$ID.2
MAC=$(printf 'AA:FC:00:00:%02X:%02X' $((ID / 256 % 256)) $((ID % 256)))
RUN_DIR=${XDG_RUNTIME_DIR:-/tmp}/vs-fc-$ID
API_SOCK=$RUN_DIR/firecracker.sock
mkdir -p "$RUN_DIR"
rm -f "$API_SOCK"

# ── TAP device ────────────────────────────────────────────────────────────────
if ! ip link show "$TAP" >/dev/null 2>&1; then
    ip tuntap add dev "$TAP" mode tap user "$(id -un)"
    ip addr add "$HOST_IP/30" dev "$TAP"
    ip link set "$TAP" up
    EGRESS=$(ip route show default | awk '{print $5; exit}')
    if [ -n "$EGRESS" ]; then
        sysctl -qw net.ipv4.ip_forward=1
        iptables -t nat -C POSTROUTING -s "$GUEST_IP/32" -o "$EGRESS" -j MASQUERADE 2>/dev/null \
            || iptables -t nat -A POSTROUTING -s "$GUEST_IP/32" -o "$EGRESS" -j MASQUERADE
        iptables -C FORWARD -i "$TAP" -o "$EGRESS" -j ACCEPT 2>/dev/null \
            || iptables -A FORWARD -i "$TAP" -o "$EGRESS" -j ACCEPT
        iptables -C FORWARD -i "$EGRESS" -o "$TAP" -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null \
            || iptables -A FORWARD -i "$EGRESS" -o "$TAP" -m state --state RELATED,ESTABLISHED -j ACCEPT
    fi
fi

# ── VM config from the example ────────────────────────────────────────────────
# The boot args carry the role and the guest address for init.sh; the drives, TAP and
# sizes are substituted into a copy of the example config.
BOOT_ARGS="console=ttyS0 reboot=k panic=1 pci=off ro vs.role=$ROLE vs.ip=$GUEST_IP/30 vs.gw=$HOST_IP vs.dns=$HOST_IP"
VM_JSON=$RUN_DIR/vm-config.json
jq --arg kernel "$(readlink -f "$KERNEL")" \
   --arg rootfs "$(readlink -f "$ROOTFS")" \
   --arg boot "$BOOT_ARGS" \
   --arg tap "$TAP" --arg mac "$MAC" \
   --argjson vcpus "${VCPUS:-null}" --argjson mem "${MEM:-null}" \
   '."boot-source".kernel_image_path = $kernel
    | ."boot-source".boot_args = $boot
    | .drives |= map(if .drive_id == "rootfs" then .path_on_host = $rootfs else . end)
    | ."network-interfaces"[0].host_dev_name = $tap
    | ."network-interfaces"[0].guest_mac = $mac
    | (if $vcpus != null then ."machine-config".vcpu_count = $vcpus else . end)
    | (if $mem != null then ."machine-config".mem_size_mib = $mem else . end)' \
   "$CONFIG" > "$VM_JSON"
if [ -n "$SCRATCH" ]; then
    [ -r "$SCRATCH" ] || { echo "cannot read $SCRATCH" >&2; exit 1; }
    jq --arg scratch "$(readlink -f "$SCRATCH")" \
       '.drives |= (map(select(.drive_id != "scratch")) + [{drive_id: "scratch", path_on_host: $scratch, is_root_device: false, is_read_only: false}])' \
       "$VM_JSON" > "$VM_JSON.tmp" && mv "$VM_JSON.tmp" "$VM_JSON"
else
    jq '.drives |= map(select(.drive_id != "scratch"))' "$VM_JSON" > "$VM_JSON.tmp" && mv "$VM_JSON.tmp" "$VM_JSON"
fi

# ── Start, then load MMDS secrets before the guest reads them ─────────────────
# `--config-file` boots immediately; MMDS content is PUT over the API socket right after.
# init.sh waits for the network first, which leaves enough time in practice; for a strict
# ordering start without --config-file and drive the API (see README.md).
firecracker --api-sock "$API_SOCK" --config-file "$VM_JSON" &
FC_PID=$!
trap 'kill "$FC_PID" 2>/dev/null || true' INT TERM

for _ in $(seq 1 50); do
    [ -S "$API_SOCK" ] && break
    sleep 0.1
done
if [ -n "$MMDS_ENV" ]; then
    # secrets.env: VS__SECTION__KEY=value per line, exported by init.sh.
    jq -Rs '{vs: {env: .}}' "$MMDS_ENV" \
        | curl -sS --unix-socket "$API_SOCK" -X PUT -H 'Content-Type: application/json' --data-binary @- http://localhost/mmds
fi

echo "microVM $ID ($ROLE) started: tap=$TAP guest=$GUEST_IP api=$API_SOCK" >&2
wait "$FC_PID"
