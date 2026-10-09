set positional-arguments

repo := justfile_directory()

default:
  @just --list

agent-workbench port='3030':
  ./scripts/agent-workbench-dev.sh up --port "{{port}}"

# Non-destructive: replaces only the workbench process and keeps any managed
# Postgres and the application data.
agent-workbench-restart port='3030':
  ./scripts/agent-workbench-dev.sh restart --port "{{port}}"

# Destructive: available only for a wholly launcher-owned disposable stack.
agent-workbench-reset port='3030':
  ./scripts/agent-workbench-dev.sh restart --reset-dev-state --port "{{port}}"

agent-workbench-status port='3030':
  ./scripts/agent-workbench-dev.sh status --port "{{port}}"

agent-workbench-logs port='3030':
  ./scripts/agent-workbench-dev.sh logs --port "{{port}}"

agent-workbench-logs-follow port='3030':
  ./scripts/agent-workbench-dev.sh logs --port "{{port}}" --follow

agent-workbench-down port='3030':
  ./scripts/agent-workbench-dev.sh down --port "{{port}}"

agent-workbench-foreground port='3030':
  ./scripts/agent-workbench-dev.sh foreground --port "{{port}}"

rlm-smoke-e2e:
  bash "{{repo}}/scripts/rlm-smoke-e2e.sh"

workflow-graph-roundtrip port='3031':
  #!/usr/bin/env bash
  set -euo pipefail
  target_dir="${WORKFLOW_GRAPH_TARGET_DIR:-/tmp/lash-workflow-graph-{{port}}}"
  npm --prefix "{{repo}}/examples/workflow-graph-roundtrip/frontend" ci
  npm --prefix "{{repo}}/examples/workflow-graph-roundtrip/frontend" run build
  # The host finds the VM worker beside its own executable.
  CARGO_TARGET_DIR="$target_dir" \
    cargo build -p lash-internal-vm-worker --bin lash-vm-worker --profile judged
  WORKFLOW_GRAPH_ADDR="127.0.0.1:{{port}}" CARGO_TARGET_DIR="$target_dir" \
    cargo run -p workflow-graph-roundtrip --bin workflow-graph-roundtrip --profile judged

workflow-graph-integration-verify:
  bash "{{repo}}/scripts/workflow-graph-integration-verify.sh"

# FIG-4042: token-free RLM warning and frame-switch companion for the manual
# workbench continue_as runbook. The provider responses are scripted in-process.
workbench-continue-as-budget-gate:
  #!/usr/bin/env bash
  set -euo pipefail
  kiln test //crates/lash-protocol-rlm:lash-protocol-rlm__unit_test --test_arg=budget_warning --test_output=errors
  kiln test //examples/agent-workbench:agent-workbench__unit_test --test_arg=continue_as_warning_override --test_output=errors
  kiln test //crates/lash-protocol-rlm:protocol_drivers__test --test_arg=scripted_context_budget_warning_reaches_model_and_continue_as_carries_only_seed --test_output=errors

