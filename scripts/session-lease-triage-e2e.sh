#!/usr/bin/env bash
set -euo pipefail

# Deterministic companion for runbooks/session-lease-triage. It induces the three
# situations the published stuck-turn triage procedure claims to distinguish
# (provider hang, lease takeover, commit-CAS livelock), captures both surfaces the
# procedure names, and asserts that each situation reads the way the docs say. A
# fourth phase runs the killed-worker recovery against a *direct* turn, which is
# recoverable at all only because direct ingress accepts before it drives.
#
# Every phase runs on SQLite. PostgreSQL is storage only (ADR 0104), so its
# session lease is exercised under Restate by the workers E2E. The companion
# owns no container and no host port.

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

# Built through Bazel so this companion shares the box's action cache with every
# other checkout instead of compiling the workspace again into its own Cargo
# target directory. `cargo build` without a profile and Bazel's default
# `fastbuild` are both `-C opt-level=0` with debug assertions on, so the geometry
# the phases run under is unchanged.
symlink_prefix="$artifact_dir/bazel-"
if command -v kiln >/dev/null 2>&1; then
  # A kiln fork: build and test through the shared executor.
  kiln build "--symlink_prefix=$symlink_prefix" \
    //runbooks/restate-postgres-workers:lash-e2e-session-lease-triage__bin \
    2>&1 | tee "$artifact_dir/build.log"
  harness_bin="${symlink_prefix}bin/runbooks/restate-postgres-workers/lash-e2e-session-lease-triage__bin"
  [ -x "$harness_bin" ] || {
    echo "session-lease-triage harness binary is missing at $harness_bin" >&2
    exit 1
  }
  harness() {
    "$harness_bin" "$1"
  }
  trace_event_tests() {
    kiln test "--symlink_prefix=$symlink_prefix" --test_output=all \
      --test_arg=session_lease_observability \
      //crates/lash-core:runtime_observability__test
  }
  facade_read_tests() {
    kiln test "--symlink_prefix=$symlink_prefix" --test_output=all \
      --test_arg=lease_triage \
      //examples/agent-service:agent-service__unit_test
  }
else
  # No kiln on this host (the GitHub runner has neither kiln nor a shared
  # executor): the same targets through cargo, the geometry the CI leg has
  # always run under.
  cargo build --locked --quiet -p lash-restate-postgres-workers-e2e \
    --bin lash-e2e-session-lease-triage 2>&1 | tee "$artifact_dir/build.log"
  harness() {
    cargo run --locked --quiet -p lash-restate-postgres-workers-e2e \
      --bin lash-e2e-session-lease-triage -- "$1"
  }
  trace_event_tests() {
    cargo test --locked --quiet -p lash-internal-core --test runtime_observability \
      session_lease_observability
  }
  facade_read_tests() {
    cargo test --locked --quiet -p agent-service lease_triage
  }
fi

backends="sqlite"
echo "session-lease-triage backends: $backends" | tee "$test_output"

# The lease trace transitions are contract, so their unit coverage is part of the
# companion rather than something the judged run takes on trust. Both legs print
# each test name: a companion whose own unit gates report only a count cannot be
# read for which transitions were actually covered.
trace_event_tests \
  2>&1 | tee "$artifact_dir/00-trace-event-tests.log" | tee -a "$test_output"
# The facade read and its host-side classification, exercised through the example
# that owns the operator endpoint.
facade_read_tests \
  2>&1 | tee "$artifact_dir/01-facade-read-tests.log" | tee -a "$test_output"

harness hang 2>&1 | tee "$artifact_dir/02-provider-hang.jsonl" | tee -a "$test_output"
harness takeover 2>&1 | tee "$artifact_dir/03-lease-takeover.jsonl" | tee -a "$test_output"
harness livelock 2>&1 | tee "$artifact_dir/04-commit-cas-livelock.jsonl" | tee -a "$test_output"
harness direct-turn 2>&1 | tee "$artifact_dir/08-direct-turn-recovery.jsonl" | tee -a "$test_output"

python3 - "$artifact_dir" "$backends" <<'PY'
import json
import sys
from pathlib import Path

