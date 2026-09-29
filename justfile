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

# The runs share one restate-server the process starts, the pinned binary,
# each in a namespace of its own.
toolbench model='z-ai/glm-5.3-flash' *args:
  LASH_RESTATE_SERVER_BIN="$(python3 "{{repo}}/scripts/ci/restate_suite.py" server-path)" \
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
  cargo test -p agent-service \
    live_restate_ingress_runs_agent_turn_and_process_workflow_end_to_end -- --ignored --nocapture

agent-workbench-restate-e2e:
  bash "{{repo}}/scripts/agent-workbench-restate-e2e.sh"

# FIG-4042: token-free RLM warning and frame-switch companion for the manual
# workbench continue_as runbook. The provider responses are scripted in-process.
workbench-continue-as-budget-gate:
  #!/usr/bin/env bash
  set -euo pipefail
  kiln test //crates/lash-protocol-rlm:lash-protocol-rlm__unit_test --test_arg=budget_warning --test_sharding_strategy=disabled --test_output=errors
  kiln test //examples/agent-workbench:agent-workbench__unit_test --test_arg=continue_as_warning_override --test_sharding_strategy=disabled --test_output=errors
  kiln test //crates/lash-protocol-rlm:protocol_drivers__test --test_arg=scripted_context_budget_warning_reaches_model_and_continue_as_carries_only_seed --test_sharding_strategy=disabled --test_output=errors

