#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

# Every binary this runbook runs comes from the shared build pool's cache, in
# one build (FIG-3666): the Cargo compiles it used to run one scenario at a
# time spent 20 of the job's 22 minutes, one of them a release build of the
# operator-flow binary the worker artifacts already carry.
build_labels=(
  //crates/lash-s3-store:lash-s3-store__unit_test
  //crates/lash-postgres-store:conformance__test
  //crates/lash-core:runtime_lifecycle__test
  //crates/lash:lash__unit_test
)
mapfile -t built < <(python3 "$repo/scripts/ci/restate_suite.py" build "${build_labels[@]}")
if [ "${#built[@]}" -ne "${#build_labels[@]}" ]; then
  echo "expected ${#build_labels[@]} built outputs, got ${#built[@]}" >&2
  exit 1
fi
s3_store_tests="${built[0]}"
postgres_conformance_tests="${built[1]}"
core_lifecycle_tests="${built[2]}"
runtime_unit_tests="${built[3]}"

staged_bin_dir=""
if [ -n "${LASH_E2E_PREBUILT_BIN_DIR:-}" ]; then
  LASH_PROCESS_OPERATIONS_BIN_DIR="$(cd "$LASH_E2E_PREBUILT_BIN_DIR" && pwd)"
else
  # The same binaries CI's worker-artifacts job stages, under their Cargo
  # names (the compose file mounts the worker by it).
  staged_bin_dir="$(mktemp -d "${TMPDIR:-/tmp}/lash-process-operations-bin.XXXXXX")"
  LASH_PROCESS_OPERATIONS_BIN_DIR="$staged_bin_dir"
  python3 "$repo/scripts/ci/restate_suite.py" stage-binaries \
    //runbooks/restate-postgres-workers "$LASH_PROCESS_OPERATIONS_BIN_DIR" >/dev/null
fi
export LASH_PROCESS_OPERATIONS_BIN_DIR
for binary in lash-e2e-process-operations-worker; do
  if [ ! -x "$LASH_PROCESS_OPERATIONS_BIN_DIR/$binary" ]; then
    echo "Missing executable worker: $LASH_PROCESS_OPERATIONS_BIN_DIR/$binary" >&2
    exit 1
  fi
done

# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"
lash_gate_acquire process-operations-e2e

compose_project="${LASH_PROCESS_OPERATIONS_COMPOSE_PROJECT:-lash-process-operations-${LASH_GATE_WORKTREE_SLUG}}"
compose=(docker compose -p "$compose_project" -f "$repo/runbooks/process-operations/docker-compose.yml")
postgres_port="${LASH_PROCESS_OPERATIONS_POSTGRES_PORT:-$((LASH_E2E_PORT_BASE + 46))}"
s3_port="${LASH_PROCESS_OPERATIONS_S3_PORT:-$((LASH_E2E_PORT_BASE + 41))}"
restate_admin_port="${LASH_PROCESS_OPERATIONS_RESTATE_ADMIN_PORT:-$((LASH_E2E_PORT_BASE + 43))}"
restate_ingress_port="${LASH_PROCESS_OPERATIONS_RESTATE_INGRESS_PORT:-$((LASH_E2E_PORT_BASE + 44))}"
restate_node_port="${LASH_PROCESS_OPERATIONS_RESTATE_NODE_PORT:-$((LASH_E2E_PORT_BASE + 45))}"
export LASH_PROCESS_OPERATIONS_POSTGRES_PORT="$postgres_port"
export LASH_PROCESS_OPERATIONS_S3_PORT="$s3_port"
# The S3 service's image, credentials and bucket, which the compose file reads.
# shellcheck source=scripts/ci/s3-service.sh
source "$repo/scripts/ci/s3-service.sh"
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

run_postgres_conformance_test() {
  local selector="$1"
  local listing
  local test_count

  listing="$(
    cd crates/lash-postgres-store &&
      "$postgres_conformance_tests" "$selector" --exact --list
  )" || return
  test_count="$(awk '/: test$/ { count++ } END { print count + 0 }' <<<"$listing")"
  printf '%s\n' "$listing"
  if [ "$test_count" -ne 1 ]; then
    echo "Expected exactly one PostgreSQL conformance test for '$selector', found $test_count" >&2
    return 4
  fi

  (cd crates/lash-postgres-store &&
    "$postgres_conformance_tests" "$selector" --exact --nocapture --test-threads=1)
}

# FIG-3156. The runbook tells its judge to read a typed outcome out of a named
# artifact, so an artifact that lost its evidence line is a failed gate rather
# than a quieter pass. Scenarios 2, 5 and 8 already assert on their checkpoints;
# this is the same rule for the phases whose evidence comes from a Rust fixture's
# `--nocapture` stdout.
require_checkpoints() {
  local log="$1"
  shift
  local checkpoint
  for checkpoint in "$@"; do
    if ! grep -q "\"checkpoint\":\"${checkpoint}\"" "$log"; then
      echo "Missing runbook evidence checkpoint '${checkpoint}' in $log" >&2
      return 5
    fi
  done
}

