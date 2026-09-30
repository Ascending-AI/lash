#!/usr/bin/env bash
set -euo pipefail
export LASH_BUILD_WORKING_DIRECTORY="$PWD"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
if [[ -f "$repo/env.sh" ]]; then
  # shellcheck source=/dev/null
  source "$repo/env.sh"
fi
cd "$repo"
mode=()
if [[ "${1:-}" == --local || "${1:-}" == --shared ]]; then
  mode=("$1")
  shift
fi
# Formatting and the separately pinned Miri interpreter retain named recipes.
case "${1:-}" in
  fmt) shift; exec cargo fmt --all "$@" ;;
  miri) shift; exec bash "$repo/scripts/append-vec-miri.sh" "$@" ;;
esac
exec python3 "$repo/tools/buck2/driver.py" "${mode[@]}" "$@"