# The regression gate for the Restate effect-group choreography. Its suites are
# `#[ignore]`d because they need a Restate server, so this recipe is the only
# thing that runs them: `scripts/ci/restate_suite.py` builds the test binary on
# the shared pool, runs every ignored law of the suite (it asks libtest for
# `--ignored` tests only, which `scripts/check_service_gate_pinning.py` pins)
# beside pinned `restate-server`s, one law per process, and then runs the same
# laws again with every await suspended and replayed (the replay leg). The
# suite's filters are registered in `scripts/restate-suites.toml`, its replay
# divergences in `scripts/restate-divergences/`.
effect-group-conformance-e2e:
  #!/usr/bin/env bash
  set -euo pipefail
  source "{{repo}}/scripts/worktree-gate-env.sh"
  lash_gate_acquire_locks effect-group-conformance-e2e

  # The test binaries run with the crate dir as cwd, so a relative artifact
  # dir (which is what CI exports) is anchored at the repo root.
  artifacts="${LASH_EFFECT_GROUP_ARTIFACT_DIR:-target/functional-e2e-artifacts/effect-group-conformance}"
  case "$artifacts" in
    /*) ;;
    *) artifacts="{{repo}}/$artifacts" ;;
  esac
  mkdir -p "$artifacts"

  python3 "{{repo}}/scripts/ci/restate_suite.py" suite effect-group --leg live \
    --artifacts "$artifacts"

  python3 "{{repo}}/scripts/ci/restate_suite.py" suite effect-group --leg replay \
    --artifacts "$artifacts"

# The server double's deployment laws against a live restate-server (FIG-3795
# part B): newest-deployment routing and invocation pinning, so the double
# cannot drift; and the deployment-namespace laws (FIG-3898): namespaced cores
# share the server, and a registration over another deployment's names is
# refused; and the crash windows (FIG-4095): a crash inside a process
# segment's handover, and between a tool presentation's put and its journaled
# outcome. Suite wiring lives in `scripts/restate-suites.toml` under
# `server-double`, `namespaces` and `crash-windows`.
server-double-e2e:
  #!/usr/bin/env bash
  set -euo pipefail
  source "{{repo}}/scripts/worktree-gate-env.sh"
  lash_gate_acquire_locks server-double-e2e

  python3 "{{repo}}/scripts/ci/restate_suite.py" suite server-double --leg live
  python3 "{{repo}}/scripts/ci/restate_suite.py" suite server-double --leg replay
  python3 "{{repo}}/scripts/ci/restate_suite.py" suite namespaces --leg live
  python3 "{{repo}}/scripts/ci/restate_suite.py" suite namespaces --leg replay
  python3 "{{repo}}/scripts/ci/restate_suite.py" suite crash-windows --leg live
  python3 "{{repo}}/scripts/ci/restate_suite.py" suite crash-windows --leg replay

# The crash-point matrix (FIG-3849) with a live `restate-server` as its engine
# (FIG-3872): every active cell of `lash_sim::crash_matrix::MATRIX` over its
# seeds, the deployment killed for real at each crash point (its endpoint's
# connections dropped, or cut at the journal frame the cell names), and the
# same invariants checked. The test binary is the double's own, switched by
# `LASH_CRASH_MATRIX_ENGINE=live`; it runs one cell at a time because every
# world shares the one server. Under `kiln gate lash <fork> -- just
# crash-matrix-restate-e2e` the gate's KILN_GATE_ID names the server and picks
# the port block.
crash-matrix-restate-e2e:
  #!/usr/bin/env bash
  set -euo pipefail
  if [ -n "${KILN_GATE_ID:-}" ] && [ -z "${LASH_GATE_SLOT_OVERRIDE:-}" ]; then
    gate_sum="$(printf '%s' "$KILN_GATE_ID" | cksum)"
    export LASH_GATE_SLOT_OVERRIDE="$(( ${gate_sum%% *} % 90 ))"
  fi
  source "{{repo}}/scripts/worktree-gate-env.sh"
  lash_gate_acquire_locks crash-matrix-restate-e2e
  gate="${KILN_GATE_ID:-lash-${LASH_GATE_WORKTREE_SLUG}}"

  artifacts="${LASH_CRASH_MATRIX_ARTIFACT_DIR:-target/functional-e2e-artifacts/crash-matrix-restate}"
  case "$artifacts" in
    /*) ;;
    *) artifacts="{{repo}}/$artifacts" ;;
  esac
  mkdir -p "$artifacts"
  log="$artifacts/crash-matrix.log"

  binary="$(python3 "{{repo}}/scripts/ci/restate_suite.py" build //crates/lash-sim:crash_point_matrix__test | tail -n 1)"

  # The server redelivers a failed attempt within a quarter second of the
  # deployment coming back, and never kills or pauses one on its own: a cell
  # judges a paused drive a wedge, so a retry budget must not decide it.
  set +e
  LASH_CRASH_MATRIX_ENGINE=live \
  LASH_CRASH_MATRIX_ENDPOINT_BIND="127.0.0.1:$((LASH_E2E_PORT_BASE + 33))" \
    timeout --kill-after=30 2400 \
    python3 "{{repo}}/scripts/ci/restate_suite.py" serve \
      --name "$gate" \
      --port-base "$((LASH_E2E_PORT_BASE + 30))" \
      --keep-log "$artifacts/restate-server.log" \
      --server-env RESTATE_DEFAULT_RETRY_POLICY__INITIAL_INTERVAL=10ms \
      --server-env RESTATE_DEFAULT_RETRY_POLICY__EXPONENTIATION_FACTOR=2.0 \
      --server-env RESTATE_DEFAULT_RETRY_POLICY__MAX_INTERVAL=250ms \
      --server-env RESTATE_DEFAULT_RETRY_POLICY__MAX_ATTEMPTS=1000000 \
      --server-env RESTATE_DEFAULT_RETRY_POLICY__ON_MAX_ATTEMPTS=pause \
      -- bash -c 'cd "$1" && exec "$2" --test-threads=1 --nocapture' _ \
        "{{repo}}/crates/lash-sim" "$binary" 2>&1 | tee "$log"
  status="${PIPESTATUS[0]}"
  set -e

  # The counts: every cell that ran says which engine it ran on.
  live_cells="$(grep -c ' on the live engine: ' "$log" || true)"
  other_cells="$(grep -E ' on the [a-z]+ engine: ' "$log" | grep -vc ' on the live engine: ' || true)"
  echo "crash matrix on live Restate: ${live_cells} cell(s) ran live, ${other_cells} elsewhere"
  grep -E ' on the [a-z]+ engine: ' "$log" || true
  grep -E '^test result: ' "$log" | tail -n 1 || true
  if [ "$status" -ne 0 ]; then
    echo "crash matrix on live Restate failed (exit $status); log: $log" >&2
    exit "$status"
  fi
  if [ "$live_cells" -eq 0 ] || [ "$other_cells" -ne 0 ]; then
    echo "crash matrix on live Restate: expected every cell on the live engine" >&2
    exit 1
  fi

# The send-to-completion latency gate (FIG-3843): `send()` → Restate drive →
# `outcome()` measured end to end on a live `restate-server`, the same-process
# fast fixture gated at overhead p50 < 50 ms / p99 < 250 ms over 10,000
# samples, every other case (stream, tool, failure, busy, controlled
# real-provider, cross-worker, poll, grace) measured and reported. The report
# and the raw sample ledger land under the artifact directory. The harness
# builds in the release profile — the same build the release's perf guard
# measures — so the budget binds optimized code, not a debug binary.
latency-gate:
  #!/usr/bin/env bash
  set -euo pipefail
  if [ -n "${KILN_GATE_ID:-}" ] && [ -z "${LASH_GATE_SLOT_OVERRIDE:-}" ]; then
    gate_sum="$(printf '%s' "$KILN_GATE_ID" | cksum)"
    export LASH_GATE_SLOT_OVERRIDE="$(( ${gate_sum%% *} % 90 ))"
  fi
  source "{{repo}}/scripts/worktree-gate-env.sh"
  lash_gate_acquire_locks latency-gate
  gate="${KILN_GATE_ID:-lash-${LASH_GATE_WORKTREE_SLUG}}"

  artifacts="${LASH_LATENCY_ARTIFACT_DIR:-target/functional-e2e-artifacts/latency-gate}"
  case "$artifacts" in
    /*) ;;
    *) artifacts="{{repo}}/$artifacts" ;;
  esac
  mkdir -p "$artifacts"
  log="$artifacts/latency-gate.log"

  cargo build --release --locked --package lash-perf --bin lash-perf
  binary="${CARGO_TARGET_DIR:-{{repo}}/target}/release/lash-perf"

  set +e
  LASH_LATENCY_WORKER_BIND="127.0.0.1:$((LASH_E2E_PORT_BASE + 39))" \
    timeout --kill-after=30 5400 \
    python3 "{{repo}}/scripts/ci/restate_suite.py" serve \
      --name "$gate" \
      --port-base "$((LASH_E2E_PORT_BASE + 36))" \
      --keep-log "$artifacts/restate-server.log" \
      -- "$binary" latency \
        --out "$artifacts/latency-report.json" \
        --samples-out "$artifacts/latency-samples.json" \
        --store-dir "$artifacts/stores" 2>&1 | tee "$log"
  status="${PIPESTATUS[0]}"
  set -e

  grep -E '^latency gate: ' "$log" || true
  if [ "$status" -ne 0 ]; then
    echo "latency gate failed (exit $status); log: $log" >&2
    exit "$status"
  fi

# Builds Phase A's two builds of head (ADR 0115 §6, FIG-3805) with Bazel: N
# (the default build) and N+1 (the `synthetic-next` feature), each as a
# `lash-upgrade-node` and a `lashctl`, copied into `<artifacts>/bin/n` and
# `<artifacts>/bin/n+1`. Each feature variant's label is read from its
# generated BUILD file, so a change to either build's feature set moves no
# recipe.
_upgrade-harness-builds artifacts:
  #!/usr/bin/env bash
  set -euo pipefail
  cd "{{repo}}"
  artifacts="{{artifacts}}"
  mkdir -p "$artifacts/bin/n" "$artifacts/bin/n+1"
  # variant <package> <target> <none|synthetic-next>: the target's feature
  # binary built with no features, or with `synthetic-next` alone.
  variant() {
    awk -v target="$2" -v want="$3" '
      /^lash_rust_feature_binary\(/ { block = 1; name = ""; features = "" }
      block && name == "" && /^    name = "/ { split($0, part, "\""); name = part[2] }
      block && /^    crate_features = \[\],/ { features = "none" }
      block && /^    crate_features = \[$/ { getline; if ($0 ~ /^        "synthetic-next",$/) features = "synthetic-next" }
      block && /^\)/ { if (index(name, target "__fv_") == 1 && features == want) print name; block = 0 }
    ' "$1/BUILD.bazel"
  }
  node_n="$(variant crates/lash-upgrade-harness lash-upgrade-node__bin none)"
  node_next="$(variant crates/lash-upgrade-harness lash-upgrade-node__bin synthetic-next)"
  lashctl_next="$(variant crates/lashctl lashctl synthetic-next)"
  for resolved in "$node_n" "$node_next" "$lashctl_next"; do
    if [ -z "$resolved" ] || [ "$(printf '%s\n' "$resolved" | wc -l)" -ne 1 ]; then
      echo "cannot resolve the Phase A feature variants: '$node_n' '$node_next' '$lashctl_next'" >&2
      exit 1
    fi
  done
  bazel_startup=()
  if [ -n "${BAZEL_OUTPUT_USER_ROOT:-}" ]; then
    bazel_startup=(--output_user_root="$BAZEL_OUTPUT_USER_ROOT")
  fi
  read -r -a bazel_flags <<< "${BAZEL_SHARED_CACHE_FLAGS:---config=shared}"
  bazel "${bazel_startup[@]}" build "${bazel_flags[@]}" --remote_download_outputs=all \
    "//crates/lash-upgrade-harness:$node_next" \
    "//crates/lash-upgrade-harness:$node_n" \
    //crates/lashctl:lashctl \
    "//crates/lashctl:$lashctl_next"
  cp "bazel-bin/crates/lash-upgrade-harness/$node_next" "$artifacts/bin/n+1/lash-upgrade-node"
  cp "bazel-bin/crates/lash-upgrade-harness/$node_n" "$artifacts/bin/n/lash-upgrade-node"
  cp bazel-bin/crates/lashctl/lashctl "$artifacts/bin/n/lashctl"
  cp "bazel-bin/crates/lashctl/$lashctl_next" "$artifacts/bin/n+1/lashctl"

# Phase A's rolling upgrade (ADR 0115 §6, FIG-3805): head built twice, N
# (the default build) and N+1 (the `synthetic-next` feature), run as separate
# `lash-upgrade-node` processes over real PostgreSQL, a SQLite store directory
# and one live `restate-server`. Bazel builds both nodes and lashctl. The
# operator binary runs the PostgreSQL version, migrate, preflight, drain,
# finalize and contract steps, over a database the run creates for itself;
# SQLite migrates on open. The `phase_a` legs run under `just phase-a`.
# `LASH_POSTGRES_DATABASE_URL` reuses a server the caller provides; otherwise
# a throwaway pg16 container serves the run. Evidence (the step report and every node's log) lands under the
# artifact directory, which `runbooks/rolling-upgrade/` judges.
e2e-rolling:
  #!/usr/bin/env bash
  set -euo pipefail
  cd "{{repo}}"
  artifacts="${LASH_E2E_ROLLING_ARTIFACT_DIR:-target/functional-e2e-artifacts/e2e-rolling}"
  case "$artifacts" in
    /*) ;;
    *) artifacts="{{repo}}/$artifacts" ;;
  esac
  rm -rf "$artifacts"
  just _upgrade-harness-builds "$artifacts"
  cargo test --locked -p lash-upgrade-harness --test rolling --no-run

  export LASH_UPGRADE_NODE_N="$artifacts/bin/n/lash-upgrade-node"
  export LASH_UPGRADE_NODE_NEXT="$artifacts/bin/n+1/lash-upgrade-node"
  export LASH_UPGRADE_LASHCTL_N="$artifacts/bin/n/lashctl"
  export LASH_UPGRADE_LASHCTL_NEXT="$artifacts/bin/n+1/lashctl"
  export LASH_E2E_ROLLING_ARTIFACT_DIR="$artifacts"
  run=(
    python3 scripts/ci/restate_suite.py serve --name e2e-rolling
      --keep-log "$artifacts/restate-server.log"
      -- cargo test --locked -p lash-upgrade-harness --test rolling
      -- --ignored --exact roll_and_rollback_smoke --nocapture
  )
  if [ -n "${LASH_POSTGRES_DATABASE_URL:-}" ]; then
    "${run[@]}" 2>&1 | tee "$artifacts/e2e-rolling.log"
  else
    scripts/ci/with-service.sh pg16 -- "${run[@]}" 2>&1 | tee "$artifacts/e2e-rolling.log"
  fi

# Phase A's legs (ADR 0115 §6, FIG-3805) over the same two builds, real
# PostgreSQL (each leg creates a database of its own) and one live
# `restate-server` whose retries back off within a second, so a leg that
# crashes or swaps a deployment sees Restate redeliver promptly. Name the
# legs to run (`just phase-a negotiated_wire_both_directions`); with none,
# every leg runs, and a leg whose lane has not landed fails by design.
# `LASH_POSTGRES_DATABASE_URL` reuses a server the caller provides; otherwise
# a throwaway pg16 container serves the run. Each leg's evidence lands under
# the artifact directory.
phase-a *legs:
  #!/usr/bin/env bash
  set -euo pipefail
  cd "{{repo}}"
  artifacts="${LASH_PHASE_A_ARTIFACT_DIR:-target/functional-e2e-artifacts/phase-a}"
  case "$artifacts" in
    /*) ;;
    *) artifacts="{{repo}}/$artifacts" ;;
  esac
  rm -rf "$artifacts"
  just _upgrade-harness-builds "$artifacts"
  cargo test --locked -p lash-upgrade-harness --test phase_a --no-run

  export LASH_UPGRADE_NODE_N="$artifacts/bin/n/lash-upgrade-node"
  export LASH_UPGRADE_NODE_NEXT="$artifacts/bin/n+1/lash-upgrade-node"
  export LASH_UPGRADE_LASHCTL_N="$artifacts/bin/n/lashctl"
  export LASH_UPGRADE_LASHCTL_NEXT="$artifacts/bin/n+1/lashctl"
  export LASH_PHASE_A_ARTIFACT_DIR="$artifacts"
  filters=()
  for leg in {{legs}}; do
    filters+=("$leg::$leg")
  done
  if [ "${#filters[@]}" -gt 0 ]; then
    filters=(--exact "${filters[@]}")
  fi
  run=(
    python3 scripts/ci/restate_suite.py serve --name phase-a
      --server-env RESTATE_DEFAULT_RETRY_POLICY__INITIAL_INTERVAL=50ms
      --server-env RESTATE_DEFAULT_RETRY_POLICY__EXPONENTIATION_FACTOR=2.0
      --server-env RESTATE_DEFAULT_RETRY_POLICY__MAX_INTERVAL=1s
      --keep-log "$artifacts/restate-server.log"
      -- cargo test --locked -p lash-upgrade-harness --test phase_a
      -- --include-ignored --test-threads 1 --nocapture "${filters[@]}"
  )
  if [ -n "${LASH_POSTGRES_DATABASE_URL:-}" ]; then
    "${run[@]}" 2>&1 | tee "$artifacts/phase-a.log"
  else
    scripts/ci/with-service.sh pg16 -- "${run[@]}" 2>&1 | tee "$artifacts/phase-a.log"
  fi

agent-workbench-attachment-usage-gate port='3030':
  bash "{{repo}}/scripts/agent-workbench-attachment-usage-gate.sh" "{{port}}"

restate-postgres-workers-e2e:
  bash "{{repo}}/scripts/restate-postgres-workers-e2e.sh"

process-operations-e2e:
  bash "{{repo}}/scripts/process-operations-e2e.sh"

# Fast live proof of the shared Postgres/S3/Restate gate isolation contract.
gate-container-smoke:
  bash "{{repo}}/scripts/gate-container-smoke.sh"

gate-worktree-concurrency-check peer:
  bash "{{repo}}/scripts/test-gate-worktree-concurrency.sh" "{{peer}}"

gate-stale-trace-regression:
  bash "{{repo}}/scripts/test-restate-workers-trace-scrub.sh"

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
# restores it here. Run it on a COMMITTED head so the gates judge the tree
# that will land.
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

# The release gate's chaos soak (FIG-3873): randomized lash-sim workloads
# under deployment kills, leader-lease loss and rolling deploys on the Restate
# server double, checked against the crash matrix's invariants. An empty
# `seed` draws one from the clock; the soak prints it, and a failed epoch
# prints the `LASH_CHAOS_SOAK_*` settings that replay it alone. release.yml's
# `chaos-soak` job runs the same test under Cargo.
chaos-soak duration='90m' seed='':
  LASH_CHAOS_SOAK_DURATION="{{duration}}" LASH_CHAOS_SOAK_SEED="{{seed}}" kiln run //crates/lash-sim:chaos_soak__test -- chaos_soak_release --exact --ignored --nocapture

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

# Foreground local Kubernetes proof for the FIG-3790 topology.
# Run through kiln gate so every cluster, namespace and artifact has an owner.
multi-node-load:
  bash "{{repo}}/scripts/multi-node-load.sh"

loadtest-chart-check:
  bash "{{repo}}/scripts/check-loadtest-chart.sh"
  python3 "{{repo}}/scripts/test_loadtest_topology.py"
