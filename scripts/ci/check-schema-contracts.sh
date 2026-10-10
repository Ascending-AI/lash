#!/usr/bin/env bash
# Portable CI jobs build generators once and use the same comparison code
# as Buck2. Functional E2E owns only the example contracts.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo"
context="${1:-untrusted}"
case "$context" in
  untrusted)
    [[ "${BUCK2_TRUSTED:-}" == false ]] || {
      echo 'check-schema-contracts: portable Cargo path is only for untrusted CI' >&2
      exit 2
    }
    ;;
  --functional-e2e)
    [[ "${GITHUB_ACTIONS:-}" == true && "${GITHUB_EVENT_NAME:-}" == workflow_dispatch ]] || {
      echo 'check-schema-contracts: functional E2E requires an explicit GitHub workflow dispatch' >&2
      exit 2
    }
    ;;
  *) echo 'usage: check-schema-contracts.sh [--functional-e2e]' >&2; exit 2 ;;
esac
if [[ -f env.sh ]]; then
  source ./env.sh
fi
build_dir="$(mktemp -d)"
trap 'rm -rf -- "$build_dir"' EXIT
# Cargo reports the actual executable rather than assuming a target directory
# or profile. Do not send compiler diagnostics into the generator's JSON.
# The same generators as the root BUCK's `host_schema_documents`.
generators=(
  "lash-internal-trace trace_schema_generator"
  "lash-internal-core-execution process_event_schema_generator"
  "lash-kernel-doc kernel_schema_generator"
  "lash-kernel-edit kernel_edit_schema_generator"
  "lash-kernel-state parked_schema_generator"
)
if [[ "$context" == untrusted ]]; then
  build_args=()
  for generator in "${generators[@]}"; do
    read -r package bin <<<"$generator"
    build_args+=(-p "$package" --bin "$bin")
  done
  cargo build --locked "${build_args[@]}" \
    --message-format=json-render-diagnostics > "$build_dir/host-build.json"
fi
executable() {
  python3 - "$1" "$2" <<'PY'
import json
import sys
for line in open(sys.argv[1], encoding="utf-8"):
    event = json.loads(line)
    if event.get("reason") == "compiler-artifact" and event.get("target", {}).get("name") == sys.argv[2] and event.get("executable"):
        print(event["executable"])
        break
else:
    raise SystemExit(f"Cargo did not report generator {sys.argv[2]}")
PY
}
if [[ "$context" == untrusted ]]; then
  check_args=()
  for generator in "${generators[@]}"; do
    read -r _ bin <<<"$generator"
    check_args+=(--generator "$(executable "$build_dir/host-build.json" "$bin")")
  done
  python3 scripts/generate-workflow-schemas.py --check "${check_args[@]}"
fi
