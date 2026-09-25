set positional-arguments

repo := justfile_directory()

default:
  @just --list

agent-workbench port='3030':
  ./scripts/agent-workbench-dev.sh up --port "{{port}}"

agent-workbench-up port='3030':
  ./scripts/agent-workbench-dev.sh up --port "{{port}}"

# Non-destructive: replaces only the workbench process and keeps the Restate
# engine and its journals, any managed Postgres, the registered deployment, and
# the application data. Export the same RESTATE_AUTHORITY_ID as the running
# stack.
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

toolbench model='z-ai/glm-5.3-flash' *args:
  kiln run //examples/toolbench:toolbench -- --model "{{model}}" {{args}}

rlm-smoke-e2e:
  bash "{{repo}}/scripts/rlm-smoke-e2e.sh"

example-core-shutdown-e2e:
  bash "{{repo}}/scripts/example-core-shutdown-e2e.sh"

# The slack-clone example is three processes: the platform on `port`, the bot on
# `port + 1`, and the runtime-attachable HTTP MCP server on `port + 2`. `up`
# starts them and waits for the bot to register for events.
slack-clone port='3040':
  ./scripts/slack-clone-dev.sh up --port "{{port}}"

slack-clone-up port='3040':
  ./scripts/slack-clone-dev.sh up --port "{{port}}"

slack-clone-restart port='3040':
  ./scripts/slack-clone-dev.sh restart --port "{{port}}"

slack-clone-status port='3040':
  ./scripts/slack-clone-dev.sh status --port "{{port}}"

slack-clone-logs port='3040':
  ./scripts/slack-clone-dev.sh logs --port "{{port}}"

slack-clone-logs-follow port='3040':
  ./scripts/slack-clone-dev.sh logs --port "{{port}}" --follow

slack-clone-down port='3040':
  ./scripts/slack-clone-dev.sh down --port "{{port}}"

slack-clone-platform-foreground port='3040':
  ./scripts/slack-clone-dev.sh platform-foreground --port "{{port}}"

slack-clone-full-host-e2e:
  bash "{{repo}}/scripts/slack-clone-full-host-e2e.sh"

slack-clone-live-model-e2e *args:
  bash "{{repo}}/scripts/slack-clone-live-model-e2e.sh" {{args}}

workflow-graph-roundtrip port='3031':
  #!/usr/bin/env bash
  set -euo pipefail
  target_dir="${WORKFLOW_GRAPH_TARGET_DIR:-/tmp/lash-workflow-graph-{{port}}}"
  npm --prefix "{{repo}}/examples/workflow-graph-roundtrip/frontend" ci
  npm --prefix "{{repo}}/examples/workflow-graph-roundtrip/frontend" run build
  WORKFLOW_GRAPH_ADDR="127.0.0.1:{{port}}" CARGO_TARGET_DIR="$target_dir" \
    cargo run -p workflow-graph-roundtrip --profile judged

workflow-graph-integration-verify:
  bash "{{repo}}/scripts/workflow-graph-integration-verify.sh"

# Generate the checked-in host contract schemas.
workflow-schema-generate:
  python3 scripts/generate-workflow-schemas.py

# Fail when checked-in host contract schemas differ from Rust types.
workflow-schema-check:
  python3 scripts/generate-workflow-schemas.py --check

