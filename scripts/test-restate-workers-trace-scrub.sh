#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"
compose_project="${LASH_RESTATE_WORKERS_COMPOSE_PROJECT:-lash-restate-workers-${LASH_GATE_WORKTREE_SLUG}}"
trace_volume="${compose_project}_trace-output"
created_volume=0

cleanup() {
  if [ "$created_volume" -eq 1 ]; then
    docker volume rm -f "$trace_volume" >/dev/null 2>&1 || true
  fi
  lash_gate_cleanup
}
trap cleanup EXIT

if docker volume inspect "$trace_volume" >/dev/null 2>&1; then
  echo "Refusing trace scrub regression: volume '$trace_volume' already exists." >&2
  exit 73
fi

docker volume create \
  --label "com.docker.compose.project=${compose_project}" \
  --label "com.docker.compose.volume=trace-output" \
  "$trace_volume" >/dev/null
created_volume=1
docker run --rm -v "$trace_volume:/e2e-traces" postgres:16-alpine sh -c '
  for needle in app_lookup async_lookup make_attachment crash_once parent child parent_wake on_button; do
    printf "{\"stale\":\"%s\"}\n" "$needle"
  done > /e2e-traces/stale.jsonl
'

stale_count="$(docker run --rm -v "$trace_volume:/e2e-traces" postgres:16-alpine \
  sh -c 'wc -l < /e2e-traces/stale.jsonl')"
[ "$stale_count" -eq 8 ] || { echo "failed to seed stale trace evidence" >&2; exit 1; }

probe_output="$(LASH_E2E_TRACE_SCRUB_PROBE=1 \
  bash "$repo/scripts/restate-postgres-workers-e2e.sh")"
created_volume=0
printf '%s\n' "$probe_output"
grep -Fq "stale evidence removed; fresh assertion input empty" <<<"$probe_output"
if docker volume inspect "$trace_volume" >/dev/null 2>&1; then
  echo "trace scrub probe leaked '$trace_volume'" >&2
  exit 1
fi

echo "stale trace regression passed: eight stale presence needles could not reach assertion input"
