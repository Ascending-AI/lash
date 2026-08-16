#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
# shellcheck source=scripts/worktree-gate-env.sh
source "$repo_root/scripts/worktree-gate-env.sh"

workbench_port="${1:-3030}"
if [[ ! "$workbench_port" =~ ^[0-9]+$ ]]; then
  printf 'workbench port must be numeric, got %s\n' "$workbench_port" >&2
  exit 2
fi
workbench_port_number=$((10#$workbench_port))
if (( workbench_port_number < 1 || workbench_port_number > 65535 )); then
  printf 'workbench port must be between 1 and 65535, got %s\n' "$workbench_port" >&2
  exit 2
fi

# Offsets +0..+9 are reserved for this gate and selected by the workbench
# port's last decimal digit. This keeps every derived Postgres port inside the
# worktree's 50-port block without colliding with the +10..+48 lane offsets.
postgres_offset=$((workbench_port_number % 10))
postgres_port="${AGENT_WORKBENCH_USAGE_GATE_POSTGRES_PORT:-$((LASH_E2E_PORT_BASE + postgres_offset))}"
if (( postgres_port < 1 || postgres_port > 65535 )); then
  printf 'derived Postgres port is outside 1..65535: %s\n' "$postgres_port" >&2
  exit 2
fi

if [[ "${AGENT_WORKBENCH_USAGE_GATE_PORT_PROBE:-0}" == "1" ]]; then
  printf '%s\n' "$postgres_port"
  exit 0
fi

postgres_image="${AGENT_WORKBENCH_USAGE_GATE_POSTGRES_IMAGE:-postgres:16-alpine}"
postgres_container="${AGENT_WORKBENCH_USAGE_GATE_POSTGRES_CONTAINER:-lash-agent-workbench-attachment-usage-gate-${LASH_GATE_WORKTREE_SLUG}-$workbench_port}"
database_url="${AGENT_WORKBENCH_USAGE_GATE_DATABASE_URL:-}"
owns_postgres=0

if [[ -z "$database_url" ]]; then
  lash_gate_acquire agent-workbench-attachment-usage-gate
fi

cleanup() {
  if (( owns_postgres )); then
    docker rm -fv "$postgres_container" >/dev/null 2>&1 || true
  fi
  lash_gate_cleanup
}
trap cleanup EXIT

printf '[attachment-usage-gate] SQLite file-store/session-store pass\n'
cargo test -p agent-workbench attachment_usage_gate_sqlite -- --nocapture --test-threads=1

if [[ -z "$database_url" ]]; then
  bash "$repo_root/scripts/docker-pull-with-retry.sh" "$postgres_image"
  docker run -d --name "$postgres_container" --label "$LASH_GATE_LABEL" --network host \
    -e POSTGRES_USER=lash \
    -e POSTGRES_PASSWORD=lash \
    -e POSTGRES_DB=lash \
    "$postgres_image" -p "$postgres_port" >/dev/null
  owns_postgres=1
  database_url="postgres://lash:lash@127.0.0.1:$postgres_port/lash"

  deadline=$((SECONDS + 60))
  until docker exec "$postgres_container" pg_isready -p "$postgres_port" -U lash -d lash >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
      docker logs "$postgres_container" >&2 || true
      printf 'Postgres did not become ready on derived port %s\n' "$postgres_port" >&2
      exit 1
    fi
    sleep 1
  done
fi

printf '[attachment-usage-gate] Postgres session-store pass\n'
AGENT_WORKBENCH_USAGE_GATE_DATABASE_URL="$database_url" \
  cargo test -p agent-workbench attachment_usage_gate_postgres \
    -- --ignored --nocapture --test-threads=1

printf '[attachment-usage-gate] upload -> reference -> persist -> retrieve and usage restart gates passed\n'
