#!/usr/bin/env bash
set -euo pipefail

# Deterministic companion for runbooks/version-bump-recreation. It seeds a
# PostgreSQL deployment that an older lash owned, proves the exact-match schema
# gate refuses it in both directions, performs the recreation bump, and proves
# the three durable surfaces work on the recreated store.

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"
lash_gate_acquire version-bump-recreation-e2e

postgres_port="${LASH_VERSION_BUMP_POSTGRES_PORT:-$((LASH_E2E_PORT_BASE + 47))}"
export LASH_VERSION_BUMP_POSTGRES_PORT="$postgres_port"
compose_project="${LASH_VERSION_BUMP_COMPOSE_PROJECT:-lash-version-bump-${LASH_GATE_WORKTREE_SLUG}}"
compose=(docker compose -p "$compose_project" -f "$repo/runbooks/version-bump-recreation/docker-compose.yml")
if [ -n "${LASH_VERSION_BUMP_ARTIFACT_DIR:-}" ]; then
  artifact_dir="$LASH_VERSION_BUMP_ARTIFACT_DIR"
else
  artifact_dir="$(mktemp -d "${TMPDIR:-/tmp}/lash-version-bump-${LASH_GATE_WORKTREE_SLUG}.XXXXXX")"
fi
mkdir -p "$artifact_dir"
test_output="$artifact_dir/version-bump-recreation-e2e.log"

cleanup() {
  status=$?
  "${compose[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
  if [ "$status" -ne 0 ]; then
    echo "version-bump recreation E2E failed with status $status; artifacts: $artifact_dir" >&2
  fi
  exit "$status"
}
trap cleanup EXIT

database_url="postgres://lash:lash@127.0.0.1:${postgres_port}/lash"
export DATABASE_URL="$database_url"

harness() {
  cargo run --locked --quiet -p lash-restate-postgres-workers-e2e \
    --bin lash-e2e-version-bump -- "$1"
}

"${compose[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
"${compose[@]}" up -d postgres

deadline=$((SECONDS + 90))
until docker run --rm --name "lash-version-bump-postgres-probe-${LASH_GATE_WORKTREE_SLUG}-$$" \
  --label "$LASH_GATE_LABEL" --network host postgres:16-alpine \
  pg_isready -h 127.0.0.1 -p "$postgres_port" -U lash -d lash >/dev/null 2>&1; do
  if ((SECONDS >= deadline)); then
    "${compose[@]}" logs postgres >&2 || true
    echo "PostgreSQL did not become ready on port $postgres_port" >&2
    exit 1
  fi
  sleep 1
done

"${compose[@]}" ps --format json >"$artifact_dir/00-live-services.json"
docker ps --filter "publish=$postgres_port" --format json >"$artifact_dir/00-postgres-service.json"
if [ ! -s "$artifact_dir/00-postgres-service.json" ]; then
  echo "No running container publishes the assigned PostgreSQL port $postgres_port" >&2
  exit 1
fi
docker run --rm --name "lash-version-bump-postgres-query-${LASH_GATE_WORKTREE_SLUG}-$$" \
  --label "$LASH_GATE_LABEL" --network host -e PGPASSWORD=lash postgres:16-alpine \
  psql -h 127.0.0.1 -p "$postgres_port" -U lash -d lash -Atqc \
  "SELECT json_build_object('postgres_version', current_setting('server_version'), 'port', ${postgres_port})" \
  >"$artifact_dir/00-postgres.json"
echo "scenario 0 evidence: PostgreSQL:${postgres_port} is live and empty" | tee "$test_output"

harness seed 2>&1 | tee "$artifact_dir/01-seed.jsonl" | tee -a "$test_output"
harness refuse 2>&1 | tee "$artifact_dir/02-refusal.jsonl" | tee -a "$test_output"
harness recreate 2>&1 | tee "$artifact_dir/03-recreation.jsonl" | tee -a "$test_output"
harness health 2>&1 | tee "$artifact_dir/04-health.jsonl" | tee -a "$test_output"

python3 - "$artifact_dir" <<'PY'
import json
import sys
from pathlib import Path

artifacts = Path(sys.argv[1])


def checkpoint(name, filename):
    path = artifacts / filename
    for line in path.read_text(encoding="utf-8").splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if value.get("checkpoint") == name:
            return value
    raise SystemExit(f"missing {name!r} checkpoint in {path}")


def fail(message):
    raise SystemExit(f"version-bump recreation gate failed: {message}")


seeded = checkpoint("seeded_older_deployment", "01-seed.jsonl")
if seeded["recorded_version"] != seeded["expected_version"] - 1:
    fail(f"seed did not record the previous component version: {seeded}")
for field in ("session_ids", "process_ids", "trigger_process_ids"):
    if not seeded[field]:
        fail(f"seed left {field} empty: {seeded}")
if seeded["committed_sessions"] != len(seeded["session_ids"]):
    fail(f"seed did not commit a turn on every session: {seeded}")
if seeded["trigger_reservations"] != 1:
    fail(f"the seeded trigger did not reserve exactly one delivery: {seeded}")

stale = checkpoint("refused_older_store", "02-refusal.jsonl")
future = checkpoint("refused_newer_store", "02-refusal.jsonl")
for refusal in (stale, future):
    found = refusal["found_version"]
    expected = refusal["expected_version"]
    message = refusal["error"]
    if f"version {found}" not in message or f"expected {expected}" not in message:
        fail(f"refusal did not name both versions: {refusal}")
    if refusal["opened"]:
        fail(f"a mismatched store was opened: {refusal}")
if stale["found_version"] >= stale["expected_version"]:
    fail(f"older-store refusal was not older: {stale}")
if future["found_version"] <= future["expected_version"]:
    fail(f"newer-store refusal was not newer: {future}")

recreated = checkpoint("recreated_store", "03-recreation.jsonl")
if recreated["recorded_version"] != recreated["expected_version"]:
    fail(f"recreated store is not at the expected version: {recreated}")
if recreated["surviving_seeded_rows"] != 0:
    fail(f"recreation preserved pre-bump rows: {recreated}")
if recreated["surviving_seeded_graph_nodes"] != 0:
    fail(f"pre-bump committed session content survived recreation: {recreated}")
if recreated["pre_bump_graph_nodes"] < 1:
    fail(f"the pre-bump store held no committed session content: {recreated}")

health = checkpoint("verified_recreated_deployment", "04-health.jsonl")
for gate in ("session_turn_committed", "process_ran_to_terminal", "trigger_fired"):
    if health.get(gate) is not True:
        fail(f"post-bump gate {gate} did not pass: {health}")
if health["session_ids_reused"] != seeded["session_ids"]:
    fail(f"health check did not reuse the pre-bump session ids: {health}")
if health["trigger_reservations"] != 1 or health["trigger_process_status"] != "Completed":
    fail(f"the post-bump trigger did not deliver to a finished process: {health}")

print("version-bump recreation gates: seed, both refusals, recreation, and health all asserted")
PY

if grep -Fn 'panicked at' "$test_output" >&2; then
  echo "panic gate: FAILED ('panicked at' found in version-bump recreation E2E output)" >&2
  exit 1
fi
echo "panic gate: clean (no 'panicked at' lines in version-bump recreation E2E output)" | tee -a "$test_output"
echo "version-bump recreation e2e passed: scenarios=4 artifacts=$artifact_dir" | tee -a "$test_output"
