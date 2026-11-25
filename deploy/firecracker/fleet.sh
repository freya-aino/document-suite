#!/usr/bin/env bash
# The host agent: several service microVMs on one host, and the autoscaler over them.
#
#   fleet.sh up        --count N [boot options]   boot the fleet
#   fleet.sh scale     --count N [boot options]   grow or shrink to N
#   fleet.sh status                               what is running, and each VM's load
#   fleet.sh down                                 stop everything, release the TAPs
#   fleet.sh endpoints                            one guest address per line, for a load balancer
#   fleet.sh autoscale --min N --max M [--interval S] [boot options]
#   fleet.sh snapshot  --id N                     pause one VM and write a snapshot
#
# **Why this exists, and why it did not before.** This tier was specified for queue
# consumers: a microVM takes no inbound connections, so work had to be pushed to it, and
# when decision D13 removed the task bus there was no way to give a VM a document — the
# tier was marked superseded. What changed is that the suite became one service with an
# HTTP API (D12). A VM's guest address *is* reachable from the host across its own TAP
# device, so a load balancer on the host can hand it uploads, and the tier is a deployment
# path again. `endpoints` is what a load balancer's configuration is generated from.
#
# Host prerequisites: firecracker, jq, curl, iproute2, and either root or CAP_NET_ADMIN
# for the TAP devices. `--dry-run` prints every command it would run and touches nothing,
# which is how the logic here is exercised without /dev/kvm.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
STATE_DIR=${DS_FLEET_STATE:-${XDG_RUNTIME_DIR:-/tmp}/ds-fleet}
DRY_RUN=0

# Boot options, passed through to firecracker-run.sh.
KERNEL=vmlinux
ROOTFS=rootfs.ext4
SCRATCH_TEMPLATE=
VCPUS=
MEM=
MMDS_ENV=
API_PORT=8080
# Hosts a guest may talk to, as host:port. Everything else is dropped on the way out, so
# a compromised page parser cannot reach the rest of the network — the egress allow-list
# the spec asks for, applied per TAP.
ALLOW=()

# Autoscaler bounds and the signal it scales on.
MIN=1
MAX=4
INTERVAL=30
# Scale up when the mean fraction of document slots in use across the fleet is above
# this, down when it is below half of it. Document slots rather than CPU: the suite's
# own `worker.max_documents` is what bounds it, and a copy at its limit is the thing a
# new copy relieves.
TARGET_LOAD=0.7

usage() {
    sed -n '2,20p' "$HERE/fleet.sh" >&2
    exit "${1:-2}"
}

log() { printf '%s fleet: %s\n' "$(date -u +%H:%M:%S)" "$*" >&2; }

# run CMD...: execute, or print under --dry-run.
run() {
    if [ "$DRY_RUN" = 1 ]; then
        printf '+ %s\n' "$*"
        return 0
    fi
    "$@"
}

CMD=${1:-}
[ -n "$CMD" ] || usage
shift || true

while [ $# -gt 0 ]; do
    case "$1" in
        --count) COUNT=$2; shift 2 ;;
        --id) ONE_ID=$2; shift 2 ;;
        --kernel) KERNEL=$2; shift 2 ;;
        --rootfs) ROOTFS=$2; shift 2 ;;
        --scratch-template) SCRATCH_TEMPLATE=$2; shift 2 ;;
        --vcpus) VCPUS=$2; shift 2 ;;
        --mem) MEM=$2; shift 2 ;;
        --mmds-env) MMDS_ENV=$2; shift 2 ;;
        --api-port) API_PORT=$2; shift 2 ;;
        --allow) ALLOW+=("$2"); shift 2 ;;
        --min) MIN=$2; shift 2 ;;
        --max) MAX=$2; shift 2 ;;
        --interval) INTERVAL=$2; shift 2 ;;
        --target-load) TARGET_LOAD=$2; shift 2 ;;
        --state) STATE_DIR=$2; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) usage 0 ;;
        *) echo "unknown argument: $1" >&2; usage ;;
    esac
done

