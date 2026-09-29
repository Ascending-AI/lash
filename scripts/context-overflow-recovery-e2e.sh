#!/usr/bin/env bash
set -euo pipefail

# Deterministic companion for runbooks/context-overflow-recovery (FIG-1272).
#
# No container, no token, no network beyond loopback: a SQLite scratch store
# set, a scripted provider and a local restate-server, the zero-infra effect
# engine `scripts/ci/with-service.sh restate` runs (ADR 0104 section 4).
# The RLM TypeScript row and standard-protocol row have separate artifact
# directories; the harness makes a fresh data directory for each session.

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"
lash_gate_acquire context-overflow-recovery-e2e

if [ -n "${LASH_CONTEXT_OVERFLOW_ARTIFACT_DIR:-}" ]; then
  artifact_root="$LASH_CONTEXT_OVERFLOW_ARTIFACT_DIR"
else
  artifact_root="$(mktemp -d "${TMPDIR:-/tmp}/lash-context-overflow-${LASH_GATE_WORKTREE_SLUG}.XXXXXX")"
fi
mkdir -p "$artifact_root"
run_log="$artifact_root/context-overflow-recovery-e2e.log"

cleanup() {
  status=$?
  lash_gate_cleanup
  if [ "$status" -ne 0 ]; then
    echo "context-overflow-recovery E2E failed with status $status; artifacts: $artifact_root" >&2
  fi
  exit "$status"
}
trap cleanup EXIT

# The mapping this scenario judges, proved in the kernel before a row is spent.
if [[ -f .kiln.bazelrc ]]; then
  kiln test --test_output=all \
    --test_arg=context_overflow_response_stops_as_its_own_outcome \
    //crates/lash-sansio:lash-sansio__unit_test \
    2>&1 | tee "$artifact_root/01-contract-tests.log" | tee "$run_log"
else
  # The portable CI runner has no Kiln fork or shared executor.
  cargo test --locked -p lash-internal-sansio \
    context_overflow_response_stops_as_its_own_outcome \
    2>&1 | tee "$artifact_root/01-contract-tests.log" | tee "$run_log"
fi
if ! grep -Eq 'test result: ok\. 1 passed' "$artifact_root/01-contract-tests.log"; then
  echo "focused contract filter did not execute exactly one passing test" >&2
  exit 1
fi

# The served dialect is not an operator choice. This companion renders its
# cells in exactly one RLM language, named once by `SERVED_DIALECT` in
# runbooks/restate-postgres-workers/src/bin/context_overflow_recovery.rs and
# reported on every checkpoint it emits. The row directory is named from that
# reported value, so the directory and the `dialect` field read from the same
# source and cannot disagree (FIG-3169). `LASH_RUNBOOK_DIALECT` selected
# nothing here -- it only renamed the directory - so refuse it outright rather
# than let a caller believe a row was served in the dialect they asked for.
if [ -n "${LASH_RUNBOOK_DIALECT:-}" ]; then
  echo "LASH_RUNBOOK_DIALECT is not a knob for this companion: it serves one dialect and reports it on every checkpoint. Unset it; the served dialect names the row directory." >&2
  exit 1
fi

staging="$artifact_root/context-overflow-recovery/.observed"
mkdir -p "$staging"
if [[ -f .kiln.bazelrc ]]; then
  "$repo/scripts/ci/with-service.sh" restate -- \
    kiln run //runbooks/restate-postgres-workers:lash-e2e-context-overflow-recovery__bin \
    2>&1 | tee "$staging/03-observed.jsonl" | tee -a "$run_log"
else
  "$repo/scripts/ci/with-service.sh" restate -- \
    cargo run --locked --quiet -p lash-restate-postgres-workers-e2e \
    --bin lash-e2e-context-overflow-recovery \
    2>&1 | tee "$staging/03-observed.jsonl" | tee -a "$run_log"
fi

dialect="$(python3 - "$staging/03-observed.jsonl" <<'DIALECT'
import json
import sys
from pathlib import Path

served = set()
for line in Path(sys.argv[1]).read_text(encoding="utf-8").splitlines():
    try:
        value = json.loads(line)
    except json.JSONDecodeError:
        continue
    if "checkpoint" in value and "dialect" in value:
        served.add(value["dialect"])

if len(served) != 1:
    raise SystemExit(
        f"checkpoints did not agree on one served dialect: {sorted(served)}"
    )
name = served.pop()
if not name or "/" in name or name.startswith("."):
    raise SystemExit(f"served dialect is not a usable row directory name: {name!r}")
print(name)
DIALECT
)"

row_dir="$artifact_root/context-overflow-recovery/$dialect"
mkdir -p "$row_dir"
mv "$staging/03-observed.jsonl" "$row_dir/03-observed.jsonl"
rmdir "$staging"
echo "context-overflow-recovery row: dialect=$dialect (read off the run, not chosen)" | tee -a "$run_log"

