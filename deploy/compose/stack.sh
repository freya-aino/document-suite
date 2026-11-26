#!/usr/bin/env bash
# Brings the integration stack up, down, or tells you where it is.
#
#   deploy/compose/stack.sh up   [--tier fake|cpu|gpu] [--no-build] [--profile mcp|replicas]
#   deploy/compose/stack.sh down [--tier ...]
#   deploy/compose/stack.sh env                      # the variables the test driver reads
#   deploy/compose/stack.sh logs [service ...]
#
# The tier chooses the model tier and is composed with `-f` rather than a compose
# profile; `docker-compose.yml` says why. `fake` is the default and the one CI runs.
#
# Qdrant, Garage and Keycloak come from .github/scripts/services.sh with NETWORK set, so
# they join this network under the aliases the compose files use while keeping their
# published localhost ports for the host-side driver.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HERE="$ROOT/deploy/compose"

# Ports deliberately in the 5700x range and all bound to localhost: a developer machine
# running this repo is likely to have `devenv --profile stack up` (6333/6334/3900/8081)
# and other project stacks already holding the obvious 5xxxx numbers. Every one is
# overridable, and `stack.sh up` fails loudly on a collision rather than half-starting.
NETWORK="${DS_ITEST_NETWORK:-ds-itest}"
PREFIX="${DS_ITEST_PREFIX_CONTAINERS:-ds-itest}"
QDRANT_REST="${QDRANT_REST:-57333}"
QDRANT_GRPC="${QDRANT_GRPC:-57334}"
S3_PORT="${S3_PORT:-57900}"
KEYCLOAK_PORT="${KEYCLOAK_PORT:-57081}"
API_PORT="${DS_ITEST_API_PORT:-57080}"
METRICS_PORT="${DS_ITEST_METRICS_PORT:-57464}"
READY_TIMEOUT="${DS_ITEST_READY_TIMEOUT:-180}"

tier="fake"
build=(--build)
profiles=()
cmd="${1:-}"
[ $# -gt 0 ] && shift
while [ $# -gt 0 ]; do
  case "$1" in
    --tier) tier="$2"; shift 2 ;;
    --profile) profiles+=(--profile "$2"); shift 2 ;;
    # For CI, which builds the images itself with a shared layer cache and would
    # otherwise pay for the build twice.
    --no-build) build=(); shift ;;
    *) break ;;
  esac
done

case "$tier" in
  fake) files=(-f "$HERE/docker-compose.yml" -f "$HERE/compose.fake.yml") ;;
  cpu)  files=(-f "$HERE/docker-compose.yml" -f "$HERE/compose.cpu.yml") ;;
  gpu)  files=(-f "$HERE/docker-compose.yml" -f "$ROOT/deploy/gpu/docker-compose.yml" -f "$HERE/compose.gpu.yml") ;;
  *) echo "unknown tier: $tier (fake|cpu|gpu)" >&2; exit 2 ;;
esac

# The registry the service mounts, per tier. Paths are relative to this directory
# because that is what compose resolves a relative volume source against.
case "$tier" in
  fake) models="./models/fake.toml" ;;
  cpu)  models="./models/cpu.toml" ;;
  gpu)  models="./models/gpu.toml" ;;
esac
export DS_ITEST_MODELS="$models"
export DS_ITEST_NETWORK="$NETWORK"
export DS_ITEST_API_PORT="$API_PORT"
export DS_ITEST_METRICS_PORT="$METRICS_PORT"

compose() { docker compose "${files[@]}" "${profiles[@]+"${profiles[@]}"}" "$@"; }

print_env() {
  echo "export DS_ITEST_BASE_URL=http://127.0.0.1:$API_PORT"
  echo "export DS_ITEST_METRICS_URL=http://127.0.0.1:$METRICS_PORT/metrics"
  echo "export DS_ITEST_ISSUER=http://keycloak:8080/realms/koki"
  echo "export DS_ITEST_TOKEN_URL=http://127.0.0.1:$KEYCLOAK_PORT/realms/koki/protocol/openid-connect/token"
  echo "export DS_ITEST_TIER=$tier"
}

case "$cmd" in
  up)
    # Idempotent: an existing network is reused, and the service script replaces its own
    # containers.
    docker network inspect "$NETWORK" >/dev/null 2>&1 || docker network create "$NETWORK" >/dev/null

    echo "== Qdrant, Garage and Keycloak (.github/scripts/services.sh)"
    NETWORK="$NETWORK" PREFIX="$PREFIX" \
      QDRANT_REST="$QDRANT_REST" QDRANT_GRPC="$QDRANT_GRPC" \
      S3_PORT="$S3_PORT" KEYCLOAK_PORT="$KEYCLOAK_PORT" \
      "$ROOT/.github/scripts/services.sh"

    echo "== building and starting the suite (tier: $tier)"
    # No `--wait`: `migrate` is a one-shot that exits 0, and `--wait` treats an exited
    # container as a failure to come up. Readiness is asserted below instead, against
    # the migration's exit code and the service's own public probe — which is what a
    # load balancer would ask anyway.
    compose up -d "${build[@]+"${build[@]}"}" || {
      echo "!! the stack did not come up; logs follow" >&2
      compose ps >&2 || true
      compose logs --tail 80 >&2 || true
      for c in "$PREFIX-qdrant" "$PREFIX-garage" "$PREFIX-keycloak"; do
        echo "== $c" >&2
        docker logs "$c" 2>&1 | tail -40 >&2 || true
      done
      exit 1
    }

    # The migration is a deploy step, so a non-zero exit here is the whole stack's
    # failure and not something to discover later as a schema-version mismatch.
    migrate_id="$(compose ps -aq migrate)"
    if [ -n "$migrate_id" ]; then
      migrate_code="$(docker inspect -f '{{.State.ExitCode}}' "$migrate_id")"
      if [ "$migrate_code" != "0" ]; then
        echo "!! ds-suite migrate exited $migrate_code" >&2
        docker logs "$migrate_id" 2>&1 | tail -40 >&2 || true
        exit 1
      fi
    fi

    deadline=$(( $(date +%s) + READY_TIMEOUT ))
    until curl -sf "http://127.0.0.1:$API_PORT/readyz" >/dev/null; do
      if [ "$(date +%s)" -ge "$deadline" ]; then
        echo "!! /readyz never answered on 127.0.0.1:$API_PORT" >&2
        compose logs --tail 80 ds-suite >&2 || true
        exit 1
      fi
      sleep 2
    done

    echo "== ready"
    print_env
    ;;
  down)
    compose down -v --remove-orphans || true
    for c in "$PREFIX-qdrant" "$PREFIX-garage" "$PREFIX-keycloak"; do
      docker rm -f "$c" >/dev/null 2>&1 || true
    done
    docker network rm "$NETWORK" >/dev/null 2>&1 || true
    ;;
  env) print_env ;;
  logs) compose logs --tail "${DS_ITEST_LOG_TAIL:-120}" "$@" ;;
  ps) compose ps ;;
  *)
    sed -n '2,12p' "${BASH_SOURCE[0]}" >&2
    exit 2
    ;;
esac
