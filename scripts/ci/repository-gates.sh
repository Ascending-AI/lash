#!/usr/bin/env bash
# The local form of CI's `Test repository scripts` step. Its command list is
# embedded in .github/workflows/ci.yml as the GATES heredoc piped to
# run-gate-commands.sh, so this extracts that list instead of duplicating it:
# a gate added to the CI step joins the local run automatically.
#
# One gate is skipped locally unless `--all` is passed:
# `bash scripts/test-agent-workbench-dev-reset.sh` runs 170 s on the dev box
# (measured 2026-09-22 on a `just floor` whose Bazel leg had dropped to
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
  capture { sub(/^[[:space:]]+/, ""); print }
' "$repo/.github/workflows/ci.yml")"

if [[ -z "${commands//[[:space:]]/}" ]]; then
  printf 'repository-gates: no GATES heredoc found in .github/workflows/ci.yml\n' >&2
  exit 2
fi

skipped=""
if ((run_all == 0)); then
  skipped="$(printf '%s\n' "$commands" | grep -Fx -- "$local_skip" || true)"
  commands="$(printf '%s\n' "$commands" | grep -Fxv -- "$local_skip" || true)"
fi

# --jobs 4 mirrors the CI runner's four-core pool: a bare `nproc` fan-out on
# the shared dev box lets the 43 gates starve each other's spawned process
# trees, and time-sensitive self-tests (e.g. test_with_service.py's interrupt
# window) race their own deadlines instead of measuring the product.
status=0
printf '%s\n' "$commands" \
  | bash "$repo/scripts/ci/run-gate-commands.sh" --jobs 4 --serial '^bash ' \
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
