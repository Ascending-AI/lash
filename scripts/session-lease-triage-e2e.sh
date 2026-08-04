#!/usr/bin/env bash
set -euo pipefail

# Deterministic companion for runbooks/session-lease-triage. It induces the three
# situations the published stuck-turn triage procedure claims to distinguish
# (provider hang, lease takeover, commit-CAS livelock), captures both surfaces the
# procedure names, and asserts that each situation reads the way the docs say.
#
# PostgreSQL is optional. Set LASH_POSTGRES_DATABASE_URL to run every phase on a
# shared substrate as well as SQLite; the companion owns no container and no host
# port, so it never serializes against another worktree's assigned service.

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

if [ -n "${LASH_SESSION_LEASE_ARTIFACT_DIR:-}" ]; then
  artifact_dir="$LASH_SESSION_LEASE_ARTIFACT_DIR"
else
  artifact_dir="$(mktemp -d "${TMPDIR:-/tmp}/lash-session-lease-triage.XXXXXX")"
fi
mkdir -p "$artifact_dir"
test_output="$artifact_dir/session-lease-triage-e2e.log"

on_exit() {
  status=$?
  if [ "$status" -ne 0 ]; then
    echo "session-lease-triage E2E failed with status $status; artifacts: $artifact_dir" >&2
  fi
  exit "$status"
}
trap on_exit EXIT

harness() {
  cargo run --locked --quiet -p lash-restate-postgres-workers-e2e \
    --bin lash-e2e-session-lease-triage -- "$1"
}

backends="sqlite"
if [ -n "${LASH_POSTGRES_DATABASE_URL:-}" ]; then
  backends="sqlite,postgres"
fi
echo "session-lease-triage backends: $backends" | tee "$test_output"

# The four trace events are contract, so their unit coverage is part of the
# companion rather than something the judged run takes on trust.
cargo test --locked --quiet -p lash-core --lib session_lease_observability \
  2>&1 | tee "$artifact_dir/00-trace-event-tests.log" | tee -a "$test_output"
# The facade read and its host-side classification, exercised through the example
# that owns the operator endpoint.
cargo test --locked --quiet -p agent-service lease_triage \
  2>&1 | tee "$artifact_dir/01-facade-read-tests.log" | tee -a "$test_output"

harness hang 2>&1 | tee "$artifact_dir/02-provider-hang.jsonl" | tee -a "$test_output"
harness takeover 2>&1 | tee "$artifact_dir/03-lease-takeover.jsonl" | tee -a "$test_output"
harness livelock 2>&1 | tee "$artifact_dir/04-commit-cas-livelock.jsonl" | tee -a "$test_output"

# The documented procedure and its compiled snippet must still agree with the
# sources, so a docs-vs-observed judgment has something stable to score against.
python3 scripts/lint_docs.py 2>&1 | tee "$artifact_dir/05-docs-lint.log" | tee -a "$test_output"

python3 - "$artifact_dir" "$backends" <<'PY'
import json
import sys
from pathlib import Path

artifacts = Path(sys.argv[1])
backends = sys.argv[2].split(",")

LEASE_EVENTS = (
    "session_execution_lease.claimed",
    "session_execution_lease.renew_failed",
    "session_execution_lease.taken_over",
    "session_execution_lease.commit_cas_rejected",
)


def fail(message):
    raise SystemExit(f"session-lease-triage gate failed: {message}")


def checkpoints(name, filename):
    path = artifacts / filename
    found = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if value.get("checkpoint") == name:
            found[value["backend"]] = value
    for backend in backends:
        if backend not in found:
            fail(f"missing {name!r} checkpoint for backend {backend!r} in {path}")
    return found


def event(record, name):
    matched = [
        entry for entry in record["lease_trace"] if entry.get("event") == name
    ]
    if not matched:
        fail(f"{record['checkpoint']}/{record['backend']}: no {name!r} trace event")
    return matched[0]


def require_identity_fields(record, name, entry):
    for field in ("session_id", "generation", "owner_id", "incarnation_id"):
        if entry.get(field) in (None, ""):
            fail(
                f"{record['checkpoint']}/{record['backend']}: {name!r} omits {field!r}: {entry}"
            )