mkdir -p "$STATE_DIR"

guest_ip() { echo "172.16.$1.2"; }
pid_file() { echo "$STATE_DIR/vm-$1.pid"; }

# Ids of the VMs this agent believes are running, ascending.
running_ids() {
    local f id
    for f in "$STATE_DIR"/vm-*.pid; do
        [ -e "$f" ] || continue
        id=${f##*/vm-}
        id=${id%.pid}
        if alive "$id"; then
            echo "$id"
        else
            # A VM that died on its own: forget it, so `scale` replaces it rather than
            # counting it.
            log "vm $id is gone; releasing its slot"
            rm -f "$f"
        fi
    done | sort -n
}

alive() {
    local pid
    pid=$(cat "$(pid_file "$1")" 2>/dev/null) || return 1
    # Under --dry-run nothing was started, and pretending otherwise would make the
    # output of a dry run say the opposite of what a real run does.
    [ "$pid" = dry-run ] && return 0
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null
}

# ── egress allow-list ─────────────────────────────────────────────────────────
#
# One chain per TAP. The default is DROP, so a guest reaches Postgres, the object store
# and the model endpoints and nothing else — including the other guests, which is the
# isolation the spec claims and nothing enforced. Resolved to addresses once, here,
# because a per-guest DNS resolver would be another thing to allow.
apply_egress() {
    local id=$1 tap=tap-ds-$1 ip
    ip=$(guest_ip "$id")
    if [ ${#ALLOW[@]} -eq 0 ]; then
        log "vm $id: no --allow given, egress is unrestricted (NAT only)"
        return 0
    fi
    local chain=DS_EGRESS_$id
    run iptables -N "$chain" 2>/dev/null || true
    run iptables -F "$chain"
    local entry host port addrs addr
    for entry in "${ALLOW[@]}"; do
        host=${entry%:*}
        port=${entry##*:}
        addrs=$(getent ahostsv4 "$host" 2>/dev/null | awk '{print $1}' | sort -u)
        [ -n "$addrs" ] || { log "vm $id: cannot resolve $host, skipping"; continue; }
        for addr in $addrs; do
            run iptables -A "$chain" -p tcp -d "$addr" --dport "$port" -j ACCEPT
        done
    done
    # Replies to connections the guest opened.
    run iptables -A "$chain" -m state --state RELATED,ESTABLISHED -j ACCEPT
    run iptables -A "$chain" -j DROP
    # Everything the guest sends, except to the host itself — the host is its gateway and
    # its load balancer, and the API port has to stay reachable in both directions.
    #
    # Matched on the interface *and* the address. On the address alone a guest could send
    # with a neighbour's source address and be judged by the neighbour's chain, which for
    # a rule whose whole purpose is to keep the guests apart is the wrong way round. A
    # guest cannot forge the device it is attached to.
    run iptables -C FORWARD -i "$tap" -s "$ip" -j "$chain" 2>/dev/null \
        || run iptables -I FORWARD 1 -i "$tap" -s "$ip" -j "$chain"
    log "vm $id: egress restricted to ${ALLOW[*]}"
}

clear_egress() {
    local id=$1 chain=DS_EGRESS_$1 tap=tap-ds-$1 ip
    ip=$(guest_ip "$id")
    # The same match as `apply_egress` installs, or the rule is not the one deleted.
    run iptables -D FORWARD -i "$tap" -s "$ip" -j "$chain" 2>/dev/null || true
    run iptables -F "$chain" 2>/dev/null || true
    run iptables -X "$chain" 2>/dev/null || true
}

# ── one VM ────────────────────────────────────────────────────────────────────

# The MMDS secrets for one VM: the caller's file, plus this VM's own lease holder id.
#
# That id matters more than it looks. The suite leases each document to the process
# working on it, and two processes claiming the same holder id would each believe they
# hold the other's lease. A holder id is random per process, which is right for containers
# — and wrong for a restored **snapshot**, which is a copy of a process that already had
# one. Giving each VM its id from MMDS makes it a property of the slot rather than of the
# process image.
mmds_for() {
    local id=$1 out=$STATE_DIR/mmds-$1.env
    : > "$out"
    [ -n "$MMDS_ENV" ] && cat "$MMDS_ENV" >> "$out"
    {
        echo "DS_HOLDER_ID=fc-$(hostname -s 2>/dev/null || echo host)-$id"
        echo "DS_API_ADDR=0.0.0.0:$API_PORT"
    } >> "$out"
    echo "$out"
}

start_vm() {
    local id=$1 args scratch
    args=(--id "$id" --kernel "$KERNEL" --rootfs "$ROOTFS")
    [ -n "$VCPUS" ] && args+=(--vcpus "$VCPUS")
    [ -n "$MEM" ] && args+=(--mem "$MEM")
    if [ -n "$SCRATCH_TEMPLATE" ]; then
        # One scratch disk per VM, copied from the template: two VMs sharing a writable
        # disk would corrupt each other's spooled originals.
        scratch=$STATE_DIR/scratch-$id.ext4
        [ -f "$scratch" ] || run cp "$SCRATCH_TEMPLATE" "$scratch"
        args+=(--scratch "$scratch")
    fi
    args+=(--mmds-env "$(mmds_for "$id")")

    log "starting vm $id"
    if [ "$DRY_RUN" = 1 ]; then
        printf '+ %s\n' "$HERE/firecracker-run.sh ${args[*]}"
        echo "dry-run" > "$(pid_file "$id")"
    else
        "$HERE/firecracker-run.sh" "${args[@]}" >"$STATE_DIR/vm-$id.log" 2>&1 &
        echo $! > "$(pid_file "$id")"
    fi
    apply_egress "$id"
}

stop_vm() {
    local id=$1 pid
    pid=$(cat "$(pid_file "$id")" 2>/dev/null) || true
    log "stopping vm $id"
    if [ -n "${pid:-}" ] && [ "$pid" != dry-run ]; then
        # SIGTERM reaches init.sh, which forwards it to the service: the API stops
        # accepting and in-flight documents get their shutdown grace. A document still
        # running when the grace elapses is abandoned and resumed by another copy — that
        # is what the lease is for.
        run kill -TERM "$pid" 2>/dev/null || true
        local waited=0
        while [ "$DRY_RUN" = 0 ] && kill -0 "$pid" 2>/dev/null && [ "$waited" -lt 90 ]; do
            sleep 1
            waited=$((waited + 1))
        done
        run kill -KILL "$pid" 2>/dev/null || true
    fi
    clear_egress "$id"
    rm -f "$(pid_file "$id")"
}

# Fraction of this VM's document slots in use, from its own metrics endpoint, or empty
# when it cannot be read.
vm_load() {
    local id=$1 ip body in_flight limit
    ip=$(guest_ip "$id")
    body=$(curl -sS --max-time 2 "http://$ip:9100/metrics" 2>/dev/null) || return 1
    in_flight=$(awk '/^worker_stage_in_flight\{.*stage="doc"/ {sum += $NF} END {print sum + 0}' <<<"$body")
    limit=$(awk '/^suite_max_documents/ {print $NF; exit}' <<<"$body")
    [ -n "$limit" ] && [ "$limit" != 0 ] || limit=4
    awk -v a="$in_flight" -v b="$limit" 'BEGIN { printf "%.3f", (b > 0 ? a / b : 0) }'
}

scale_to() {
    local want=$1 have ids id
    ids=$(running_ids)
    have=$(printf '%s\n' "$ids" | grep -c . || true)
    if [ "$want" -gt "$have" ]; then
        # Lowest free ids, so a TAP device and a guest address belong to one slot for as
        # long as the fleet lives.
        for id in $(seq 1 "$MAX"); do
            [ "$(printf '%s\n' "$ids" | grep -c .)" -ge "$want" ] && break
            if ! alive "$id"; then
                start_vm "$id"
                ids=$(printf '%s\n%s\n' "$ids" "$id" | grep . | sort -n)
            fi
        done
    elif [ "$want" -lt "$have" ]; then
        # Highest ids first: the newest copies are the least likely to be mid-document.
        for id in $(printf '%s\n' "$ids" | sort -rn | head -n "$((have - want))"); do
            stop_vm "$id"
        done
    fi
    log "fleet at $(running_ids | grep -c . || true) vm(s)"
}

case "$CMD" in
    up)
        scale_to "${COUNT:-$MIN}"
        ;;
    scale)
        [ -n "${COUNT:-}" ] || { echo "scale needs --count" >&2; exit 2; }
        scale_to "$COUNT"
        ;;
    down)
        for id in $(running_ids); do stop_vm "$id"; done
        ;;
    status)
        printf '%-4s %-16s %-8s %s\n' ID GUEST STATE LOAD
        for id in $(running_ids); do
            printf '%-4s %-16s %-8s %s\n' "$id" "$(guest_ip "$id"):$API_PORT" running "$(vm_load "$id" || echo '-')"
        done
        ;;
    endpoints)
        for id in $(running_ids); do echo "$(guest_ip "$id"):$API_PORT"; done
        ;;
    snapshot)
        # Pause, snapshot, resume. Firecracker's snapshot is a *running process* frozen:
        # restoring it gives a copy whose database connections and HTTP pools are dead
        # sockets, which the suite reconnects on its own, and whose lease holder id is
        # whatever the snapshotted process had — which is why that id comes from MMDS per
        # slot (see `mmds_for`) and must be re-PUT before a restored VM resumes.
        [ -n "${ONE_ID:-}" ] || { echo "snapshot needs --id" >&2; exit 2; }
        sock=${XDG_RUNTIME_DIR:-/tmp}/ds-fc-$ONE_ID/firecracker.sock
        out=$STATE_DIR/snapshot-$ONE_ID
        run curl -sS --unix-socket "$sock" -X PATCH -H 'Content-Type: application/json' \
            -d '{"state":"Paused"}' http://localhost/vm
        run curl -sS --unix-socket "$sock" -X PUT -H 'Content-Type: application/json' \
            -d "{\"snapshot_type\":\"Full\",\"snapshot_path\":\"$out.vmstate\",\"mem_file_path\":\"$out.mem\"}" \
            http://localhost/snapshot/create
        run curl -sS --unix-socket "$sock" -X PATCH -H 'Content-Type: application/json' \
            -d '{"state":"Resumed"}' http://localhost/vm
        log "snapshot written to $out.vmstate / $out.mem"
        ;;
    autoscale)
        log "autoscaling between $MIN and $MAX every ${INTERVAL}s at target load $TARGET_LOAD"
        scale_to "$MIN"
        while true; do
            sleep "$INTERVAL"
            total=0
            n=0
            for id in $(running_ids); do
                load=$(vm_load "$id") || continue
                total=$(awk -v t="$total" -v l="$load" 'BEGIN { print t + l }')
                n=$((n + 1))
            done
            [ "$n" -gt 0 ] || { scale_to "$MIN"; continue; }
            mean=$(awk -v t="$total" -v n="$n" 'BEGIN { printf "%.3f", t / n }')
            # One copy at a time, in either direction: the fleet should settle rather than
            # oscillate, and a copy takes seconds to become useful and up to the shutdown
            # grace to retire.
            want=$n
            if awk -v m="$mean" -v t="$TARGET_LOAD" 'BEGIN { exit !(m > t) }'; then
                want=$((n + 1))
            elif awk -v m="$mean" -v t="$TARGET_LOAD" 'BEGIN { exit !(m < t / 2) }'; then
                want=$((n - 1))
            fi
            [ "$want" -lt "$MIN" ] && want=$MIN
            [ "$want" -gt "$MAX" ] && want=$MAX
            if [ "$want" != "$n" ]; then
                log "mean load $mean over $n vm(s): scaling to $want"
                scale_to "$want"
            fi
        done
        ;;
    *)
        echo "unknown command: $CMD" >&2
        usage
        ;;
esac
