#!/usr/bin/env bash
# The local form of CI's `Test repository scripts` step. Its command list is
# embedded in .github/workflows/ci.yml as the GATES heredoc piped to
# run-gate-commands.sh, so this extracts that list instead of duplicating it:
# a gate added to the CI step joins the local run automatically.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

commands="$(awk '
  /run-gate-commands\.sh .*<<.GATES./ { capture = 1; next }
  capture && /^[[:space:]]*GATES[[:space:]]*$/ { capture = 0 }
  capture { sub(/^[[:space:]]+/, ""); print }
' "$repo/.github/workflows/ci.yml")"

if [[ -z "${commands//[[:space:]]/}" ]]; then
  printf 'repository-gates: no GATES heredoc found in .github/workflows/ci.yml\n' >&2
  exit 2
fi

# --jobs 4 mirrors the CI runner's four-core pool: a bare `nproc` fan-out on
# the shared dev box lets the 43 gates starve each other's spawned process
# trees, and time-sensitive self-tests (e.g. test_with_service.py's interrupt
# window) race their own deadlines instead of measuring the product.
printf '%s\n' "$commands" \
  | bash "$repo/scripts/ci/run-gate-commands.sh" --jobs 4 --serial '^bash '
