#!/usr/bin/env bash
set -euo pipefail

# Deterministic companion for runbooks/session-lease-triage. It induces the three
# situations the published stuck-turn triage procedure claims to distinguish
# (provider hang, lease takeover, commit-CAS livelock), captures both surfaces the
# procedure names, and asserts that each situation reads the way the docs say. A
# fourth phase runs the killed-worker recovery against a *direct* turn, which is
# recoverable at all only because direct ingress accepts before it drives.
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

# The lease trace transitions are contract, so their unit coverage is part of the
# companion rather than something the judged run takes on trust.
cargo test --locked --quiet -p lash-internal-core --lib session_lease_observability \
  2>&1 | tee "$artifact_dir/00-trace-event-tests.log" | tee -a "$test_output"
# The facade read and its host-side classification, exercised through the example
# that owns the operator endpoint.
cargo test --locked --quiet -p agent-service lease_triage \
  2>&1 | tee "$artifact_dir/01-facade-read-tests.log" | tee -a "$test_output"

harness hang 2>&1 | tee "$artifact_dir/02-provider-hang.jsonl" | tee -a "$test_output"
harness takeover 2>&1 | tee "$artifact_dir/03-lease-takeover.jsonl" | tee -a "$test_output"
harness livelock 2>&1 | tee "$artifact_dir/04-commit-cas-livelock.jsonl" | tee -a "$test_output"
harness direct-turn 2>&1 | tee "$artifact_dir/08-direct-turn-recovery.jsonl" | tee -a "$test_output"

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
    "session_execution_lease.acquired",
    "session_execution_lease.lost",
    "session_execution_lease.taken_over",
    "session_execution_lease.commit_cas_rejected",
    "session_execution_lease.busy",
    "session_execution_lease.busy_advisory",
    "session_execution_lease.busy_wait",
    "session_execution_lease.busy_gave_up",
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


def require_identity_fields(record, name, entry, require_generation=True):
    fields = ["session_id", "owner_id", "incarnation_id", "executor_id"]
    if require_generation:
        fields.append("fencing_token")
    for field in fields:
        if entry.get(field) in (None, ""):
            fail(
                f"{record['checkpoint']}/{record['backend']}: {name!r} omits {field!r}: {entry}"
            )


# Phase 1: a healthy holder is the provider-hang shape, and it is silent.
hang_records = checkpoints("provider_hang_shape", "02-provider-hang.jsonl")
for backend, record in hang_records.items():
    claimed = event(record, "session_execution_lease.acquired")
    require_identity_fields(record, "claimed", claimed)
    if claimed["level"] != "INFO":
        fail(f"{backend}: claimed must be operator-visible, got {claimed['level']}")
    if not record["holder_matches_running_worker"]:
        fail(f"{backend}: the reading did not name the worker running the parked turn: {record}")
    if not record["renewals_current_while_parked"]:
        fail(f"{backend}: a parked turn's lane must read as current: {record}")
    if record["renewed_count"] < 1:
        fail(f"{backend}: current is meaningful only after a renewal landed: {record}")
    if record["reading_while_parked"]["renewal"] != "current":
        fail(f"{backend}: parked reading was not current: {record['reading_while_parked']}")
    if not record["reading_while_parked"]["expires_in_ms"]:
        fail(f"{backend}: a current reading must carry positive headroom: {record}")
    if record["lease_lost_count"] or record["taken_over_count"]:
        fail(f"{backend}: a healthy lane must emit no lease-loss events: {record}")
    if record["commit_cas_rejected_count"]:
        fail(f"{backend}: the only writer must not lose the head CAS: {record}")
    if not record["turn_committed_after_release"]:
        fail(f"{backend}: the released turn did not commit: {record}")
    if record["reading_after_commit"]["renewal"] != "unheld":
        fail(f"{backend}: a committed turn must release its lane: {record['reading_after_commit']}")