python3 - "$dialect" "$row_dir" <<'PY'
import json
import sys
from pathlib import Path

dialect = sys.argv[1]
row = Path(sys.argv[2])


def fail(message):
    raise SystemExit(f"context-overflow-recovery [{dialect}] gate failed: {message}")


def checkpoint(name):
    for line in (row / "03-observed.jsonl").read_text(encoding="utf-8").splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if value.get("checkpoint") == name:
            return value
    fail(f"missing checkpoint {name!r}")


observed = checkpoint("context_overflow_recovered")
classified = checkpoint("classified_overflow_recovered")
control = checkpoint("provider_error_control")

for value in (observed, classified, control):
    if value.get("dialect") != dialect:
        fail(f"checkpoint did not record served dialect {dialect!r}: {value}")

# The overflow really arrived mid-turn, carried by a tool result the prompt
# budget never saw. Both arms must be honestly mid-turn.
for name, value in (("injected", observed), ("classified", classified)):
    if value["oversized_tool_result_bytes"] < 256 * 1024:
        fail(f"[{name}] the tool result was not oversized: {value}")
    if value["provider_calls"] < 3:
        fail(f"[{name}] the fixture did not drive a second in-turn request: {value}")

# The outcome is its own thing, and is not a success -- on both arms. The
# classified arm is the one that matters for a real provider: there the reason
# is produced by `is_context_overflow_text`, not handed to lash by the fixture.
for name, value in (("injected", observed), ("classified", classified)):
    if value["overflow_stop"] != "context_overflow":
        fail(f"[{name}] the overflow turn did not stop as context_overflow: {value}")
    if value["overflow_is_context_overflow"] is not True:
        fail(f"[{name}] the public read side did not report the overflow: {value}")
    if value["overflow_is_success"] is not False:
        fail(f"[{name}] an overflow turn reported success: {value}")

# ... and it is distinguishable from an ordinary provider error.
if control["control_stop"] != "provider_error":
    fail(f"the control turn did not stop as provider_error: {control}")
if control["control_stop"] == observed["overflow_stop"]:
    fail(f"overflow and provider error collapsed into one outcome: {control}")

# The RLM sessions continue after the overflow. Their outcome gates stay
# independent of the standard protocol's plugin recovery.
for name, value in (("injected", observed), ("classified", classified)):
    if value["continued_is_success"] is not True:
        fail(f"[{name}] the session did not continue after the overflow: {value}")
    if value["continued_is_context_overflow"] is not False:
        fail(f"[{name}] the continued turn overflowed again: {value}")
    if value.get("continued_final_value") is None:
        fail(f"[{name}] the continued turn committed no finish payload: {value}")

# The two arms must agree: the path the reason took must not change the outcome.
if classified["overflow_stop"] != observed["overflow_stop"]:
    fail(f"the classifier arm produced a different stop: {classified}")

standard = checkpoint("standard_plugin_recovered")
if standard.get("protocol") != "standard":
    fail(f"the recovery arm did not use the standard protocol: {standard}")
if standard["oversized_tool_result_bytes"] < 256 * 1024 or standard["provider_calls"] < 4:
    fail(f"the standard arm did not overflow after a real tool result: {standard}")
if standard["overflow_stop"] != "context_overflow" or standard["overflow_is_context_overflow"] is not True:
    fail(f"the standard arm lost its overflow outcome: {standard}")
if standard["plugin_recovery_pending"] is not True:
    fail(f"the standard plugin did not persist the pending marker: {standard}")
if standard["plugin_recovery_completed"] is not True:
    fail(f"the standard plugin did not persist completion: {standard}")
if standard["plugin_recovery_summary_chars"] <= 0:
    fail(f"the standard plugin produced no recovery summary: {standard}")
if standard["recovery_frame_reason"] != "compaction" or standard["recovery_frame_moved"] is not True:
    fail(f"the standard session did not enter a new compaction frame: {standard}")
if standard["continued_is_success"] is not True or standard["continued_is_context_overflow"] is not False:
    fail(f"the standard session did not continue after recovery: {standard}")
if not standard.get("continued_assistant_message"):
    fail(f"the standard continuation has no assistant answer: {standard}")

standard_dir = row.parent / "standard"
standard_dir.mkdir(exist_ok=True)
(standard_dir / "03-observed.jsonl").write_text(json.dumps(standard) + "\n", encoding="utf-8")

print(
    f"context-overflow-recovery [{dialect}] gates: mid-turn overflow (injected + "
    "classified), own outcome, distinct from provider_error, session continued"
)
print("context-overflow-recovery [standard] gates: plugin recovery pending + completed, summary, compaction frame, session continued")
PY

echo "context-overflow-recovery e2e passed: rows=2 rlm_dialect=$dialect" | tee -a "$run_log"
