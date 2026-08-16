#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"
lash_gate_acquire process-operations-e2e

compose_project="${LASH_PROCESS_OPERATIONS_COMPOSE_PROJECT:-lash-process-operations-${LASH_GATE_WORKTREE_SLUG}}"
compose=(docker compose -p "$compose_project" -f "$repo/runbooks/process-operations/docker-compose.yml")
postgres_port="${LASH_PROCESS_OPERATIONS_POSTGRES_PORT:-$((LASH_E2E_PORT_BASE + 46))}"
minio_port="${LASH_PROCESS_OPERATIONS_MINIO_PORT:-$((LASH_E2E_PORT_BASE + 41))}"
minio_console_port="${LASH_PROCESS_OPERATIONS_MINIO_CONSOLE_PORT:-$((LASH_E2E_PORT_BASE + 42))}"
restate_admin_port="${LASH_PROCESS_OPERATIONS_RESTATE_ADMIN_PORT:-$((LASH_E2E_PORT_BASE + 43))}"
restate_ingress_port="${LASH_PROCESS_OPERATIONS_RESTATE_INGRESS_PORT:-$((LASH_E2E_PORT_BASE + 44))}"
restate_node_port="${LASH_PROCESS_OPERATIONS_RESTATE_NODE_PORT:-$((LASH_E2E_PORT_BASE + 45))}"
export LASH_PROCESS_OPERATIONS_POSTGRES_PORT="$postgres_port"
export LASH_PROCESS_OPERATIONS_MINIO_PORT="$minio_port"
export LASH_PROCESS_OPERATIONS_MINIO_CONSOLE_PORT="$minio_console_port"
export LASH_PROCESS_OPERATIONS_RESTATE_ADMIN_PORT="$restate_admin_port"
export LASH_PROCESS_OPERATIONS_RESTATE_INGRESS_PORT="$restate_ingress_port"
export LASH_PROCESS_OPERATIONS_RESTATE_NODE_PORT="$restate_node_port"
if [ -n "${LASH_PROCESS_OPERATIONS_ARTIFACT_DIR:-}" ]; then
  artifact_dir="$LASH_PROCESS_OPERATIONS_ARTIFACT_DIR"
else
  artifact_dir="$(mktemp -d "${TMPDIR:-/tmp}/lash-process-operations-${LASH_GATE_WORKTREE_SLUG}.XXXXXX")"
fi
mkdir -p "$artifact_dir"
crash_container="${compose_project}-crash-window"
test_output="$artifact_dir/process-operations-e2e.log"

cleanup() {
  status=$?
  docker rm -f "$crash_container" >/dev/null 2>&1 || true
  "${compose[@]}" --profile crash down -v --remove-orphans >/dev/null 2>&1 || true
  if [ "$status" -ne 0 ]; then
    echo "process-operations E2E failed with status $status; artifacts: $artifact_dir" >&2
  fi
  exit "$status"
}
trap cleanup EXIT

cargo build --locked --release -p lash-restate-postgres-workers-e2e \
  --bin lash-e2e-process-operations-worker
export LASH_PROCESS_OPERATIONS_BIN_DIR="${CARGO_TARGET_DIR:-$repo/target}/release"

bash scripts/docker-pull-with-retry.sh ubuntu:24.04
"${compose[@]}" up -d postgres minio minio-init restate

deadline=$((SECONDS + 90))
until docker run --rm --name "lash-process-postgres-probe-${LASH_GATE_WORKTREE_SLUG}-$$" \
  --label "$LASH_GATE_LABEL" --network host postgres:16-alpine \
  pg_isready -h 127.0.0.1 -p "$postgres_port" -U lash -d lash >/dev/null 2>&1; do
  if ((SECONDS >= deadline)); then
    echo "Postgres did not become ready" >&2
    exit 1
  fi
  sleep 1
done
until curl -fsS --max-time 2 "http://127.0.0.1:${minio_port}/minio/health/live" >/dev/null; do
  if ((SECONDS >= deadline)); then
    echo "MinIO did not become ready" >&2
    exit 1
  fi
  sleep 1
done
while true; do
  minio_init_id="$("${compose[@]}" ps -a -q minio-init)"
  if [ -n "$minio_init_id" ]; then
    minio_init_status="$(docker inspect -f '{{.State.Status}}' "$minio_init_id")"
    if [ "$minio_init_status" = "exited" ]; then
      minio_init_exit="$(docker inspect -f '{{.State.ExitCode}}' "$minio_init_id")"
      if [ "$minio_init_exit" != "0" ]; then
        echo "minio-init exited with status $minio_init_exit" >&2
        exit 1
      fi
      break
    fi
  fi
  if ((SECONDS >= deadline)); then
    echo "minio-init did not complete before timeout" >&2
    exit 1
  fi
  sleep 1
