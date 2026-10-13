#!/usr/bin/env bash
set -euo pipefail

# Clippy size lints and named type-size assertions are the primary regression
# gate (FIG-595, after the FIG-594 structural fix); the product-stack probes
# are the defense-in-depth backstop for dynamic poll-stack behavior.
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$repo"
# A kiln fork carries its buck2 configuration in env.sh; a plain checkout has
# none and falls through to the hermetic driver below.
[[ -f env.sh ]] && source env.sh

# The stack-budget laws live in lash-sim's stack_policy target: each probe
# replays one fixed agent product contract on a thread of the product's
# 2 MiB stack budget, so no rlimit wrapper is needed. The former
# `cargo test -p lash-runtime stack_budget` selection was deleted when its
# laws moved to the durable substrate; docs/testing/substrate-port-ledger.toml
# records it covered by this target.
if command -v kiln >/dev/null 2>&1; then
  kiln test //crates/lash-sim:stack_policy__test
else
  scripts/hermetic-build.sh test //crates/lash-sim:stack_policy__test
fi

cargo build -p lash-perf --locked

# RLM turns run model code in the shipped worker helper, which the runtime
# finds beside the host binary or through LASH_VM_WORKER. Bazel's test_env
# names it for these packages; the cargo run names it the same way. The
# `testing` feature is part of the baked-in build identity, so the helper
# must carry it to match the feature set the test-side client sees.
cargo build -p lash-internal-vm-worker --bin lash-vm-worker --features testing,dialect-typescript,dialect-python --locked
target_dir="${CARGO_TARGET_DIR:-target}"
[[ "$target_dir" = /* ]] || target_dir="$PWD/$target_dir"
export LASH_VM_WORKER="$target_dir/debug/lash-vm-worker"

python3 scripts/profile_runtime_stack.py \
  --binary "$target_dir/debug/lash-perf" \
  --no-build \
  --enforce-budgets \
  --budget-only \
  --out .benchmarks/runtime-stack/ci-deep-turn-composition.json
