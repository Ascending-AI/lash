#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
. ./env.sh
evidence="${LASH_SCENARIO_EVIDENCE:-$PWD/target/perf-replay-scenarios/evidence}"
stores="${LASH_SCENARIO_STORES:-$PWD/target/perf-replay-scenarios/stores}"
mkdir -p "$evidence" "$PWD/target/perf-replay-scenarios/tmp"
export TMPDIR="$PWD/target/perf-replay-scenarios/tmp"
kiln build --config=optimized \
  //crates/lash-perf:lash-perf__bin //crates/lash-vm-worker:lash-vm-worker__bin \
  --materializations final --build-report "$evidence/build-report.json" \
  > "$evidence/build.log" 2>&1
LASH_VM_WORKER="$(python3 tools/buck2/outputs.py \
  --report "$evidence/build-report.json" \
  --label //crates/lash-vm-worker:lash-vm-worker__bin --single)"
export LASH_VM_WORKER
workloads=(crates/lash-perf/workloads/replay/scenarios/*.json)
(ulimit -v 8388608; /usr/bin/time -v -o "$evidence/peak-rss.txt" \
  kiln run --config=optimized //crates/lash-perf:lash-perf__bin -- \
  replay "${workloads[@]}" --store-dir "$stores" --out "$evidence/receipt.json") \
  > "$evidence/run.log" 2>&1
printf 'Replay receipt: %s\n' "$evidence/receipt.json"
