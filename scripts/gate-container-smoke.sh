#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"
lash_gate_acquire gate-container-smoke

postgres_container="lash-gate-smoke-postgres-${LASH_GATE_WORKTREE_SLUG}"
minio_container="lash-gate-smoke-minio-${LASH_GATE_WORKTREE_SLUG}"
restate_container="lash-gate-smoke-restate-${LASH_GATE_WORKTREE_SLUG}"
postgres_port="${LASH_GATE_SMOKE_POSTGRES_PORT:-$((LASH_E2E_PORT_BASE + 10))}"
minio_port="${LASH_GATE_SMOKE_MINIO_PORT:-$((LASH_E2E_PORT_BASE + 11))}"
restate_admin_port="${LASH_GATE_SMOKE_RESTATE_ADMIN_PORT:-$((LASH_E2E_PORT_BASE + 20))}"
restate_ingress_port="${LASH_GATE_SMOKE_RESTATE_INGRESS_PORT:-$((LASH_E2E_PORT_BASE + 21))}"
restate_node_port="${LASH_GATE_SMOKE_RESTATE_NODE_PORT:-$((LASH_E2E_PORT_BASE + 22))}"
started_at="$SECONDS"

cleanup() {
  docker rm -f "$postgres_container" "$minio_container" "$restate_container" >/dev/null 2>&1 || true
}
trap cleanup EXIT

bash scripts/docker-pull-with-retry.sh postgres:16-alpine
bash scripts/docker-pull-with-retry.sh minio/minio:RELEASE.2025-04-22T22-12-26Z
bash scripts/docker-pull-with-retry.sh restatedev/restate:1.7.0

docker run -d --name "$postgres_container" \
  --label "$LASH_GATE_LABEL" \
  --network "$LASH_E2E_NETWORK" \
  -e POSTGRES_USER=lash \
  -e POSTGRES_PASSWORD=lash \
  -e POSTGRES_DB=lash \
  -p "127.0.0.1:${postgres_port}:5432" \
  postgres:16-alpine >/dev/null
docker run -d --name "$minio_container" \
  --label "$LASH_GATE_LABEL" \
  --network "$LASH_E2E_NETWORK" \
  -e MINIO_ROOT_USER=minioadmin \
  -e MINIO_ROOT_PASSWORD=minioadmin \
  -p "127.0.0.1:${minio_port}:9000" \
  minio/minio:RELEASE.2025-04-22T22-12-26Z server /data >/dev/null
docker run -d --name "$restate_container" \
  --label "$LASH_GATE_LABEL" \
  --network host \
  -e RESTATE_ADMIN__BIND_PORT="$restate_admin_port" \
  -e RESTATE_INGRESS__BIND_PORT="$restate_ingress_port" \
  -e RESTATE_BIND_PORT="$restate_node_port" \
  restatedev/restate:1.7.0 >/dev/null

deadline=$((SECONDS + 60))
until docker exec "$postgres_container" pg_isready -U lash -d lash >/dev/null 2>&1; do
  ((SECONDS < deadline)) || { docker logs "$postgres_container" >&2; exit 1; }
  sleep 1
done
until curl -fsS --max-time 2 "http://127.0.0.1:${minio_port}/minio/health/live" >/dev/null; do
  ((SECONDS < deadline)) || { docker logs "$minio_container" >&2; exit 1; }
  sleep 1
done
until curl -fsS --max-time 2 "http://127.0.0.1:${restate_admin_port}/deployments" >/dev/null; do
  ((SECONDS < deadline)) || { docker logs "$restate_container" >&2; exit 1; }
  sleep 1
done

printf 'gate container smoke ready: slug=%s base=%s ports=%s,%s,%s-%s network=%s elapsed=%ss\n' \
  "$LASH_GATE_WORKTREE_SLUG" "$LASH_E2E_PORT_BASE" \
  "$postgres_port" "$minio_port" "$restate_admin_port" "$restate_node_port" \
  "$LASH_E2E_NETWORK" "$((SECONDS - started_at))"

if [ "${LASH_GATE_SMOKE_HOLD_SECONDS:-0}" -gt 0 ]; then
  sleep "$LASH_GATE_SMOKE_HOLD_SECONDS"
fi
