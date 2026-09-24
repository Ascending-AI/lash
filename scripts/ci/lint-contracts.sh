#!/usr/bin/env bash
# Keep portable Cargo contracts and frontend type checks in one bounded group.
# Trusted events prove the OFF and Restate release resolutions in the
# feature-lanes job and run the schema actions alongside Clippy.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo"
case "${BAZEL_TRUSTED:-}" in
  true|false) ;;
  *) echo 'lint-contracts: BAZEL_TRUSTED must be true or false' >&2; exit 2 ;;
esac
if [[ -f env.sh ]]; then
  source ./env.sh
fi
# These Cargo commands share a target directory. Keep them in one sequential
# leg while Node checks run beside them, and collect both failures.
cargo_contracts() {
  local status=0
  if [[ "$BAZEL_TRUSTED" == false ]]; then
    cargo check -p lash-runtime --lib --no-default-features --locked || status=$?
    cargo check -p lash-runtime --lib --no-default-features --features restate --locked || status=$?
    bash scripts/ci/check-schema-contracts.sh || status=$?
  fi
  return "$status"
}
export -f cargo_contracts
export BAZEL_TRUSTED
printf '%s\n' \
  'cargo_contracts' \
  'npm --prefix examples/workflow-graph-roundtrip/frontend run check:generated-types' \
  | bash scripts/ci/run-gate-commands.sh --jobs 2
