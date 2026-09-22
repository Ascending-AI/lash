#!/usr/bin/env bash
# Portable CI jobs build generators once and use the same comparison code
# as Bazel. Functional E2E owns only the example contracts.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo"
context="${1:-untrusted}"
case "$context" in
  untrusted)
    [[ "${BAZEL_TRUSTED:-}" == false ]] || {
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
if [[ "$context" == untrusted ]]; then
  cargo build --locked -p lash-internal-lashlang --bin workflow_schema_generator \
    --message-format=json-render-diagnostics > "$build_dir/host-build.json"
fi
cargo build --locked -p workflow-graph-roundtrip --bin workflow_contract_schema \
  --message-format=json-render-diagnostics > "$build_dir/example-build.json"
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
  python3 scripts/generate-workflow-schemas.py --check \
    --generator "$(executable "$build_dir/host-build.json" workflow_schema_generator)"
fi
python3 examples/workflow-graph-roundtrip/scripts/generate-contract-schema.py --check \
  --generator "$(executable "$build_dir/example-build.json" workflow_contract_schema)"