done
until curl -fsS --max-time 2 "http://127.0.0.1:${restate_admin_port}/deployments" >"$artifact_dir/restate-deployments.json"; do
  if ((SECONDS >= deadline)); then
    echo "Restate did not become ready" >&2
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
docker run --rm --name "lash-process-postgres-query-${LASH_GATE_WORKTREE_SLUG}-$$" \
  --label "$LASH_GATE_LABEL" --network host -e PGPASSWORD=lash postgres:16-alpine \
  psql -h 127.0.0.1 -p "$postgres_port" -U lash -d lash -Atqc \
  "SELECT json_build_object('postgres_version', current_setting('server_version'), 'port', ${postgres_port})" \
  >"$artifact_dir/00-postgres.json"
echo "scenario 0 evidence: Restate, PostgreSQL:${postgres_port}, and MinIO are live" | tee "$test_output"

LASH_MINIO_ENDPOINT="http://127.0.0.1:${minio_port}" \
LASH_MINIO_BUCKET="lash-attachments" \
LASH_MINIO_REGION="us-east-1" \
LASH_MINIO_ACCESS_KEY="minioadmin" \
LASH_MINIO_SECRET_KEY="minioadmin" \
LASH_MINIO_PREFIX="runbooks/process-operations-${LASH_GATE_WORKTREE_SLUG}-$$" \
LASH_REQUIRE_MINIO=1 \
  cargo test --locked -p lash-s3-store -- --nocapture \
  2>&1 | tee "$artifact_dir/00-minio-conformance.log" | tee -a "$test_output"

postgres_url="postgres://lash:lash@127.0.0.1:${postgres_port}/lash"
LASH_POSTGRES_DATABASE_URL="$postgres_url" \
  cargo test --locked -p lash-postgres-store --test conformance \
  postgres_wake_delivery_crash_matrix_when_configured -- --nocapture --test-threads=1 \
  2>&1 | tee "$artifact_dir/01-wake-delivery.log" | tee -a "$test_output"
echo "scenario 1 evidence: TargetGone and Expired typed discards plus blocked-head redrive passed on PostgreSQL" | tee -a "$test_output"
echo "scenario 6 evidence: prune/re-register delivered a strictly higher sequence; forced rewind surfaced sequence_rewound" | tee -a "$test_output"

"${compose[@]}" --profile crash run --rm crash-worker retarget \
  2>&1 | tee "$artifact_dir/02-retarget.jsonl" | tee -a "$test_output"
grep -q '"old_discard_reason":"retargeted"' "$artifact_dir/02-retarget.jsonl"
grep -q '"old_target_turn_count":0' "$artifact_dir/02-retarget.jsonl"
grep -q '"new_target_turn_count":1' "$artifact_dir/02-retarget.jsonl"
echo "scenario 2 evidence: old pending delivery is Retargeted with an audit event; one next wake reached only the new target" | tee -a "$test_output"

cargo test --locked -p lash-core \
  process_tool_filter_narrows_only_session_tools_and_never_internal_wakes -- --nocapture \
  2>&1 | tee "$artifact_dir/03-tool-visibility.log" | tee -a "$test_output"
cargo test --locked -p lash-runtime \
  process_admin_list_signal_and_cancel_bypass_model_tool_filter -- --nocapture \
  2>&1 | tee -a "$artifact_dir/03-tool-visibility.log" | tee -a "$test_output"
echo "scenario 3 evidence: model process tools were filtered while host list/signal/cancel remained complete" | tee -a "$test_output"

LASH_POSTGRES_DATABASE_URL="$postgres_url" \
  cargo test --locked -p lash-postgres-store --test conformance \
  postgres_runtime_persistence_satisfies_conformance_when_configured \
  -- --nocapture --test-threads=1 \
  2>&1 | tee "$artifact_dir/04-wake-turn-policy.log" | tee -a "$test_output"
echo "scenario 4 evidence: EachWake produced separate claims and Coalesce produced one multi-batch claim on PostgreSQL" | tee -a "$test_output"

DATABASE_URL="$postgres_url" \
  cargo run --locked --release --quiet -p lash-restate-postgres-workers-e2e \
    --bin lash-e2e-process-operator-flow -- selected-drain \
  2>&1 | tee "$artifact_dir/08-selected-drain.jsonl" | tee -a "$test_output"
python3 - "$artifact_dir/08-selected-drain.jsonl" <<'PY'
import json
import sys


