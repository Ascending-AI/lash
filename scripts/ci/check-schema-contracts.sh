#!/usr/bin/env bash
# Untrusted CI has no pool credentials. Build each portable generator once,
# then run the same comparison code the Bazel actions use.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo"
[[ "${BAZEL_TRUSTED:-}" == false ]] || {
  echo 'check-schema-contracts: portable Cargo path is only for untrusted CI' >&2
  exit 2
}
if [[ -f env.sh ]]; then
  source ./env.sh
fi
build_dir="$(mktemp -d)"
trap 'rm -rf -- "$build_dir"' EXIT
# Cargo reports the actual executable rather than assuming a target directory
# or profile. Do not send compiler diagnostics into the generator's JSON.
cargo build --locked -p lash-internal-lashlang --bin workflow_schema_generator \
  --message-format=json-render-diagnostics > "$build_dir/host-build.json"
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
python3 scripts/generate-workflow-schemas.py --check \
  --generator "$(executable "$build_dir/host-build.json" workflow_schema_generator)"
python3 examples/workflow-graph-roundtrip/scripts/generate-contract-schema.py --check \
  --generator "$(executable "$build_dir/example-build.json" workflow_contract_schema)"
