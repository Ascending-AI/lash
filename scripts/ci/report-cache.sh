#!/usr/bin/env bash
set -euo pipefail

# Setup failures remain failed; do not obscure them with a second missing-tool
# error from this always-run diagnostic step.
if ! command -v sccache >/dev/null 2>&1; then
  echo "Compilation cache was not initialized; see the failed setup step."
  exit 0
fi
sccache --show-stats
# Local-backend errors retain their filesystem cause, rather than being
# reduced to a write-error count. No remote credential-bearing requests exist.
if [[ -s "${SCCACHE_ERROR_LOG:-}" ]]; then
  echo "Local compilation cache diagnostics (last 40 lines):"
  tail -n 40 "$SCCACHE_ERROR_LOG"
fi
echo "Cache filesystem capacity:"
df -h "${RUNNER_TEMP}"
