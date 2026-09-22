#!/usr/bin/env bash
set -euo pipefail

# Deterministic companion for runbooks/context-overflow-recovery (FIG-1272).
#
# No container, no token, no network: a SQLite scratch store and a scripted
# provider. One TypeScript row, as runbooks/RULES.md requires since FIG-3023,
# with its own artifact directory and its own fresh data directory (the harness
# makes one per run).

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
cargo test --locked -p lash-internal-sansio \
  context_overflow_response_stops_as_its_own_outcome \
  2>&1 | tee "$artifact_root/01-contract-tests.log" | tee "$run_log"
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
cargo run --locked --quiet -p lash-restate-postgres-workers-e2e \
  --bin lash-e2e-context-overflow-recovery \
  2>&1 | tee "$staging/03-observed.jsonl" | tee -a "$run_log"

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

# Plugin-owned recovery (FIG-2950): the registered standard-compaction policy
# derives the pending trigger from the durable marker, spends one bounded
# out-of-band summarizer, and appends its summary plus a terminal record. The
# host only observes the durable state and the same session continues.
for name, value in (("injected", observed), ("classified", classified)):
    if value["plugin_recovery_pending"] is not True:
        fail(f"[{name}] the plugin never recorded the overflow trigger: {value}")
    if value["plugin_recovery_completed"] is not True:
        fail(f"[{name}] plugin-owned recovery did not complete: {value}")
    if value["plugin_recovery_summary_chars"] <= 0:
        fail(f"[{name}] plugin recovery produced no summary: {value}")
    # FIG-3107: the recovery is a plugin-visible durable agent-frame switch.
    # The recovery frame exists (an ordinary compaction frame) and the
    # continued session is resident in it after recovery.
    if value.get("recovery_frame_reason") != "compaction":
        fail(f"[{name}] the recovery frame is not a compaction frame: {value}")
    if value.get("recovery_frame_moved") is not True:
        fail(f"[{name}] the session is not resident in the recovery frame: {value}")
    if value["continued_is_success"] is not True:
        fail(f"[{name}] the session did not continue after recovery: {value}")
    if value["continued_is_context_overflow"] is not False:
        fail(f"[{name}] the continued turn overflowed again: {value}")
    if value.get("continued_final_value") is None:
        fail(f"[{name}] the continued turn committed no finish payload: {value}")

# The two arms must agree: the path the reason took must not change the outcome.
if classified["overflow_stop"] != observed["overflow_stop"]:
    fail(f"the classifier arm produced a different stop: {classified}")

print(
    f"context-overflow-recovery [{dialect}] gates: mid-turn overflow (injected + "
    "classified), own outcome, distinct from provider_error, plugin-owned recovery, "
    "session continued"
)
PY

echo "context-overflow-recovery e2e passed: rows=1 dialect=$dialect" | tee -a "$run_log"