# Phase 1: a healthy holder is the provider-hang shape, and it is silent.
for backend, record in checkpoints("provider_hang_shape", "02-provider-hang.jsonl").items():
    claimed = event(record, "session_execution_lease.claimed")
    require_identity_fields(record, "claimed", claimed)
    if claimed["level"] != "INFO":
        fail(f"{backend}: claimed must be operator-visible, got {claimed['level']}")
    if not record["holder_matches_running_worker"]:
        fail(f"{backend}: the reading did not name the worker running the parked turn: {record}")
    if not record["renewals_current_while_parked"]:
        fail(f"{backend}: a parked turn's lane must read as current: {record}")
    if record["reading_while_parked"]["renewal"] != "current":
        fail(f"{backend}: parked reading was not current: {record['reading_while_parked']}")
    if not record["reading_while_parked"]["expires_in_ms"]:
        fail(f"{backend}: a current reading must carry positive headroom: {record}")
    if record["renew_failed_count"] or record["taken_over_count"]:
        fail(f"{backend}: a healthy lane must emit no lease-loss events: {record}")
    if record["commit_cas_rejected_count"]:
        fail(f"{backend}: the only writer must not lose the head CAS: {record}")
    if not record["turn_committed_after_release"]:
        fail(f"{backend}: the released turn did not commit: {record}")
    if record["reading_after_commit"]["renewal"] != "unheld":
        fail(f"{backend}: a committed turn must release its lane: {record['reading_after_commit']}")

# Phase 2: a takeover is two ordered events plus a higher generation, and the
# displaced turn's fate is recorded rather than assumed.
for backend, record in checkpoints("lease_takeover", "03-lease-takeover.jsonl").items():
    renew_failed = event(record, "session_execution_lease.renew_failed")
    taken_over = event(record, "session_execution_lease.taken_over")
    require_identity_fields(record, "renew_failed", renew_failed)
    require_identity_fields(record, "taken_over", taken_over)
    if renew_failed["level"] != "WARN":
        fail(f"{backend}: renew_failed must warn, got {renew_failed['level']}")
    if taken_over["level"] != "INFO":
        fail(f"{backend}: taken_over must be operator-visible, got {taken_over['level']}")
    if not record["renew_failed_before_taken_over"]:
        fail(f"{backend}: the timeline did not order renew_failed before taken_over: {record}")
    if renew_failed["generation"] != taken_over["generation"]:
        fail(f"{backend}: the two events name different displaced generations: {record}")
    if taken_over["superseding_generation"] <= taken_over["generation"]:
        fail(f"{backend}: takeover did not advance the generation: {taken_over}")
    if taken_over["superseding_owner_id"] == taken_over["owner_id"]:
        fail(f"{backend}: takeover named the displaced holder as its own successor: {taken_over}")
    if record["superseding_generation"] <= record["displaced_generation"]:
        fail(f"{backend}: the swept lane did not advance the durable generation: {record}")
    before = record["reading_before_takeover"]
    after = record["reading_after_takeover"]
    if before["holder_owner_id"] != record["displaced_owner_id"]:
        fail(f"{backend}: the pre-takeover reading named the wrong holder: {before}")
    if after["holder_owner_id"] != record["successor_owner_id"]:
        fail(f"{backend}: the post-takeover reading did not name the successor: {after}")
    if after["generation"] <= before["generation"]:
        fail(f"{backend}: the operator read did not show a higher generation: {before} -> {after}")
    # The doctrine under test: lease loss is not a turn verdict. Whichever way
    # this turn settled, the run must record it and it must be self-consistent.
    if record["turn_committed_after_lease_loss"] and record["commit_cas_rejected_count"]:
        fail(f"{backend}: a committed turn cannot also have lost the head CAS: {record}")
    if not record["turn_committed_after_lease_loss"] and not record["turn_error_after_lease_loss"]:
        fail(f"{backend}: the displaced turn neither committed nor reported an error: {record}")

# Phase 3: livelock is a rejected commit while the writer still holds a lane.
for backend, record in checkpoints("commit_cas_livelock", "04-commit-cas-livelock.jsonl").items():
    if not record["winner_committed"]:
        fail(f"{backend}: the first writer did not commit: {record}")
    if not record["loser_rejected"]:
        fail(f"{backend}: the stale writer's commit was accepted: {record}")
    rejected = event(record, "session_execution_lease.commit_cas_rejected")
    require_identity_fields(record, "commit_cas_rejected", rejected)
    if rejected["level"] != "WARN":
        fail(f"{backend}: commit_cas_rejected must warn, got {rejected['level']}")
    if rejected["lease_lost"] is not False:
        fail(f"{backend}: livelock is a rejection while the lane is still held: {rejected}")
    if rejected["actual_head_revision"] <= rejected["expected_head_revision"]:
        fail(f"{backend}: the rejection did not name a head that had moved on: {rejected}")
    if record["renew_failed_count"] or record["taken_over_count"]:
        fail(f"{backend}: livelock must be distinguishable from a handoff: {record}")

print(
    "session-lease-triage gates: provider hang, takeover ordering, and CAS livelock "
    f"asserted on {', '.join(backends)}; all four lease events observed"
)
PY

if grep -Fn 'panicked at' "$test_output" >&2; then
  echo "panic gate: FAILED ('panicked at' found in session-lease-triage E2E output)" >&2
  exit 1
fi
echo "panic gate: clean (no 'panicked at' lines in session-lease-triage E2E output)" | tee -a "$test_output"
echo "session-lease-triage e2e passed: scenarios=3 backends=$backends artifacts=$artifact_dir" | tee -a "$test_output"
