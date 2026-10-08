#!/usr/bin/env bash
set -euo pipefail

# Clippy size lints and named type-size assertions are the primary regression
# gate (FIG-595, after the FIG-594 structural fix); this 2 MiB run is the
# defense-in-depth backstop for dynamic poll-stack behavior.
stack_kb="${LASH_STACK_BUDGET_KB:-2048}"
rust_min_stack="${LASH_RUST_MIN_STACK_BUDGET:-2097152}"

# The runtime selection lives behind lash-runtime's `rlm` feature; without it
# `stack_budget` compiles no test and the run passes vacuously (FIG-4432).
cargo test -p lash-internal-lashlang --test stack_budget --locked --no-run
cargo test -p lash-runtime --features rlm stack_budget --locked --no-run
cargo build -p lash-perf --locked

# RLM turns run model code in the shipped worker helper, which the runtime
# finds beside the host binary or through LASH_VM_WORKER. Bazel's test_env
# names it for these packages; the cargo run names it the same way. The
# `testing` feature is part of the baked-in build identity, so the helper
# must carry it to match the feature set the test-side client sees.
cargo build -p lash-internal-vm-worker --bin lash-vm-worker --features testing --locked
target_dir="${CARGO_TARGET_DIR:-target}"
[[ "$target_dir" = /* ]] || target_dir="$PWD/$target_dir"
export LASH_VM_WORKER="$target_dir/debug/lash-vm-worker"

log_dir="$(mktemp -d "${TMPDIR:-/tmp}/lash-stack-budget.XXXXXX")"

# A filtered selection whose filter matches nothing exits 0 with
# "0 passed", so each run must show at least one passing test in its log.
run_stack_budget() {
  local name="$1"
  shift
  local log="$log_dir/$name.log"
  "$@" 2>&1 | tee "$log"
  if ! grep -Eq 'test result: ok\. [1-9][0-9]* passed' "$log"; then
    echo "stack budget selection executed zero tests: $*" >&2
    return 1
  fi
}

(
  ulimit -s "$stack_kb"
  export RUST_MIN_STACK="$rust_min_stack"
  run_stack_budget lashlang cargo test -p lash-internal-lashlang --test stack_budget --locked -- --nocapture --test-threads=1
  run_stack_budget runtime cargo test -p lash-runtime --features rlm stack_budget --locked -- --nocapture --test-threads=1
)

python3 scripts/profile_runtime_stack.py \
  --no-build \
  --enforce-budgets \
  --budget-only \
  --out .benchmarks/runtime-stack/ci-deep-turn-composition.json
