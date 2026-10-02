#!/usr/bin/env bash
# The local form of CI's `Test repository scripts` step. Discover self-tests
# with the same helper as CI and extract the non-test checks from its GATES
# heredoc, so new self-tests and checks join both entrypoints automatically.
#
# One gate is skipped locally unless `--all` is passed:
# `bash scripts/test-agent-workbench-dev-reset.sh` runs 170 s on the dev box
# (measured 2026-09-22 on a `just floor` whose build leg had dropped to
# 90–105 s), so it alone set the floor's wall clock. It is named in the table
# as skipped; CI's `Test repository scripts` job still runs the full list.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

local_skip='bash scripts/test-agent-workbench-dev-reset.sh'
run_all=0
if [[ "${1:-}" == "--all" ]]; then
  run_all=1
  shift
fi
if (($#)); then
  printf 'usage: scripts/ci/repository-gates.sh [--all]\n' >&2
  exit 2
fi

commands="$(awk '
  /run-gate-commands\.sh .*<<.GATES./ { capture = 1; next }
  capture && /^[[:space:]]*GATES[[:space:]]*$/ { capture = 0 }
  capture {
    sub(/^[[:space:]]+/, "")
    if ($0 !~ /^#/ && $0 !~ /^[[:space:]]*$/) print
  }
' "$repo/.github/workflows/ci.yml")"

if [[ -z "${commands//[[:space:]]/}" ]]; then
  printf 'repository-gates: no GATES heredoc found in .github/workflows/ci.yml\n' >&2
  exit 2
fi

discovered="$(python3 "$repo/scripts/ci/repository_gate_commands.py")"
commands="$(printf '%s\n' "$commands" "$discovered" | awk '!seen[$0]++')"

skipped=""
if ((run_all == 0)); then
  skipped="$(printf '%s\n' "$commands" | grep -Fx -- "$local_skip" || true)"
  commands="$(printf '%s\n' "$commands" | grep -Fxv -- "$local_skip" || true)"
fi

# Keep process-heavy self-tests within a four-command budget on the shared
# host. Launcher tests isolate their own global state and can run together.
status=0
printf '%s\n' "$commands" \
  | bash "$repo/scripts/ci/run-gate-commands.sh" --jobs 4 \
  || status=$?

# The last line is what scripts/gate-table.sh shows as this leg's evidence, so
# the skip is stated there, beside the count, rather than only in a log.
if [[ -n "$skipped" ]]; then
  count="$(printf '%s\n' "$commands" | grep -c .)"
  if ((status == 0)); then
    printf '%s gate commands passed; skipped locally (CI still runs it): %s\n' \
      "$count" "$skipped"
  else
    printf 'skipped locally (CI still runs it): %s\n' "$skipped"
  fi
fi
exit "$status"