checkpoint = None
with open(sys.argv[1], encoding="utf-8") as stream:
    for line in stream:
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if value.get("checkpoint") == "selected_drain_scope_isolated":
            checkpoint = value
            break
if checkpoint is None:
    raise SystemExit("missing selected_drain_scope_isolated checkpoint")
expected = {
    "selected_satisfaction": "ClaimedNow",
    "replay_satisfaction": "AlreadySatisfied",
    "unselected_pending_after_claim": True,
    "refusal": "UnclaimableTogether",
    "provider_calls": 1,
}
for field, expected_value in expected.items():
    if checkpoint.get(field) != expected_value:
        raise SystemExit(
            f"selected-drain field {field} was {checkpoint.get(field)!r}, "
            f"expected {expected_value!r}: {checkpoint}"
        )
if checkpoint["unselected_batch_id"] not in checkpoint["pending_after_refusal"]:
    raise SystemExit(f"selected drain settled unselected B: {checkpoint}")
if checkpoint["refusal_unclaimed_batch_ids"] == []:
    raise SystemExit(f"typed refusal named no unclaimed row: {checkpoint}")
PY
echo "scenario 8 evidence: selected A settled alone; unselected B remained pending; replay and refusal stayed typed" | tee -a "$test_output"

LASH_POSTGRES_DATABASE_URL="$postgres_url" \
  cargo test --locked -p lash-postgres-store --test conformance \
  postgres_process_trigger_retention_satisfies_conformance_when_configured \
  -- --nocapture --test-threads=1 \
  2>&1 | tee "$artifact_dir/07-retention.log" | tee -a "$test_output"
echo "scenario 7 evidence: receipts survived; pruned-process deliveries reconciled; guarded tombstones refused compaction" | tee -a "$test_output"

"${compose[@]}" --profile crash run --rm crash-worker prepare \
  2>&1 | tee "$artifact_dir/05-crash-prepare.jsonl" | tee -a "$test_output"
"${compose[@]}" --profile crash run -d --name "$crash_container" crash-worker crash >/dev/null
deadline=$((SECONDS + 30))
until docker logs "$crash_container" 2>&1 | tee "$artifact_dir/05-crash-window.jsonl" | \
  grep -q 'receiver_enqueued_sender_unmarked'; do
  if ! docker inspect "$crash_container" --format '{{.State.Running}}' 2>/dev/null | grep -q true; then
    docker logs "$crash_container" >&2 || true
    echo "crash worker exited before the crash-window checkpoint" >&2
    exit 1
  fi
  if ((SECONDS >= deadline)); then
    docker logs "$crash_container" >&2 || true
    echo "crash worker did not reach the crash-window checkpoint" >&2
    exit 1
  fi
  sleep 1
done
docker kill "$crash_container" >/dev/null
docker wait "$crash_container" >"$artifact_dir/05-killed-exit-code.txt"
"${compose[@]}" --profile crash run --rm crash-worker recover \
  2>&1 | tee "$artifact_dir/05-crash-recovered.jsonl" | tee -a "$test_output"
grep -q '"receiver_turn_count":1' "$artifact_dir/05-crash-recovered.jsonl"
grep -q '"floor_absorbed":1' "$artifact_dir/05-crash-recovered.jsonl"
python3 - "$artifact_dir/05-crash-window.jsonl" "$artifact_dir/05-crash-recovered.jsonl" <<'PY'
import json
import sys


def checkpoint(path, name):
    with open(path, encoding="utf-8") as stream:
        for line in stream:
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                continue
            if value.get("checkpoint") == name:
                return value
    raise SystemExit(f"missing {name!r} checkpoint in {path}")


window = checkpoint(sys.argv[1], "receiver_enqueued_sender_unmarked")
recovered = checkpoint(sys.argv[2], "recovered_exactly_once")
if recovered["attempts"] < 2:
    raise SystemExit(f"recovery did not reclaim the delivery: {recovered}")
if recovered["receiver_batch_id"] != window["batch_id"]:
    raise SystemExit(
        "recovery changed receiver batch identity: "
        f"before={window['batch_id']} after={recovered['receiver_batch_id']}"
    )
PY
echo "scenario 5 evidence: worker killed after receiver enqueue and before sender mark; restart retained exactly one receiver turn" | tee -a "$test_output"

if grep -Fn 'panicked at' "$test_output" >&2; then
  echo "panic gate: FAILED ('panicked at' found in process-operations E2E output)" >&2
  exit 1
fi
echo "panic gate: clean (no 'panicked at' lines in process-operations E2E output)" | tee -a "$test_output"
echo "process-operations e2e passed: scenarios=8 artifacts=$artifact_dir" | tee -a "$test_output"
