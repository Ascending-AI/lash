#!/usr/bin/env bash
# Cargo's resolver-2 development graph is package-local. Buck's generated
# workspace feature union cannot prove that each package builds on its own.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo"
if [ -f env.sh ]; then
  . ./env.sh
fi
out="${LASH_CARGO_PARITY_OUT_DIR:-$repo/.kiln/cargo-parity}"
mkdir -p "$out"
# Keep Cargo's admission shim, but deliberately exercise Cargo, not its Buck
# routing. Offline uses the same locked sources already acquired by Kiln.
export KILN_CARGO_ROUTE=
args=(--all-targets --locked --offline --target-dir "$out/target")
if [ -n "${LASH_CI_FEATURES:-}" ]; then
  args+=(--features "$LASH_CI_FEATURES")
fi
started=$SECONDS
# --lib excludes development dependencies: a host must never need testing.
bash scripts/ci/facade-production.sh
cargo check --workspace "${args[@]}"
# The host endpoint witness is another independently reproduced omission.
cargo check --package integrator-contract "${args[@]}"
printf 'cargo parity passed: workspace and 1 isolated package in %ss\n' "$((SECONDS - started))"