# Phase 2: the winner reports the takeover truthfully, and the dead holder
# reports nothing at all. The second half is the whole point: an event emitted by
# the displaced runner would be absent here.
takeover_records = checkpoints("lease_takeover", "03-lease-takeover.jsonl")
for backend, record in takeover_records.items():
    taken_over = event(record, "session_execution_lease.taken_over")
    require_identity_fields(record, "taken_over", taken_over)
    if taken_over["level"] != "INFO":
        fail(f"{backend}: taken_over must be operator-visible, got {taken_over['level']}")
    if record["taken_over_count"] != 1:
        fail(f"{backend}: exactly one takeover must be reported: {record}")
    if record["lease_lost_count"]:
        fail(
            f"{backend}: the abandoned holder runs nothing and must report nothing, so a "
            f"session_execution_lease.lost here means the scenario is not testing a dead loser: {record}"
        )
    if taken_over["owner_id"] != record["successor_owner_id"]:
        fail(f"{backend}: the takeover was not emitted by the winner: {taken_over}")
    if taken_over["displaced_owner_id"] != record["abandoned_owner_id"]:
        fail(f"{backend}: the takeover named the wrong displaced holder: {taken_over}")
    if taken_over["displaced_fencing_token"] != record["abandoned_generation"]:
        fail(f"{backend}: the takeover named the wrong displaced generation: {taken_over}")
    if taken_over["fencing_token"] <= taken_over["displaced_fencing_token"]:
        fail(f"{backend}: takeover did not advance the generation: {taken_over}")
    if taken_over["displaced_owner_id"] == taken_over["owner_id"]:
        fail(f"{backend}: a claim reported displacing itself: {taken_over}")
    before = record["reading_before_takeover"]
    after = record["reading_after_takeover"]
    if before["holder_owner_id"] != record["abandoned_owner_id"]:
        fail(f"{backend}: the pre-takeover reading named the wrong holder: {before}")
    if before["renewal"] != "lapsed":
        fail(f"{backend}: an abandoned lane must read as lapsed: {before}")
    # After the sweep the abandoned holder must be gone from the read. The lane is
    # either held by the successor or already unheld, because a committing turn
    # releases it; both are correct and the run records which happened.
    if after["holder_owner_id"] == record["abandoned_owner_id"]:
        fail(f"{backend}: the operator read still names the displaced holder: {after}")
    if after["renewal"] not in ("unheld", "current"):
        fail(f"{backend}: unexpected post-sweep reading: {after}")
    if after["fencing_token"] is not None and after["fencing_token"] <= before["fencing_token"]:
        fail(f"{backend}: a still-held lane must show a higher generation: {before} -> {after}")
    if not record["turn_committed_after_takeover"]:
        fail(f"{backend}: the successor turn must commit after takeover: {record}")
    if record["turn_error_after_takeover"] is not None or record["commit_cas_rejected_count"]:
        fail(f"{backend}: the committed successor cannot also report an error or CAS loss: {record}")

# Phase 3: livelock is *repeated* rejection under sustained misrouting, while the
# writer still holds a lane. One collision is ordinary contention and is not what
# the documented decision procedure keys on.
livelock_records = checkpoints("commit_cas_livelock", "04-commit-cas-livelock.jsonl")
for backend, record in livelock_records.items():
    if record["rounds_attempted"] < 2:
        fail(f"{backend}: recurrence needs more than one round: {record}")
    if record["rounds_with_a_rejection"] != record["rounds_attempted"]:
        fail(
            f"{backend}: misrouting must keep colliding to be livelock rather than a one-off; "
            f"only {record['rounds_with_a_rejection']}/{record['rounds_attempted']} rounds "
            f"produced a rejection: {record}"
        )
    if record["commit_cas_rejected_count"] < record["rounds_attempted"]:
        fail(f"{backend}: expected at least one rejection per round: {record}")
    if record["busy_advisory_count"] != record["rounds_attempted"]:
        fail(f"{backend}: every busy claimant must proceed under the commit CAS: {record}")
    if record["busy_wait_count"] or record["busy_gave_up_count"]:
        fail(f"{backend}: an ordinary busy turn must neither wait nor give up: {record}")
    for advisory in record["busy_advisory"]:
        if advisory["level"] != "INFO":
            fail(f"{backend}: commit_busy_advisory must be INFO, got {advisory}")
        for field in (
            "session_id",
            "holder_owner_id_sha256",
            "holder_incarnation_id_sha256",
            "holder_executor_id_sha256",
        ):
            if not advisory.get(field):
                fail(f"{backend}: real busy advisory omits {field!r}: {advisory}")
        for field in ("generation", "fencing_token", "holder_fencing_token"):
            if field in advisory:
                fail(f"{backend}: real busy advisory must not expose {field!r}: {advisory}")
        if advisory["outcome"] != "proceeding_under_commit_cas":
            fail(f"{backend}: busy claimant recorded the wrong disposition: {advisory}")
    for round_record in record["rounds"]:
        if round_record["committed"] != 1:
            fail(f"{backend}: each round must have exactly one winner: {round_record}")
        if not round_record["loser_rejected"]:
            fail(f"{backend}: a round's stale writer was accepted: {round_record}")
    for rejected in record["commit_cas_rejected"]:
        require_identity_fields(record, "commit_cas_rejected", rejected)
        if rejected["level"] != "WARN":
            fail(f"{backend}: commit_cas_rejected must warn, got {rejected['level']}")
        if rejected["lease_lost"] is not False:
            fail(f"{backend}: livelock is a rejection while the lane is still held: {rejected}")
        if rejected["lane_held"] is not True:
            fail(f"{backend}: the parked CAS loser must still hold its lane: {rejected}")
        if rejected["actual_head_revision"] <= rejected["expected_head_revision"]:
            fail(f"{backend}: the rejection did not name a head that had moved on: {rejected}")
    if record["lease_lost_count"] or record["taken_over_count"]:
        fail(f"{backend}: livelock must be distinguishable from a handoff: {record}")


