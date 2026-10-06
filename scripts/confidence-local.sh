#!/usr/bin/env bash
# One on-demand entry point for the local Confidence stages.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$repo"
case "${1:-}" in
  -h|--help|--list) exec python3 scripts/ci/confidence_local.py "$@" ;;
esac
if [ -z "${KILN_GATE_ID:-}" ]; then
  exec kiln gate lash "$(basename "$repo")" -- bash "$repo/scripts/confidence-local.sh" "$@"
fi
# Own the network once; consumers inherit the lock but each owns its services.
source "$repo/scripts/worktree-gate-env.sh"
lash_gate_acquire confidence-local
finish_confidence_local() {
  if [ "${LASH_GATE_ACQUIRED_HERE:-0}" = "1" ]; then
    lash_gate_cleanup
  fi
}
trap finish_confidence_local EXIT
python3 scripts/ci/confidence_local.py "$@"