agent-service-restate-e2e:
  #!/usr/bin/env bash
  set -euo pipefail
  source "{{repo}}/scripts/worktree-gate-env.sh"
  lash_gate_acquire agent-service-restate-e2e
  image="${AGENT_SERVICE_RESTATE_IMAGE:-restatedev/restate:1.7.12@sha256:bb9c93ab92bb401548841b35dba0e7236a3b108bc1d7d4c06a8f3ece46b80d4b}"
  container="${AGENT_SERVICE_RESTATE_CONTAINER:-lash-agent-service-restate-${LASH_GATE_WORKTREE_SLUG}}"
  admin_port="${RESTATE_ADMIN_PORT:-$((LASH_E2E_PORT_BASE + 20))}"
  ingress_port="${RESTATE_INGRESS_PORT:-$((LASH_E2E_PORT_BASE + 21))}"
  node_port="${RESTATE_NODE_PORT:-$((LASH_E2E_PORT_BASE + 22))}"
  endpoint_bind="${AGENT_SERVICE_E2E_ENDPOINT_BIND:-127.0.0.1:$((LASH_E2E_PORT_BASE + 23))}"
  endpoint_url="${AGENT_SERVICE_E2E_ENDPOINT_URL:-http://127.0.0.1:$((LASH_E2E_PORT_BASE + 23))}"
  admin_url="${RESTATE_ADMIN_URL:-http://127.0.0.1:$admin_port}"
  ingress_url="${RESTATE_INGRESS_URL:-http://127.0.0.1:$ingress_port}"
  run_token="$(date +%s)-$$"

  cleanup() {
    docker rm -f "$container" >/dev/null 2>&1 || true
    lash_gate_cleanup
  }
  trap cleanup EXIT

  bash "{{repo}}/scripts/docker-pull-with-retry.sh" "$image"

  docker run -d --name "$container" --label "$LASH_GATE_LABEL" --network host \
    -e RESTATE_ADMIN__BIND_PORT="$admin_port" \
    -e RESTATE_INGRESS__BIND_PORT="$ingress_port" \
    -e RESTATE_BIND_PORT="$node_port" \
    "$image" >/dev/null

  deadline=$((SECONDS + 60))
  until (echo >"/dev/tcp/127.0.0.1/$admin_port") >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
      docker logs "$container" >&2 || true
      echo "Restate admin port $admin_port did not become ready" >&2
      exit 1
    fi
    sleep 1
  done
  until (echo >"/dev/tcp/127.0.0.1/$ingress_port") >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
      docker logs "$container" >&2 || true
      echo "Restate ingress port $ingress_port did not become ready" >&2
      exit 1
    fi
    sleep 1
  done

  # The Restate durability path refuses to run without a trust-domain id, and
  # the effect-group workflow reads it per request: without it the handler
  # answers 500 `RESTATE_AUTHORITY_ID is required`. The id is fresh per run
  # because the Restate container above is created and removed per run, so
  # there is no durable state from a previous run for a stable id to keep
  # continuity with; it stays one value for the whole run, which is what the
  # host and the durable controller have to agree on.
  RESTATE_INGRESS_URL="$ingress_url" \
  RESTATE_ADMIN_URL="$admin_url" \
  RESTATE_AUTHORITY_ID="${RESTATE_AUTHORITY_ID:-agent-service-e2e:${LASH_GATE_WORKTREE_SLUG}:${run_token}}" \
  AGENT_SERVICE_E2E_ENDPOINT_BIND="$endpoint_bind" \
  AGENT_SERVICE_E2E_ENDPOINT_URL="$endpoint_url" \
  cargo test -p agent-service --features restate \
    live_restate_ingress_runs_agent_turn_and_process_workflow_end_to_end -- --ignored --nocapture

agent-workbench-restate-e2e:
  bash "{{repo}}/scripts/agent-workbench-restate-e2e.sh"