artifacts = Path(sys.argv[1])
backends = sys.argv[2].split(",")
# TypeScript is the sole RLM language (ADR 0096): the harness records it
# unconditionally, so the gate pins the literal rather than an environment read.
expected_dialect = "typescript"

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
    for backend, record in found.items():
        if record.get("dialect") != expected_dialect:
            fail(
                f"{name}/{backend}: checkpoint did not record served dialect "
                f"{expected_dialect!r}: {record}"
            )
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
# request is a visible held row while the provider is still parked, and the peer that
# takes the lane finds it through the ordinary queued drain.
direct_turn_records = checkpoints("direct_turn_recovery", "08-direct-turn-recovery.jsonl")
for backend, record in direct_turn_records.items():
    if not record["seed_acceptance_input_id"]:
        fail(f"{backend}: a direct turn must report the acceptance it was admitted under: {record}")
    # A direct turn's accepted row is keyed by its own turn id (FIG-3600): a
    # redrive of the turn names the same row, and direct ingress mints no key
    # of its own.
    if record["seed_acceptance_source_key"] != record["seed_turn_id"]:
        fail(
            f"{backend}: direct ingress keys its acceptance by its turn id, never a key of "
            f"its own: {record}"
        )
    if not record["seed_acceptance_settled"]:
        fail(f"{backend}: the reported acceptance is not the input that settled: {record}")
    pending_reads = record["pending_reads_while_parked"]
    if len(pending_reads) != 1:
        fail(
            f"{backend}: the parked direct turn must expose exactly one held input: {record}"
        )
    parked = pending_reads[0]
    parked_input = parked.get("input", {})
    parked_status = parked.get("status", {})
    parked_input_id = parked_input.get("input_id")
    if not (parked_input_id or "").startswith("ti:"):
        fail(f"{backend}: the parked read lost pending-input identity: {parked}")
    if parked_input.get("session_id") != record["session_id"]:
        fail(f"{backend}: the parked read names the wrong session: {parked}")
    if parked_status.get("kind") != "held":
        fail(f"{backend}: the parked direct-turn input is not projected held: {parked}")
    parked_expiry = parked_status.get("lease_expires_at_ms")
    if not isinstance(parked_expiry, int) or isinstance(parked_expiry, bool) or parked_expiry <= 0:
        fail(f"{backend}: the held projection lacks an exact lease expiry: {parked}")
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
    if record["recovered_input_id"] != parked_input_id:
        fail(f"{backend}: recovery settled a different input than the parked held row: {record}")
    if record["recovered_application_turn_id"] == record["abandoned_turn_id"]:
        fail(
            f"{backend}: the successor must commit its own turn, not the abandoned driver's: "
            f"{record}"
        )
    if record["pending_after_recovery"]:
        fail(f"{backend}: recovery must settle the row rather than leave it claimable: {record}")
    # The recovery has to face a dead holder. A lane that was released before the
    # successor claimed it is the easy, uncontested case, and a phase that only
    # reported "the turn committed" passed under exactly that shape (FIG-3160).
    if record["abandoned_lane_released_before_takeover"]:
        fail(
            f"{backend}: the abandoned lane was released before the successor acquired it, so "
            f"the recovery never took anything over: {record}"
        )
    if not record["taken_over_from_dead_worker_count"]:
        fail(f"{backend}: the recovery drain recorded no takeover from the dead worker: {record}")
    taken_over = record["taken_over_from_dead_worker"]
    if taken_over.get("displaced_owner_id") != record["abandoned_owner_id"]:
        fail(f"{backend}: the takeover names the wrong displaced holder: {taken_over}")
    if taken_over.get("displaced_fencing_token") != record["abandoned_fencing_token"]:
        fail(f"{backend}: the takeover names the wrong displaced generation: {taken_over}")
    if taken_over.get("owner_id") != record["successor_owner_id"]:
        fail(f"{backend}: a takeover is the winner's event, not the dead holder's: {taken_over}")
    if taken_over.get("fencing_token", 0) <= record["abandoned_fencing_token"]:
        fail(f"{backend}: the successor did not fence the abandoned generation: {taken_over}")

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
            "pending_reads_while_parked": len(direct["pending_reads_while_parked"]),
            "parked_status": direct["pending_reads_while_parked"][0]["status"]["kind"],
            "parked_has_exact_expiry": (
                isinstance(
                    direct["pending_reads_while_parked"][0]["status"].get(
                        "lease_expires_at_ms"
                    ),
                    int,
                )
                and not isinstance(
                    direct["pending_reads_while_parked"][0]["status"].get(
                        "lease_expires_at_ms"
                    ),
                    bool,
                )
                and direct["pending_reads_while_parked"][0]["status"][
                    "lease_expires_at_ms"
                ]
                > 0
            ),
            "drain_ran": direct["drain_ran"],
            "recovered_turn_committed": direct["recovered_turn_committed"],
            "pending_after_recovery": direct["pending_after_recovery"],
            "acceptance_source_key": direct["seed_acceptance_source_key"],
            "abandoned_lane_released_before_takeover": direct[
                "abandoned_lane_released_before_takeover"
            ],
            "taken_over_from_dead_worker_count": direct["taken_over_from_dead_worker_count"],
            "takeover_outcome": direct["taken_over_from_dead_worker"]["outcome"],
            "takeover_displaced_owner_id": direct["taken_over_from_dead_worker"][
                "displaced_owner_id"
            ],
            "takeover_fences_abandoned_generation": (
                direct["taken_over_from_dead_worker"]["fencing_token"]
                > direct["abandoned_fencing_token"]
            ),
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
  echo "panic gate: FAILED (a Rust panic marker found in session-lease-triage E2E output)" >&2
  exit 1
fi
echo "panic gate: clean (no Rust panic markers in session-lease-triage E2E output)" | tee -a "$test_output"
echo "session-lease-triage e2e passed: scenarios=4 backends=$backends artifacts=$artifact_dir" | tee -a "$test_output"