# Phase 4: the killed-worker recovery, run against a turn that entered through
# `TurnBuilder::run`. Acceptance-before-drive is what makes it recoverable: the
# request is a pending row while the provider is still parked, and the peer that
# takes the lane finds it through the ordinary queued drain.
direct_turn_records = checkpoints("direct_turn_recovery", "08-direct-turn-recovery.jsonl")
for backend, record in direct_turn_records.items():
    if not record["seed_acceptance_input_id"]:
        fail(f"{backend}: a direct turn must report the acceptance it was admitted under: {record}")
    if record["seed_acceptance_source_key"] is not None:
        fail(
            f"{backend}: direct ingress must not mint an idempotency key of its own: {record}"
        )
    if not record["seed_acceptance_settled"]:
        fail(f"{backend}: the reported acceptance is not the input that settled: {record}")
    if record["claimable_while_parked"] != 0:
        fail(
            f"{backend}: the input a parked direct turn is driving is held by its own claim, so "
            f"nothing may be claimable while it runs: {record}"
        )
    if not record["drain_ran"]:
        fail(
            f"{backend}: an orphaned direct-turn input must be claimable by an unrelated worker; "
            f"the drain ran nothing ({record['drain_empty_reason']}): {record}"
        )
    if not record["recovered_turn_committed"]:
        fail(f"{backend}: the recovering worker did not commit the turn: {record}")
    if record["recovered_application_turn_id"] is None:
        fail(f"{backend}: the recovered input never settled as canonical input: {record}")
    if not (record["recovered_input_id"] or "").startswith("ti:"):
        fail(f"{backend}: the recovered row is not a pending turn input: {record}")
    if record["recovered_application_turn_id"] == record["abandoned_turn_id"]:
        fail(
            f"{backend}: the successor must commit its own turn, not the abandoned driver's: "
            f"{record}"
        )
    if record["pending_after_recovery"]:
        fail(f"{backend}: recovery must settle the row rather than leave it claimable: {record}")

# One normalized law artifact makes backend agreement reviewable as a single
# row rather than requiring a reader to mentally join three phase files.
dispositions = {}
for backend in backends:
    hang = hang_records[backend]
    takeover = takeover_records[backend]
    busy = livelock_records[backend]
    direct = direct_turn_records[backend]
    dispositions[backend] = {
        "provider_hang": {
            "renewal": hang["reading_while_parked"]["renewal"],
            "renewed": hang["renewed_count"] > 0,
            "lease_trouble_counts": {
                "lost": hang["lease_lost_count"],
                "taken_over": hang["taken_over_count"],
                "commit_cas_rejected": hang["commit_cas_rejected_count"],
            },
            "after_commit": hang["reading_after_commit"]["renewal"],
        },
        "successor_takeover": {
            "event_level": takeover["taken_over"]["level"],
            "outcome": takeover["taken_over"]["outcome"],
            "displaced_owner_id": takeover["taken_over"]["displaced_owner_id"],
            "displaced_fencing_token": takeover["taken_over"]["displaced_fencing_token"],
            "lease_lost_count": takeover["lease_lost_count"],
            "before": takeover["reading_before_takeover"]["renewal"],
            "turn_committed": takeover["turn_committed_after_takeover"],
        },
        "busy_lane": {
            "advisory_outcomes": [event["outcome"] for event in busy["busy_advisory"]],
            "busy_wait_count": busy["busy_wait_count"],
            "busy_gave_up_count": busy["busy_gave_up_count"],
            "rejected_lane_held": [event["lane_held"] for event in busy["commit_cas_rejected"]],
            "rejected_lease_lost": [event["lease_lost"] for event in busy["commit_cas_rejected"]],
        },
        "direct_turn_recovery": {
            "claimable_while_parked": direct["claimable_while_parked"],
            "drain_ran": direct["drain_ran"],
            "recovered_turn_committed": direct["recovered_turn_committed"],
            "pending_after_recovery": direct["pending_after_recovery"],
            "acceptance_source_key": direct["seed_acceptance_source_key"],
        },
    }

normalized = {json.dumps(value, sort_keys=True) for value in dispositions.values()}
if len(normalized) != 1:
    fail(f"backend recovery dispositions disagree: {dispositions}")
(artifacts / "07-executor-recovery-law.json").write_text(
    json.dumps(
        {
            "schema": "lash.session-execution-lease-recovery-law.v1",
            "backends": backends,
            "dispositions": dispositions,
            "correction": "FIG-1380",
        },
        indent=2,
        sort_keys=True,
    )
    + "\n",
    encoding="utf-8",
)

print(
    "session-lease-triage gates: provider hang, winner-emitted takeover of a dead holder, "
    "recurring CAS livelock, and direct-turn recovery after a killed worker asserted on "
    f"{', '.join(backends)}; recovery dispositions observed"
)
PY

if grep -Fn 'panicked at' "$test_output" >&2; then
  echo "panic gate: FAILED ('panicked at' found in session-lease-triage E2E output)" >&2
  exit 1
fi
echo "panic gate: clean (no 'panicked at' lines in session-lease-triage E2E output)" | tee -a "$test_output"
echo "session-lease-triage e2e passed: scenarios=4 backends=$backends artifacts=$artifact_dir" | tee -a "$test_output"
