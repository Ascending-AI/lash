#!/usr/bin/env bash
# The full-profile Cargo-owned job has no pool; ordinary forks use Kiln.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
if [[ -f env.sh ]]; then
  source ./env.sh
fi
frontend=examples/workflow-graph-roundtrip/frontend
npm --prefix "$frontend" ci
if [[ "${GITHUB_ACTIONS:-}" == true ]]; then
  bash scripts/ci/check-schema-contracts.sh --functional-e2e
else
  python3 examples/workflow-graph-roundtrip/scripts/generate-contract-schema.py --check
fi
npm --prefix "$frontend" run check:generated-types
(
  cd "$frontend"
  npm exec -- vitest run
  npm exec -- vite build
)
cargo test -p workflow-graph-roundtrip --all-targets --locked
bash scripts/check-workflow-graph-model.sh