cleanup() {
  status=$?
  docker rm -f "$crash_container" >/dev/null 2>&1 || true
  "${compose[@]}" --profile crash down -v --remove-orphans >/dev/null 2>&1 || true
  lash_gate_cleanup
  if [ -n "$staged_bin_dir" ]; then
    rm -rf -- "$staged_bin_dir"
  fi
  if [ "$status" -ne 0 ]; then
    echo "process-operations E2E failed with status $status; artifacts: $artifact_dir" >&2
  fi
  exit "$status"
}
trap cleanup EXIT

bash scripts/docker-pull-with-retry.sh ubuntu:24.04
"${compose[@]}" up -d postgres s3 restate

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
lash_s3_wait "$("${compose[@]}" ps -q s3)" 60
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
echo "scenario 0 evidence: Restate, PostgreSQL:${postgres_port}, and S3 (Garage) are live" | tee "$test_output"

mapfile -t s3_test_env < <(lash_s3_test_env "$s3_port")
# shellcheck disable=SC2016 # "$1" is the inner shell's argument
env "${s3_test_env[@]}" \
  LASH_S3_PREFIX="runbooks/process-operations-${LASH_GATE_WORKTREE_SLUG}-$$" \
  bash -c 'cd crates/lash-s3-store && exec "$1" --nocapture' _ "$s3_store_tests" \
  2>&1 | tee "$artifact_dir/00-s3-conformance.log" | tee -a "$test_output"

postgres_url="postgres://lash:lash@127.0.0.1:${postgres_port}/lash"
LASH_POSTGRES_DATABASE_URL="$postgres_url" \
  run_postgres_conformance_test wake_delivery::wake_delivery_crash_matrix \
  2>&1 | tee "$artifact_dir/01-wake-delivery.log" | tee -a "$test_output"
require_checkpoints "$artifact_dir/01-wake-delivery.log" \
  wake_discarded_target_gone wake_discarded_expired \
  blocked_group_redrive_lever blocked_group_cleared_after_redrive \
  reused_process_id_allocates_above_the_floor \
  rewound_sequence_is_discarded_without_blocking
echo "scenario 1 evidence: TargetGone and Expired typed discards plus blocked-head redrive passed on PostgreSQL" | tee -a "$test_output"
echo "scenario 6 evidence: prune/re-register delivered a strictly higher sequence; forced rewind surfaced sequence_rewound" | tee -a "$test_output"

"${compose[@]}" --profile crash run --rm crash-worker retarget \
  2>&1 | tee "$artifact_dir/02-retarget.jsonl" | tee -a "$test_output"
grep -q '"old_discard_reason":"retargeted"' "$artifact_dir/02-retarget.jsonl"
grep -q '"old_target_turn_count":0' "$artifact_dir/02-retarget.jsonl"
grep -q '"new_target_turn_count":1' "$artifact_dir/02-retarget.jsonl"
echo "scenario 2 evidence: old pending delivery is Retargeted with an audit event; one next wake reached only the new target" | tee -a "$test_output"

(cd crates/lash-core && "$core_lifecycle_tests" \
  process_tool_filter_narrows_only_session_tools_and_never_internal_wakes --nocapture) \
  2>&1 | tee "$artifact_dir/03-tool-visibility.log" | tee -a "$test_output"
(cd crates/lash && "$runtime_unit_tests" \
  process_admin_list_signal_and_cancel_bypass_model_tool_filter --nocapture) \
  2>&1 | tee -a "$artifact_dir/03-tool-visibility.log" | tee -a "$test_output"
require_checkpoints "$artifact_dir/03-tool-visibility.log" \
  model_tool_filter_narrows_without_narrowing_the_host_rail \
  host_admin_rail_bypasses_the_model_tool_filter
echo "scenario 3 evidence: model process tools were filtered while host list/signal/cancel remained complete" | tee -a "$test_output"

LASH_POSTGRES_DATABASE_URL="$postgres_url" \
  bash -c 'cd crates/lash-postgres-store && exec "$1" "$2" --nocapture --test-threads=1' _ \
  "$postgres_conformance_tests" queued_work_join_groups_by_delivery_policy_and_merge_key \
  2>&1 | tee "$artifact_dir/04-wake-turn-policy.log" | tee -a "$test_output"
require_checkpoints "$artifact_dir/04-wake-turn-policy.log" \
  queued_work_claims_join_by_policy_and_merge_key
echo "scenario 4 evidence: EachWake produced separate claims and Coalesce produced one multi-batch claim on PostgreSQL" | tee -a "$test_output"

LASH_POSTGRES_DATABASE_URL="$postgres_url" \
  run_postgres_conformance_test process_trigger_retention \
  2>&1 | tee "$artifact_dir/07-retention.log" | tee -a "$test_output"
require_checkpoints "$artifact_dir/07-retention.log" \
  prune_preserves_trigger_mutation_receipt \
  prune_reconciles_only_pruned_process_deliveries \
  outstanding_delivery_refuses_tombstone_compaction
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
  echo "panic gate: FAILED (a Rust panic marker found in process-operations E2E output)" >&2
  exit 1
fi
echo "panic gate: clean (no Rust panic markers in process-operations E2E output)" | tee -a "$test_output"
echo "process-operations e2e passed: scenarios=7 artifacts=$artifact_dir" | tee -a "$test_output"
