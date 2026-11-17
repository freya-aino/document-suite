#!/usr/bin/env bash
# Starts the services the `--ignored` tests need: Qdrant, NATS and an S3 server.
#
# Postgres is a workflow service container instead, because it is the one of the four
# with a usable health check and no provisioning beyond a role. These three need either
# arguments the `services:` block cannot pass (NATS needs `-js`) or several setup
# commands after they are up (Garage), so they run as plain containers.
#
# Ports and the container name prefix come from the environment so this can be run on a
# developer machine that already has something on 6334 or 3900:
#
#   PREFIX=vs-ci QDRANT_GRPC=56334 NATS_PORT=54222 S3_PORT=53900 .github/scripts/services.sh
#
# It is idempotent: an existing container of the same name is replaced.
set -euo pipefail

PREFIX="${PREFIX:-vs-ci}"
QDRANT_REST="${QDRANT_REST:-6333}"
QDRANT_GRPC="${QDRANT_GRPC:-6334}"
NATS_PORT="${NATS_PORT:-4222}"
S3_PORT="${S3_PORT:-3900}"

# The same fixed dev credentials `crates/vs-infra/devenv.nix` imports into Garage, so a
# local run and CI read the same AWS_* values. Garage's format is GK + 24 hex, 64 hex.
S3_REGION="${AWS_REGION:-garage}"
S3_BUCKET="${S3_BUCKET:-visual-suite}"
S3_KEY_NAME="vs-dev"
S3_ACCESS_KEY="${AWS_ACCESS_KEY_ID:-GK0123456789abcdef01234567}"
S3_SECRET_KEY="${AWS_SECRET_ACCESS_KEY:-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef}"

QDRANT_IMAGE="qdrant/qdrant:v1.20.0"
NATS_IMAGE="nats:2-alpine"
GARAGE_IMAGE="dxflrs/garage:v1.0.1"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

run() {
  local name="$1"; shift
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker run -d --name "$name" "$@" >/dev/null
}

# Waits for a TCP port to accept a connection. Bash's /dev/tcp rather than nc, which is
# not installed everywhere — and *not* `sh`, where /dev/tcp silently does nothing.
wait_port() {
  local port="$1" name="$2"
  for _ in $(seq 1 90); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
      echo "$name is up on $port"
      return 0
    fi
    sleep 1
  done
  echo "$name did not come up on port $port" >&2
  docker logs "$PREFIX-${name}" 2>&1 | tail -30 >&2 || true
  return 1
}

echo "::group::Qdrant"
run "$PREFIX-qdrant" \
  -p "127.0.0.1:$QDRANT_REST:6333" -p "127.0.0.1:$QDRANT_GRPC:6334" \
  -e QDRANT__TELEMETRY_DISABLED=true "$QDRANT_IMAGE"
wait_port "$QDRANT_GRPC" qdrant
echo "::endgroup::"

echo "::group::NATS"
# JetStream is not on by default, and vs-infra creates its streams at startup.
run "$PREFIX-nats" -p "127.0.0.1:$NATS_PORT:4222" "$NATS_IMAGE" -js
wait_port "$NATS_PORT" nats
echo "::endgroup::"

echo "::group::Garage"
cat > "$work/garage.toml" <<TOML
metadata_dir = "/var/lib/garage/meta"
data_dir = "/var/lib/garage/data"
db_engine = "sqlite"
replication_factor = 1
rpc_bind_addr = "[::]:3901"
rpc_public_addr = "127.0.0.1:3901"
rpc_secret = "$S3_SECRET_KEY"

[s3_api]
s3_region = "$S3_REGION"
api_bind_addr = "[::]:3900"
root_domain = ".s3.garage.localhost"
TOML
mkdir -p "$work/data" "$work/meta"
chmod -R 777 "$work"
run "$PREFIX-garage" \
  -p "127.0.0.1:$S3_PORT:3900" \
  -v "$work/garage.toml:/etc/garage.toml:ro" \
  -v "$work/data:/var/lib/garage/data" \
  -v "$work/meta:/var/lib/garage/meta" \
  "$GARAGE_IMAGE" /garage server
wait_port "$S3_PORT" garage

garage() { docker exec "$PREFIX-garage" /garage "$@"; }
# A fresh Garage has no storage layout, and refuses every write until it does.
node=""
for _ in $(seq 1 60); do
  node="$(garage node id -q 2>/dev/null | cut -d@ -f1 || true)"
  [ -n "$node" ] && break
  sleep 1
done
[ -n "$node" ] || { echo "garage never reported a node id" >&2; exit 1; }
garage layout assign -z dc1 -c 1G "$node"
garage layout apply --version 1
garage key import --yes -n "$S3_KEY_NAME" "$S3_ACCESS_KEY" "$S3_SECRET_KEY"
garage bucket create "$S3_BUCKET"
garage bucket allow --read --write --owner "$S3_BUCKET" --key "$S3_KEY_NAME"
echo "::endgroup::"