# The send-to-completion latency gate (FIG-3843): `send()` → `outcome()`
# measured end to end, the same-process fast fixture gated at overhead p50 < 50 ms / p99 < 250 ms over 10,000
# samples, every other case (stream, tool, failure, busy, controlled
# real-provider, cross-worker, poll, grace) measured and reported. The report
# and the raw sample ledger land under the artifact directory. The harness
# builds in the optimized Kiln configuration — the same build the release's perf guard
# measures — so the budget binds optimized code, not a debug binary.
#
# Arguments are forwarded to `lash-perf latency`, so one case can be run
# alone: `just latency-gate --cases fast --fast-samples 1050 --lanes 1`.
#
# `scripts/latency_load_record.py` samples the host's load averages and CPU
# pressure through the run into `latency-load.json` and marks the run
# unqualified when the 1-minute load reaches the core count during the fast
# case (FIG-3843's quiet-host rule). `scripts/latency_slope.py` then reads
# the ledger's accept-to-admission slope and ordinal-bin medians.
latency-gate *args:
  #!/usr/bin/env bash
  set -euo pipefail
  if [ -n "${KILN_GATE_ID:-}" ] && [ -z "${LASH_GATE_SLOT_OVERRIDE:-}" ]; then
    gate_sum="$(printf '%s' "$KILN_GATE_ID" | cksum)"
    export LASH_GATE_SLOT_OVERRIDE="$(( ${gate_sum%% *} % 90 ))"
  fi
  source "{{repo}}/scripts/worktree-gate-env.sh"
  lash_gate_acquire_locks latency-gate

  artifacts="${LASH_LATENCY_ARTIFACT_DIR:-target/functional-e2e-artifacts/latency-gate}"
  case "$artifacts" in
    /*) ;;
    *) artifacts="{{repo}}/$artifacts" ;;
  esac
  mkdir -p "$artifacts"
  log="$artifacts/latency-gate.log"

  kiln build --config=optimized //crates/lash-perf:lash-perf__bin \
    --materializations final --build-report "$artifacts/build-report.json"
  binary="$(python3 "{{repo}}/tools/buck2/outputs.py" \
    --report "$artifacts/build-report.json" --label //crates/lash-perf:lash-perf__bin --single)"

  set +e
  timeout --kill-after=30 5400 \
    python3 "{{repo}}/scripts/latency_load_record.py" \
      --out "$artifacts/latency-load.json" \
      -- "$binary" latency \
        --out "$artifacts/latency-report.json" \
        --samples-out "$artifacts/latency-samples.json" \
        --store-dir "$artifacts/stores" {{args}} 2>&1 | tee "$log"
  status="${PIPESTATUS[0]}"
  set -e

  grep -E '^latency (gate|load record): ' "$log" || true
  if [ -s "$artifacts/latency-samples.json" ]; then
    python3 "{{repo}}/scripts/latency_slope.py" "$artifacts/latency-samples.json" \
      | tee "$artifacts/latency-slope.txt" || true
  fi
  if [ "$status" -ne 0 ]; then
    echo "latency gate failed (exit $status); log: $log" >&2
    exit "$status"
  fi

# Opt-in full local diagnostic for unusual risk, release work, or an explicit
# request. It is not a routine push or merge prerequisite; focused local checks
# plus CI's aggregate conclusion are the default proof path.
push-gate:
  bash "{{repo}}/scripts/push-gate.sh"

# Local Confidence stage matrix, one shared build, and its strict conclusion.
confidence-local *args:
  bash "{{repo}}/scripts/confidence-local.sh" "$@"

# Optional broader iteration run: the whole workspace suite except four tests
# that between them account for most of its wall clock. It can supplement a
# focused regression when wider feedback is useful, but it is not a routine
# review or stacking prerequisite. State its exact scope; CI supplies the broad
# merge proof.
#
# The list is those by measurement, not a category sweep — the cheap tests
# beside them keep running. Each exclusion, and why deferring it during
# iteration is safe:
#   lash-sim `generated_sim_profile_writes_trace_replay_and_provider_artifacts`
#     (213s), `minimizer_writes_replayable_regression_package` (112s) and
#     `replay_failure_publishes_no_minimized_package_artifacts` (109s) —
#     counterexample minimization and the generated simulation harness,
#     replayed across the committed fixture corpora. These three are the
#     critical path of the full run; the other minimizer tests are cheap and
#     stay in.
#   lash-runtime `ui` binary — the trybuild compile-fail gates on the public
#     API surface. Only 16s once trybuild's nested target directory is warm,
#     but each case is a nested `cargo build`, so on a cold cache the pair
#     costs minutes. The cost is compilation rather than product logic, and
#     only an API-surface change can move the result. `just seal` is the way
#     back in: run it whenever a diff moves the public facade.
# Drop the leading `not` from the expression to run only the excluded set.
battery-fast:
  cargo nextest run --workspace --locked -E 'not ((package(lash-runtime) & binary(ui)) + (package(lash-sim) & test(/^(minimize::tests::minimizer_writes_replayable_regression_package|minimize::tests::replay_failure_publishes_no_minimized_package_artifacts|runner::tests::generated_sim_profile_writes_trace_replay_and_provider_artifacts)$/)))'

# The API-surface seal, exactly as CI's `seal` lane runs it. `battery-fast`
# excludes the `ui` binary for wall clock, and nothing else in the local
# batteries compiles the facade the way a dependent crate sees it, so a diff
# that moves or re-homes a `pub use` is green locally and red on CI until this
# runs. Run it whenever a diff touches the public facade.
seal:
  cargo test --workspace --locked --test ui

# An opt-in broad tooling checkpoint over the Kiln fork: the dev and feature-lane test and
# clippy partitions on the shared pool plus the quick script gates. Frontend
# dependencies are installed first so npm does not replace node_modules while
# Buck2 scans the example package; the remaining gates run concurrently and
# are reported as one table. The repository-gates leg skips
# `scripts/test-agent-workbench-dev-reset.sh` locally (170 s, it alone
# bounded the floor) and says so in its table row; CI's `Test repository
# scripts` job still runs it, and `scripts/ci/repository-gates.sh --all`
# restores it here. Run it on a COMMITTED head so the gates judge the tree
# that will land.
floor:
  #!/usr/bin/env bash
  set -euo pipefail
  cd "{{repo}}"
  npm --prefix examples/workflow-graph-roundtrip/frontend ci
  printf '%s\n' \
    'kiln test //:dev_tests //:feature_lane_tests' \
    'kiln clippy //:workspace_clippy' \
    'kiln build //:schema_checks' \
    'kiln fmt -- --check' \
    'git diff --check' \
    'scripts/ci/repository-gates.sh' \
    'npm --prefix examples/workflow-graph-roundtrip/frontend run check:generated-types' \
    'python3 scripts/check_format_registry.py' \
    'python3 scripts/check_writer_stamps.py' \
    | scripts/gate-table.sh

# The store-schema gates only: the durable format registry and the
# lash-core-store unit target that holds the runtime-error classification
# exhaustiveness test.
schema-check:
  #!/usr/bin/env bash
  set -euo pipefail
  cd "{{repo}}"
  printf '%s\n' \
    'python3 scripts/check_format_registry.py' \
    'python3 scripts/check_writer_stamps.py' \
    'kiln test //crates/lash-core-store:lash-core-store__unit_test' \
    | scripts/gate-table.sh

# Opt-in durable-store and session-graph property soak. PostgreSQL executes
# when its standard LASH_POSTGRES_DATABASE_URL configuration is present.
store-contract-soak cases='256':
  #!/usr/bin/env bash
  set -euo pipefail
  service=()
  if [[ -n "${LASH_POSTGRES_DATABASE_URL:-}" ]]; then
    service=(--local-test-execution --no-test-cache --test_env=LASH_POSTGRES_DATABASE_URL)
  fi
  run_property_soak() {
    local setting="$1" label="$2" selector="$3" seed_setting="$4"
    local replay=()
    if [[ -n "${!seed_setting:-}" ]]; then
      replay=("--test_env=${seed_setting}")
    fi
    kiln test --test_timeout=1200 --test_output=all \
      "--test_env=${setting}={{cases}}" "--test_arg=${selector}" \
      --test_arg=--nocapture "${replay[@]}" "${service[@]}" "$label"
  }
  run_property_soak LASH_STORE_CONTRACT_PROPTEST_CASES //crates/lash-sqlite-store:conformance_memory__test store_contract_state_machine LASH_STORE_CONTRACT_PROPTEST_SEED
  run_property_soak LASH_STORE_CONTRACT_PROPTEST_CASES //crates/lash-sqlite-store:conformance__test store_contract_state_machine LASH_STORE_CONTRACT_PROPTEST_SEED
  run_property_soak LASH_STORE_CONTRACT_PROPTEST_CASES //crates/lash-postgres-store:conformance__test store_contract_state_machine LASH_STORE_CONTRACT_PROPTEST_SEED
  run_property_soak LASH_SESSION_GRAPH_PROPTEST_CASES //crates/lash-sqlite-store:conformance_memory__test session_graph_state_machine LASH_SESSION_GRAPH_PROPTEST_SEED
  run_property_soak LASH_SESSION_GRAPH_PROPTEST_CASES //crates/lash-sqlite-store:conformance__test session_graph_state_machine LASH_SESSION_GRAPH_PROPTEST_SEED
  run_property_soak LASH_SESSION_GRAPH_PROPTEST_CASES //crates/lash-postgres-store:conformance__test session_graph_state_machine LASH_SESSION_GRAPH_PROPTEST_SEED

# Opt-in raw durable-state soak over SQLite and PostgreSQL. Requires the
# standard Postgres configuration and logs the operation kinds omitted by each
# bounded seed.
cross-backend-store-soak cases='64' seed='852':
  #!/usr/bin/env bash
  set -euo pipefail
  : "${LASH_POSTGRES_DATABASE_URL:?cross-backend-store-soak requires an explicit LASH_POSTGRES_DATABASE_URL}"
  kiln test --local-test-execution --no-test-cache \
    --test_timeout=1200 --test_output=all \
    --test_env=LASH_POSTGRES_DATABASE_URL \
    '--test_env=LASH_CROSS_BACKEND_CASES={{cases}}' \
    '--test_env=LASH_CROSS_BACKEND_SEED={{seed}}' \
    --test_arg=generated_cross_backend_surface_differential_agrees \
    --test_arg=--nocapture --test_arg=--include-ignored \
    //crates/lash-sim:cross_backend_store_differential__test

# The packaging proof the release runs before it publishes anything: package
# every publishable crate in one cargo invocation (so workspace siblings resolve
# against the crates just packaged, not against crates.io) and report the sha256
# of each .crate. `--no-verify` skips the per-crate verify builds.
package-workspace *args:
  python3 "{{repo}}/scripts/package_workspace.py" {{args}}

# Deterministic DOM/API/SQL transcript acceptance (Surfaces A-E).
workbench-transcript-projection-e2e:
  uv run --script "{{repo}}/scripts/workbench-transcript-projection-e2e.py"
