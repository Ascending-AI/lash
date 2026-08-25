#!/usr/bin/env bash
set -euo pipefail

# Clippy size lints and named type-size assertions are the primary regression
# gate (FIG-595, after the FIG-594 structural fix); this 2 MiB run is the
# defense-in-depth backstop for dynamic poll-stack behavior.
stack_kb="${LASH_STACK_BUDGET_KB:-2048}"
rust_min_stack="${LASH_RUST_MIN_STACK_BUDGET:-2097152}"

cargo test -p lash-internal-lashlang --test stack_budget --locked --no-run
cargo test -p lash-runtime stack_budget --locked --no-run
cargo test -p lash-internal-subagents --locked --no-run
cargo build -p lash-perf --locked

(
  ulimit -s "$stack_kb"
  export RUST_MIN_STACK="$rust_min_stack"
  cargo test -p lash-internal-lashlang --test stack_budget --locked -- --nocapture --test-threads=1
  cargo test -p lash-runtime stack_budget --locked -- --nocapture --test-threads=1
  cargo test -p lash-internal-subagents rlm_spawn_seed_is_visible_to_child_executor_and_prompt --locked -- --nocapture --test-threads=1
)

python3 scripts/profile_runtime_stack.py \
  --no-build \
  --enforce-budgets \
  --budget-only \
  --out .benchmarks/runtime-stack/ci-deep-turn-composition.json
