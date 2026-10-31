#!/usr/bin/env bash
set -euo pipefail
mode="$1"
shift
repo="$(cd "$(dirname "$0")/.." && pwd)"
. "$repo/env.sh"
artifacts="${LASH_PERF_GATE_ARTIFACT_DIR:-$repo/target/perf-gates/$mode-$(date +%s)-$$}"
mkdir -p "$artifacts"
kiln build --config=optimized //crates/lash-perf:lash-perf__bin //crates/lash-vm-worker:lash-vm-worker__bin \
  --materializations final --build-report "$artifacts/build.json"
binary="$(python3 "$repo/tools/buck2/outputs.py" --report "$artifacts/build.json" --label //crates/lash-perf:lash-perf__bin --single)"
worker="$(python3 "$repo/tools/buck2/outputs.py" --report "$artifacts/build.json" --label //crates/lash-vm-worker:lash-vm-worker__bin --single)"
if [ "$mode" = history ] && [ -n "${LASH_PERF_INSTRUCTION_SAMPLE:-}" ]; then
  set -- "$@" --sample "$LASH_PERF_INSTRUCTION_SAMPLE"
fi
(ulimit -v 8388608; python3 "$repo/scripts/perf-gates.py" "$mode" --binary "$binary" --worker "$worker" --out "$artifacts" "$@")
