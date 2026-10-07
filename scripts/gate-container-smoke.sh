#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"
lash_gate_acquire gate-container-smoke
# shellcheck source=scripts/ci/s3-service.sh
source "$repo/scripts/ci/s3-service.sh"
# shellcheck source=scripts/ci/pg-service.sh
source "$repo/scripts/ci/pg-service.sh"

postgres_container="lash-gate-smoke-postgres-${LASH_GATE_WORKTREE_SLUG}"
s3_container="lash-gate-smoke-s3-${LASH_GATE_WORKTREE_SLUG}"
postgres_port="${LASH_GATE_SMOKE_POSTGRES_PORT:-$((LASH_E2E_PORT_BASE + 10))}"
s3_port="${LASH_GATE_SMOKE_S3_PORT:-$((LASH_E2E_PORT_BASE + 11))}"
started_at="$SECONDS"

cleanup() {
  docker rm -f "$postgres_container" "$s3_container" >/dev/null 2>&1 || true
  lash_gate_cleanup
}
trap cleanup EXIT

bash scripts/docker-pull-with-retry.sh postgres:18-alpine
bash scripts/docker-pull-with-retry.sh "$LASH_S3_IMAGE"

docker run -d --name "$postgres_container" \
  --label "$LASH_GATE_LABEL" \
  --network "$LASH_E2E_NETWORK" \
  -e POSTGRES_USER=lash \
  -e POSTGRES_PASSWORD=lash \
  -e POSTGRES_DB=lash \
  -p "127.0.0.1:${postgres_port}:5432" \
  postgres:18-alpine >/dev/null
lash_s3_start "$s3_container" "$s3_port" \
  --label "$LASH_GATE_LABEL" \
  --network "$LASH_E2E_NETWORK"

deadline=$((SECONDS + 60))
until lash_pg_ready docker exec "$postgres_container"; do
  ((SECONDS < deadline)) || { docker logs "$postgres_container" >&2; exit 1; }
  sleep 1
done
lash_s3_wait "$s3_container" "$((deadline - SECONDS))"

printf 'gate container smoke ready: slug=%s base=%s ports=%s,%s network=%s elapsed=%ss\n' \
  "$LASH_GATE_WORKTREE_SLUG" "$LASH_E2E_PORT_BASE" \
  "$postgres_port" "$s3_port" \
  "$LASH_E2E_NETWORK" "$((SECONDS - started_at))"

if [ "${LASH_GATE_SMOKE_HOLD_SECONDS:-0}" -gt 0 ]; then
  sleep "$LASH_GATE_SMOKE_HOLD_SECONDS"
fi
