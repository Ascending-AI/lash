#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

compose_project="lash-process-operations-${USER:-runner}-$$"
compose=(docker compose -p "$compose_project" -f "$repo/runbooks/process-operations/docker-compose.yml")
if [ -n "${LASH_PROCESS_OPERATIONS_ARTIFACT_DIR:-}" ]; then
  artifact_dir="$LASH_PROCESS_OPERATIONS_ARTIFACT_DIR"
else
  artifact_dir="$(mktemp -d "${TMPDIR:-/tmp}/lash-process-operations.XXXXXX")"
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

"${compose[@]}" --profile crash down -v --remove-orphans >/dev/null 2>&1 || true
bash scripts/docker-pull-with-retry.sh ubuntu:24.04
"${compose[@]}" up -d minio minio-init restate

deadline=$((SECONDS + 90))
until docker run --rm --network host postgres:16-alpine \
  pg_isready -h 127.0.0.1 -p 5446 -U lash -d lash >/dev/null 2>&1; do
  if ((SECONDS >= deadline)); then
    echo "Postgres did not become ready" >&2
    exit 1
  fi
  sleep 1
done
until curl -fsS --max-time 2 "http://127.0.0.1:${LASH_PROCESS_OPERATIONS_MINIO_PORT:-19446}/minio/health/live" >/dev/null; do
  if ((SECONDS >= deadline)); then
    echo "MinIO did not become ready" >&2
    exit 1
  fi
  sleep 1
done
"${compose[@]}" wait minio-init >/dev/null
until curl -fsS --max-time 2 "http://127.0.0.1:${LASH_PROCESS_OPERATIONS_RESTATE_ADMIN_PORT:-19076}/deployments" >"$artifact_dir/restate-deployments.json"; do
  if ((SECONDS >= deadline)); then
    echo "Restate did not become ready" >&2
    exit 1
  fi
  sleep 1
done

"${compose[@]}" ps --format json >"$artifact_dir/00-live-services.json"
docker ps --filter publish=5446 --format json >"$artifact_dir/00-postgres-service.json"
if [ ! -s "$artifact_dir/00-postgres-service.json" ]; then
  echo "No running container publishes the assigned PostgreSQL port 5446" >&2
  exit 1
fi
docker run --rm --network host -e PGPASSWORD=lash postgres:16-alpine \
  psql -h 127.0.0.1 -p 5446 -U lash -d lash -Atqc \
  "SELECT json_build_object('postgres_version', current_setting('server_version'), 'port', 5446)" \
  >"$artifact_dir/00-postgres.json"
echo "scenario 0 evidence: Restate, PostgreSQL:5446, and MinIO are live" | tee "$test_output"

LASH_MINIO_ENDPOINT="http://127.0.0.1:${LASH_PROCESS_OPERATIONS_MINIO_PORT:-19446}" \
LASH_MINIO_BUCKET="lash-attachments" \
LASH_MINIO_REGION="us-east-1" \
LASH_MINIO_ACCESS_KEY="minioadmin" \
LASH_MINIO_SECRET_KEY="minioadmin" \
LASH_MINIO_PREFIX="runbooks/process-operations-$$" \
LASH_REQUIRE_MINIO=1 \
  cargo test --locked -p lash-s3-store -- --nocapture \
  2>&1 | tee "$artifact_dir/00-minio-conformance.log" | tee -a "$test_output"

postgres_url="postgres://lash:lash@127.0.0.1:5446/lash"
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
echo "process-operations e2e passed: scenarios=7 artifacts=$artifact_dir" | tee -a "$test_output"
