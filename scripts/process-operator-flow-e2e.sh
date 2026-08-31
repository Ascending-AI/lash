#!/usr/bin/env bash
set -euo pipefail

scenario="${1:-}"
case "$scenario" in
  drain)
    runbook_name="graceful-drain"
    port="${LASH_GRACEFUL_DRAIN_POSTGRES_PORT:-5547}"
    artifact_override="${LASH_GRACEFUL_DRAIN_ARTIFACT_DIR:-}"
    contract_filter="owner_bound_graceful_drain_resolves_awaiter_and_prunes_end_to_end"
    ;;
  request-abandon)
    runbook_name="request-abandon"
    port="${LASH_REQUEST_ABANDON_POSTGRES_PORT:-5548}"
    artifact_override="${LASH_REQUEST_ABANDON_ARTIFACT_DIR:-}"
    contract_filter="silent_owner_stays_running_then_abandon_request_reconciles_end_to_end"
    ;;
  *)
    echo "usage: scripts/process-operator-flow-e2e.sh drain|request-abandon" >&2
    exit 2
    ;;
esac

if ! [[ "$port" =~ ^[0-9]+$ ]] || ((10#$port < 5540 || 10#$port > 5599)); then
  echo "${runbook_name} PostgreSQL port must be an integer in 5540..5599, got '${port}'." >&2
  exit 2
fi

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"
lash_gate_acquire "${runbook_name}-e2e"

if [ -n "$artifact_override" ]; then
  artifact_dir="$artifact_override"
else
  artifact_dir="$(mktemp -d "${TMPDIR:-/tmp}/lash-${runbook_name}-${LASH_GATE_WORKTREE_SLUG}.XXXXXX")"
fi
mkdir -p "$artifact_dir"
test_output="$artifact_dir/${runbook_name}-e2e.log"
container="${LASH_PROCESS_OPERATOR_POSTGRES_CONTAINER:-lash-fig897-${runbook_name}-postgres}"

cleanup() {
  status=$?
  docker rm -f "$container" >/dev/null 2>&1 || true
  lash_gate_cleanup
  if [ "$status" -ne 0 ]; then
    echo "${runbook_name} E2E failed with status $status; artifacts: $artifact_dir" >&2
  fi
  exit "$status"
}
trap cleanup EXIT

docker rm -f "$container" >/dev/null 2>&1 || true
docker run -d --rm \
  --name "$container" \
  --label "$LASH_GATE_LABEL" \
  -e POSTGRES_USER=lash \
  -e POSTGRES_PASSWORD=lash \
  -e POSTGRES_DB=lash \
  -p "127.0.0.1:${port}:5432" \
  postgres:16-alpine >"$artifact_dir/00-container-id.txt"

deadline=$((SECONDS + 90))
until docker exec "$container" pg_isready -U lash -d lash >/dev/null 2>&1; do
  if ((SECONDS >= deadline)); then
    docker logs "$container" >&2 || true
    echo "PostgreSQL did not become ready on port $port" >&2
    exit 1
  fi
  sleep 1
done
docker inspect "$container" >"$artifact_dir/00-container.json"
docker ps --filter "name=^/${container}$" --format json >"$artifact_dir/00-live-services.json"
printf '{"container":"%s","host":"127.0.0.1","port":%s}\n' \
  "$container" "$port" >"$artifact_dir/00-postgres.json"

echo "${runbook_name} backend: postgres:${port}" | tee "$test_output"
cargo test --locked -p lash-runtime --features rlm "$contract_filter" \
  2>&1 | tee "$artifact_dir/01-contract-tests.log" | tee -a "$test_output"
if ! grep -Eq 'test result: ok\. 1 passed' "$artifact_dir/01-contract-tests.log"; then
  echo "focused contract filter did not execute exactly one passing test" >&2
  exit 1
fi
DATABASE_URL="postgres://lash:lash@127.0.0.1:${port}/lash" \
  cargo run --locked --release --quiet -p lash-restate-postgres-workers-e2e \
    --bin lash-e2e-process-operator-flow -- "$scenario" \
  2>&1 | tee "$artifact_dir/03-observed.jsonl" | tee -a "$test_output"

python3 - "$scenario" "$artifact_dir" <<'PY'
import json
import os
import sys
from pathlib import Path

scenario = sys.argv[1]
artifacts = Path(sys.argv[2])
expected_dialect = os.environ.get("LASH_RUNBOOK_DIALECT", "lashlang")


def fail(message):
    raise SystemExit(f"{scenario} gate failed: {message}")


def checkpoint(name):
    for line in (artifacts / "03-observed.jsonl").read_text(encoding="utf-8").splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if value.get("checkpoint") == name:
            return value
    fail(f"missing checkpoint {name!r}")


if scenario == "drain":
    seed = checkpoint("seeded_drain_deployment")
    observed = checkpoint("graceful_drain_observed")
    if seed.get("dialect") != expected_dialect:
        fail(
            f"seeded checkpoint did not record served dialect {expected_dialect!r}: {seed}"
        )
    if seed["provider_calls"] != 1 or not seed["journal_active"]:
        fail(f"fixture did not hold one in-flight effect: {seed}")
    for field in ("in_flight_effect_completed", "provider_closed", "trace_flushed"):
        if observed[field] is not True:
            fail(f"drain step {field} did not complete: {observed}")
    if observed["ingress_accepting"] or observed["new_turn_admitted"]:
        fail(f"ingress was not quiesced: {observed}")
    if observed["journal_active"]:
        fail(f"effect journal was not empty: {observed}")
    if not observed["journal_completed"]:
        fail(f"no completed effect evidence was recorded: {observed}")
    if observed["drain_report_abandoned"] != ["drain-owner-bound-mine"]:
        fail(f"drain touched the wrong rows: {observed}")
    if observed["drain_report_deferred"] != []:
        fail(f"drain left rows deferred: {observed}")
    if observed["drain_worker_faults"] != 0:
        fail(f"drain reported worker faults: {observed}")
    if observed["observer_abandon_writer"] != "OwnerDrain":
        fail(f"drain terminal carried the wrong writer: {observed}")
    rows = {row["process_id"]: row for row in observed["processes"]}
    expected = {
        "drain-owner-bound-mine": (True, "Abandoned"),
        "drain-rerunnable-mine": (False, "Running"),
        "drain-owner-bound-foreign": (False, "Running"),
        "drain-owner-bound-unstarted": (False, "Running"),
        "drain-externally-owned": (False, "Running"),
    }
    for process_id, (terminal, status) in expected.items():
        row = rows.get(process_id)
        if not row or row["terminal"] != terminal or row["status"] != status:
            fail(f"unexpected row {process_id}: {row}")
    print("graceful-drain gates: ingress quiesced, effect settled, journal empty, dispositions exact")
else:
    seed = checkpoint("seeded_request_abandon_deployment")
    pending = checkpoint("pending_abandon_request_visible")
    reconciled = checkpoint("abandon_request_reconciled")
    if seed["status"] != "Running" or seed["terminal"]:
        fail(f"fixture was not in flight: {seed}")
    if pending["returned_status"] != "Running" or pending["returned_terminal"]:
        fail(f"request terminalized the row: {pending}")
    if not pending["observer_marker_visible"] or not pending["lease_unchanged"]:
        fail(f"pending marker/lease contract failed: {pending}")
    for field in ("lease_token", "fencing_token", "lease_expires_at_ms"):
        if pending[field] != seed[field]:
            fail(f"request changed live lease field {field}: seed={seed}, pending={pending}")
    if reconciled["lapsed_before_sweep_status"] != "Running" or reconciled["lapsed_before_sweep_terminal"]:
        fail(f"lease lapse alone terminalized the row: {reconciled}")
    if reconciled["terminal_status"] != "Abandoned" or not reconciled["terminal"]:
        fail(f"sweep did not produce Abandoned: {reconciled}")
    if reconciled["abandon_writer"] != "ReconciledRequest":
        fail(f"wrong reconciliation writer: {reconciled}")
    if not reconciled["observer_terminal_visible"] or not reconciled["lease_cleared"]:
        fail(f"observer/lease terminal contract failed: {reconciled}")
    if reconciled["sweep_admitted"] < 1:
        fail(f"sweep admitted no rows: {reconciled}")
    if reconciled["sweep_worker_faults"] != 0:
        fail(f"sweep reported worker faults: {reconciled}")
    print("request-abandon gates: marker pending, live lease unchanged, lapse non-terminal, sweep reconciled")
PY

if grep -Fn 'panicked at' "$test_output" >&2; then
  echo "panic gate: FAILED ('panicked at' found in ${runbook_name} E2E output)" >&2
  exit 1
fi
echo "panic gate: clean (no 'panicked at' lines in ${runbook_name} E2E output)" | tee -a "$test_output"
echo "${runbook_name} e2e passed: scenarios=1 artifacts=$artifact_dir" | tee -a "$test_output"
