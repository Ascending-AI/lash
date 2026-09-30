#!/usr/bin/env bash
# Current turn-path and native Restate witnesses over SQLite memory/file and
# PostgreSQL. Invoke through `kiln gate`; every run must execute one exact law.
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
cd "$repo"

if [[ "${1:-}" == --run ]]; then
  binary=$2
  repetitions=$3
  logs=$4
  scope=$5
  mkdir -p "$logs"
  export MCP_CATALOG_ENDPOINTS_FILE="$logs/native-endpoints"
  : > "$MCP_CATALOG_ENDPOINTS_FILE"
  tests=(
    mcp_catalog::catalog_storm_sqlite_memory_turn_witness
    mcp_catalog::catalog_storm_sqlite_file_turn_witness
    mcp_catalog::catalog_storm_postgres_turn_witness
    mcp_catalog::catalog_storm_native_sqlite_memory_turn_witness
    mcp_catalog::catalog_storm_native_sqlite_file_turn_witness
    mcp_catalog::catalog_storm_native_postgres_turn_witness
  )
  if [[ "$scope" == native ]]; then
    tests=("${tests[@]:3}")
  fi
  for test in "${tests[@]}"; do
    for ((run=1; run<=repetitions; run++)); do
      log="$logs/${test##*::}-$run.log"
      if ! "$binary" --exact "$test" --include-ignored --test-threads=1 --show-output >"$log" 2>&1; then
        cat "$log"
        exit 1
      fi
      rg -q -F "test $test ... ok" "$log"
      rg -q -F 'test result: ok. 1 passed; 0 failed; 0 ignored;' "$log"
      printf '%s %s/%s passed\n' "$test" "$run" "$repetitions"
      rg -F 'host load:' "$log"
    done
  done
  exit 0
fi

repetitions=${1:-1}
scope=${2:-all}
[[ "$repetitions" =~ ^[1-9][0-9]*$ ]] || { echo 'usage: scripts/test-mcp-catalog.sh [repetitions]' >&2; exit 2; }
[[ "$scope" == all || "$scope" == native ]] || { echo 'scope must be all or native' >&2; exit 2; }
label=//crates/lash:integration__test__fv_d1bdef69
report=$(mktemp)
trap 'rm -f "$report"' EXIT
kiln build "$label" --materializations final --build-report "$report"
binary=$(python3 tools/buck2/outputs.py --report "$report" --label "$label" --single)
logs="$repo/target/mcp-catalog-witnesses"
bash scripts/ci/with-service.sh pg16 -- \
  bash scripts/ci/with-service.sh restate -- \
  bash scripts/test-mcp-catalog.sh --run "$binary" "$repetitions" "$logs" "$scope"