# The regression gate for the Restate effect-group choreography. Its suites are
# `#[ignore]`d because they need a Restate server, so this recipe is the only
# thing that runs them: `scripts/ci/restate_suite.py` builds the test binary on
# the shared pool, runs every ignored law of the suite (it asks libtest for
# `--ignored` tests only, which `scripts/check_service_gate_pinning.py` pins)
# beside pinned `restate-server`s, one law per process, and then runs the same
# laws again with every await suspended and replayed (the replay leg). The
# suite's filters, redelivery laws and replay divergences are registered in
# `scripts/restate-suites.toml`.
effect-group-conformance-e2e:
  #!/usr/bin/env bash
  set -euo pipefail
  source "{{repo}}/scripts/worktree-gate-env.sh"
  lash_gate_acquire_locks effect-group-conformance-e2e

  # The ignored catalogue invocations are deferred laws (FIG-3472): they emit
  # execution receipts like every other suite, and the census below fails the
  # recipe when one left none. The test binaries run with the crate dir as
  # cwd, so a relative artifact dir (which is what CI exports) is anchored at
  # the repo root.
  receipts_dir="${LASH_EFFECT_GROUP_ARTIFACT_DIR:-target/functional-e2e-artifacts/effect-group-conformance}"
  case "$receipts_dir" in
    /*) ;;
    *) receipts_dir="{{repo}}/$receipts_dir" ;;
  esac
  mkdir -p "$receipts_dir"
  export LASH_LAW_RECEIPTS="$receipts_dir/law-receipts.txt"
  rm -f "$LASH_LAW_RECEIPTS"

  python3 "{{repo}}/scripts/ci/restate_suite.py" suite effect-group --leg live \
    --artifacts "$receipts_dir"

  python3 "{{repo}}/scripts/check_law_execution_receipts.py" \
    --deferred effect-group-conformance-e2e \
    --receipts "$LASH_LAW_RECEIPTS"

  # The executed-law census for the run: one `law<TAB>label` line per law the
  # generated tests actually reached the end of.
  echo "law execution receipts:"
  sort "$LASH_LAW_RECEIPTS"

  # The replay leg holds back its registered divergences, so its receipts are
  # a separate file the census above never reads.
  LASH_LAW_RECEIPTS="$receipts_dir/replay-law-receipts.txt" \
    python3 "{{repo}}/scripts/ci/restate_suite.py" suite effect-group --leg replay \
    --artifacts "$receipts_dir"

agent-workbench-attachment-usage-gate port='3030':
  bash "{{repo}}/scripts/agent-workbench-attachment-usage-gate.sh" "{{port}}"

restate-postgres-workers-e2e:
  bash "{{repo}}/scripts/restate-postgres-workers-e2e.sh"

process-operations-e2e:
  bash "{{repo}}/scripts/process-operations-e2e.sh"

version-bump-recreation-e2e:
  bash "{{repo}}/scripts/version-bump-recreation-e2e.sh"

# Fast live proof of the shared Postgres/S3/Restate gate isolation contract.
gate-container-smoke:
  bash "{{repo}}/scripts/gate-container-smoke.sh"

gate-worktree-concurrency-check peer:
  bash "{{repo}}/scripts/test-gate-worktree-concurrency.sh" "{{peer}}"

gate-stale-trace-regression:
  bash "{{repo}}/scripts/test-restate-workers-trace-scrub.sh"

session-lease-triage-e2e:
  bash "{{repo}}/scripts/session-lease-triage-e2e.sh"



context-overflow-recovery-e2e:
  bash "{{repo}}/scripts/context-overflow-recovery-e2e.sh"

stack-budget:
  bash "{{repo}}/scripts/ci-stack-budget.sh"

# Opt-in full local diagnostic for unusual risk, release work, or an explicit
# request. It is not a routine push or merge prerequisite; focused local checks
# plus CI's aggregate conclusion are the default proof path.
push-gate:
  bash "{{repo}}/scripts/push-gate.sh"

# Opt-in confidence diagnostics. Choose a lane only for a named risk it covers.
confidence lane='default':
  bash "{{repo}}/scripts/confidence-gate.sh" "{{lane}}"

confidence-fast:
  bash "{{repo}}/scripts/confidence-gate.sh" fast

confidence-broad:
  bash "{{repo}}/scripts/confidence-gate.sh" broad

confidence-full:
  bash "{{repo}}/scripts/confidence-gate.sh" full

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
# Bazel scans the example package; the remaining gates run concurrently and
# are reported as one table. The repository-gates leg skips
# `scripts/test-agent-workbench-dev-reset.sh` locally (170 s, it alone
# bounded the floor) and says so in its table row; CI's `Test repository
# scripts` job still runs it, and `scripts/ci/repository-gates.sh --all`
# restores it here. Run it on a COMMITTED head —
# check_version_bumps.py reads committed state, so work that exists only in
# the worktree is invisible to that leg.
floor:
  #!/usr/bin/env bash
  set -euo pipefail
  cd "{{repo}}"
  npm --prefix examples/workflow-graph-roundtrip/frontend ci
  printf '%s\n' \
    'kiln test //:dev_tests //:feature_lane_tests //:workspace_clippy //:feature_lane_clippy //:schema_checks' \
    'kiln fmt -- --check' \
    'git diff --check' \
    'scripts/ci/repository-gates.sh' \
    'npm --prefix examples/workflow-graph-roundtrip/frontend run check:generated-types' \
    'python3 scripts/check_version_bumps.py --base origin/main' \
    'python3 scripts/check_version_bump_fixtures.py' \
    'python3 scripts/check_format_registry.py' \
    'python3 scripts/check_checkpoint_component_flatten.py' \
    | scripts/gate-table.sh

# The store-bump gates only: both version-bump checks, the durable format
# registry, the store SQL ownership gate, the lash-sim schema congruence
# target, and the lash-core-store unit target that holds the runtime-error
# classification exhaustiveness test.
bump-check:
  #!/usr/bin/env bash
  set -euo pipefail
  cd "{{repo}}"
  printf '%s\n' \
    'python3 scripts/check_version_bumps.py --base origin/main' \
    'python3 scripts/check_version_bump_fixtures.py' \
    'python3 scripts/check_format_registry.py' \
    'python3 scripts/check-store-sql-ownership.py' \
    'kiln test //crates/lash-sim:schema_congruence__test //crates/lash-core-store:lash-core-store__unit_test' \
    | scripts/gate-table.sh

# Reverse-dependency selection uses the same input-identified plan as dev-test.
test-changed base='origin/main':
  python3 scripts/dev-test.py --base {{base}} --dependents

# Opt-in durable-store and session-graph property soak. PostgreSQL executes
# when its standard LASH_POSTGRES_DATABASE_URL configuration is present.
store-contract-soak cases='256':
  LASH_STORE_CONTRACT_PROPTEST_CASES="{{cases}}" kiln run //crates/lash-sqlite-store:conformance_memory__test -- store_contract_state_machine --nocapture
  LASH_STORE_CONTRACT_PROPTEST_CASES="{{cases}}" kiln run //crates/lash-sqlite-store:conformance__test -- store_contract_state_machine --nocapture
  LASH_STORE_CONTRACT_PROPTEST_CASES="{{cases}}" kiln run //crates/lash-postgres-store:conformance__test -- store_contract_state_machine --nocapture
  LASH_SESSION_GRAPH_PROPTEST_CASES="{{cases}}" kiln run //crates/lash-sqlite-store:conformance_memory__test -- session_graph_state_machine --nocapture
  LASH_SESSION_GRAPH_PROPTEST_CASES="{{cases}}" kiln run //crates/lash-sqlite-store:conformance__test -- session_graph_state_machine --nocapture
  LASH_SESSION_GRAPH_PROPTEST_CASES="{{cases}}" kiln run //crates/lash-postgres-store:conformance__test -- session_graph_state_machine --nocapture

# Opt-in runtime-persistence property soak. PostgreSQL executes when its
# standard LASH_POSTGRES_DATABASE_URL configuration is present.
runtime-persistence-soak cases='256':
  LASH_RUNTIME_PERSISTENCE_PROPTEST_CASES="{{cases}}" kiln run //crates/lash-sqlite-store:conformance_memory__test -- runtime_persistence_state_machine --nocapture
  LASH_RUNTIME_PERSISTENCE_PROPTEST_CASES="{{cases}}" kiln run //crates/lash-sqlite-store:conformance__test -- runtime_persistence_state_machine --nocapture
  LASH_RUNTIME_PERSISTENCE_PROPTEST_CASES="{{cases}}" kiln run //crates/lash-postgres-store:conformance__test -- runtime_persistence_state_machine --nocapture

# Opt-in three-backend raw durable-state soak. Requires the standard Postgres
# configuration and logs the operation kinds omitted by each bounded seed.
cross-backend-store-soak cases='64' seed='852':
  LASH_REQUIRE_POSTGRES=1 LASH_CROSS_BACKEND_CASES="{{cases}}" LASH_CROSS_BACKEND_SEED="{{seed}}" kiln run //crates/lash-sim:cross_backend_store_differential__test -- generated_cross_backend_surface_differential_agrees --nocapture --include-ignored

# The runtime leg gates on allocation ceilings and phase inventory only;
# wall-clock budgets print as advisories (see scripts/perf_guard_budgets.json,
# whose runtime scenarios split `enforced_allocation` from `advisory_duration`).
# The Lashlang iteration counts are part of the gate, not a speed knob: the
# cache-mode budgets in scripts/perf_guard_budgets.json are per-iteration costs
# of a fixed setup, so they only hold at the count they were calibrated at.
# Keep both counts equal to the ones perf.yml and release.yml run.
perf-guard:
  python3 "{{repo}}/scripts/profile_runtime.py" --profile quick --release --enforce-budgets --out "{{repo}}/.benchmarks/perf-guard/runtime-local.json"
  python3 "{{repo}}/scripts/profile_lashlang.py" --iterations 2500 --profile-iterations 2500 --enforce-budgets --out "{{repo}}/.benchmarks/perf-guard/lashlang-local.json"

release-version-test:
  python3 "{{repo}}/scripts/test_release_version.py"

release-automation-test:
  python3 "{{repo}}/scripts/test_release_version.py"
  python3 "{{repo}}/scripts/test_publish_workspace.py"
  python3 "{{repo}}/scripts/test_package_workspace.py"

# ── crates.io publishing ─────────────────────────────────────
# Show the publishable workspace set. The in-tree version is the 0.0.0-dev
# placeholder — the release publisher stamps the real version at packaging time
# and computes the dependency layers from cargo metadata
# (`python3 scripts/publish_workspace.py --plan --version X.Y.Z`).
publish-order:
  #!/usr/bin/env bash
  set -euo pipefail
  python3 - <<'PY'
  import json
  import subprocess

  metadata = json.loads(subprocess.check_output([
      "cargo",
      "metadata",
      "--format-version",
      "1",
      "--locked",
      "--no-deps",
  ], text=True))
  members = set(metadata["workspace_members"])
  publishable = sorted(
      package["name"]
      for package in metadata["packages"]
      if package["id"] in members and package.get("publish") != []
  )
  version = next(
      package["version"]
      for package in metadata["packages"]
      if package["name"] == "lash-runtime"
  )
  print(f"Workspace version: {version}")
  print()
  print("Publishable crates:")
  for index, name in enumerate(publishable, start=1):
      print(f"  {index:2}. {name}")
  PY

# The packaging proof the release runs before it publishes anything: package
# every publishable crate in one cargo invocation (so workspace siblings resolve
# against the crates just packaged, not against crates.io) and report the sha256
# of each .crate. `--no-verify` skips the per-crate verify builds.
package-workspace *args:
  python3 "{{repo}}/scripts/package_workspace.py" {{args}}

# Publish a single crate at the in-tree version. Idempotent: returns success if
# the same version is already on crates.io. NOTE: the in-tree version is the
# 0.0.0-dev placeholder unless you have stamped a real version first
# (`python3 scripts/release_version.py stamp X.Y.Z`); for a real release use the
# layered publisher (`python3 scripts/publish_workspace.py --version X.Y.Z`).
publish-one CRATE *args:
  #!/usr/bin/env bash
  set -euo pipefail
  version=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
  status=$(curl -s -o /dev/null -w "%{http_code}" \
    "https://crates.io/api/v1/crates/{{CRATE}}/$version")
  if [ "$status" = "200" ]; then
    echo "  ✓ {{CRATE}}@$version already on crates.io"
    exit 0
  fi
  echo "  → publishing {{CRATE}}@$version"
  cargo publish -p "{{CRATE}}" --no-verify --locked "$@"

# Publish every publishable workspace crate in dependency order. Re-runnable:
# already-published versions are skipped; transient crates.io/Cargo registry
# failures are retried by the helper.
publish-all *args:
  python3 "{{repo}}/scripts/publish_workspace.py" "$@"

check-file-size:
  python3 scripts/check-production-file-size.py
