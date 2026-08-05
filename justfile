set positional-arguments

repo := justfile_directory()

default:
  @just --list

agent-workbench port='3030':
  ./scripts/agent-workbench-dev.sh up --port "{{port}}"

agent-workbench-up port='3030':
  ./scripts/agent-workbench-dev.sh up --port "{{port}}"

agent-workbench-restart port='3030':
  ./scripts/agent-workbench-dev.sh restart --port "{{port}}"

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

workflow-graph-roundtrip port='3031':
  #!/usr/bin/env bash
  set -euo pipefail
  target_dir="${WORKFLOW_GRAPH_TARGET_DIR:-/tmp/lash-workflow-graph-{{port}}}"
  npm --prefix "{{repo}}/examples/workflow-graph-roundtrip/frontend" ci
  npm --prefix "{{repo}}/examples/workflow-graph-roundtrip/frontend" run build
  WORKFLOW_GRAPH_ADDR="127.0.0.1:{{port}}" CARGO_TARGET_DIR="$target_dir" \
    cargo run -p workflow-graph-roundtrip

workflow-graph-integration-verify:
  npm --prefix "{{repo}}/examples/workflow-graph-roundtrip/frontend" ci
  npm --prefix "{{repo}}/examples/workflow-graph-roundtrip/frontend" run build
  cargo test -p workflow-graph-roundtrip --all-targets --locked
  bash "{{repo}}/scripts/check-workflow-graph-model.sh"

agent-service-restate-e2e:
  #!/usr/bin/env bash
  set -euo pipefail
  source "{{repo}}/scripts/worktree-gate-env.sh"
  lash_gate_acquire agent-service-restate-e2e
  image="${AGENT_SERVICE_RESTATE_IMAGE:-restatedev/restate:1.7.0}"
  container="${AGENT_SERVICE_RESTATE_CONTAINER:-lash-agent-service-restate-${LASH_GATE_WORKTREE_SLUG}}"
  admin_port="${RESTATE_ADMIN_PORT:-$((LASH_E2E_PORT_BASE + 20))}"
  ingress_port="${RESTATE_INGRESS_PORT:-$((LASH_E2E_PORT_BASE + 21))}"
  node_port="${RESTATE_NODE_PORT:-$((LASH_E2E_PORT_BASE + 22))}"
  endpoint_bind="${AGENT_SERVICE_E2E_ENDPOINT_BIND:-127.0.0.1:$((LASH_E2E_PORT_BASE + 23))}"
  endpoint_url="${AGENT_SERVICE_E2E_ENDPOINT_URL:-http://127.0.0.1:$((LASH_E2E_PORT_BASE + 23))}"
  admin_url="${RESTATE_ADMIN_URL:-http://127.0.0.1:$admin_port}"
  ingress_url="${RESTATE_INGRESS_URL:-http://127.0.0.1:$ingress_port}"

  cleanup() {
    docker rm -f "$container" >/dev/null 2>&1 || true
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

  RESTATE_INGRESS_URL="$ingress_url" \
  RESTATE_ADMIN_URL="$admin_url" \
  AGENT_SERVICE_E2E_ENDPOINT_BIND="$endpoint_bind" \
  AGENT_SERVICE_E2E_ENDPOINT_URL="$endpoint_url" \
  cargo test -p agent-service --features restate \
    live_restate_ingress_runs_agent_turn_and_process_workflow_end_to_end -- --ignored --nocapture

agent-workbench-restate-e2e:
  #!/usr/bin/env bash
  set -euo pipefail
  source "{{repo}}/scripts/worktree-gate-env.sh"
  lash_gate_acquire agent-workbench-restate-e2e
  image="${AGENT_WORKBENCH_RESTATE_IMAGE:-restatedev/restate:1.7.0}"
  container="${AGENT_WORKBENCH_RESTATE_CONTAINER:-lash-agent-workbench-restate-${LASH_GATE_WORKTREE_SLUG}}"
  admin_port="${AGENT_WORKBENCH_RESTATE_ADMIN_PORT:-$((LASH_E2E_PORT_BASE + 30))}"
  ingress_port="${AGENT_WORKBENCH_RESTATE_INGRESS_PORT:-$((LASH_E2E_PORT_BASE + 31))}"
  node_port="${AGENT_WORKBENCH_RESTATE_NODE_PORT:-$((LASH_E2E_PORT_BASE + 32))}"
  endpoint_bind="${AGENT_WORKBENCH_E2E_ENDPOINT_BIND:-127.0.0.1:$((LASH_E2E_PORT_BASE + 33))}"
  endpoint_url="${AGENT_WORKBENCH_E2E_ENDPOINT_URL:-http://127.0.0.1:$((LASH_E2E_PORT_BASE + 33))}"
  postgres_container="${AGENT_WORKBENCH_E2E_POSTGRES_CONTAINER:-lash-agent-workbench-postgres-${LASH_GATE_WORKTREE_SLUG}}"
  postgres_port="${AGENT_WORKBENCH_E2E_POSTGRES_PORT:-$((LASH_E2E_PORT_BASE + 34))}"
  database_url="${AGENT_WORKBENCH_E2E_DATABASE_URL:-postgres://lash:lash@127.0.0.1:$postgres_port/lash}"
  admin_url="${RESTATE_ADMIN_URL:-http://127.0.0.1:$admin_port}"
  ingress_url="${RESTATE_INGRESS_URL:-http://127.0.0.1:$ingress_port}"
  test_output="$(mktemp "${TMPDIR:-/tmp}/lash-agent-workbench-restate-e2e-${LASH_GATE_WORKTREE_SLUG}.XXXXXX")"

  cleanup() {
    docker rm -f "$container" >/dev/null 2>&1 || true
    docker rm -f "$postgres_container" >/dev/null 2>&1 || true
    rm -f "$test_output"
  }
  trap cleanup EXIT

  bash "{{repo}}/scripts/docker-pull-with-retry.sh" "$image"
  bash "{{repo}}/scripts/docker-pull-with-retry.sh" postgres:16-alpine

  docker run -d --name "$container" --label "$LASH_GATE_LABEL" --network host \
    -e RESTATE_ADMIN__BIND_PORT="$admin_port" \
    -e RESTATE_INGRESS__BIND_PORT="$ingress_port" \
    -e RESTATE_BIND_PORT="$node_port" \
    "$image" >/dev/null
  docker run -d --name "$postgres_container" --label "$LASH_GATE_LABEL" --network host \
    -e POSTGRES_USER=lash \
    -e POSTGRES_PASSWORD=lash \
    -e POSTGRES_DB=lash \
    postgres:16-alpine -p "$postgres_port" >/dev/null

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
  until (echo >"/dev/tcp/127.0.0.1/$postgres_port") >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
      docker logs "$postgres_container" >&2 || true
      echo "Postgres port $postgres_port did not become ready" >&2
      exit 1
    fi
    sleep 1
  done

  RESTATE_INGRESS_URL="$ingress_url" \
  RESTATE_ADMIN_URL="$admin_url" \
  AGENT_WORKBENCH_E2E_ENDPOINT_BIND="$endpoint_bind" \
  AGENT_WORKBENCH_E2E_ENDPOINT_URL="$endpoint_url" \
  AGENT_WORKBENCH_E2E_DATABASE_URL="$database_url" \
  cargo test -p agent-workbench \
    live_restate_ -- --ignored --nocapture --test-threads=1 \
    2>&1 | tee "$test_output"
  cargo test -p lash-core \
    turn_input_claims_supersede_across_session_lease_generations \
    2>&1 | tee -a "$test_output"
  if grep -Fn 'panicked at' "$test_output" >&2; then
    echo "panic gate: FAILED ('panicked at' found in agent-workbench Restate E2E output)" >&2
    exit 1
  fi
  echo "panic gate: clean (no 'panicked at' lines in agent-workbench Restate E2E output)"

agent-workbench-attachment-usage-gate port='3030':
  bash "{{repo}}/scripts/agent-workbench-attachment-usage-gate.sh" "{{port}}"

restate-postgres-workers-e2e:
  bash "{{repo}}/scripts/restate-postgres-workers-e2e.sh"

process-operations-e2e:
  bash "{{repo}}/scripts/process-operations-e2e.sh"

version-bump-recreation-e2e:
  bash "{{repo}}/scripts/version-bump-recreation-e2e.sh"

# Fast live proof of the shared Postgres/MinIO/Restate gate isolation contract.
gate-container-smoke:
  bash "{{repo}}/scripts/gate-container-smoke.sh"

gate-worktree-concurrency-check peer:
  bash "{{repo}}/scripts/test-gate-worktree-concurrency.sh" "{{peer}}"

gate-stale-trace-regression:
  bash "{{repo}}/scripts/test-restate-workers-trace-scrub.sh"

session-lease-triage-e2e:
  bash "{{repo}}/scripts/session-lease-triage-e2e.sh"

graceful-drain-e2e:
  bash "{{repo}}/scripts/graceful-drain-e2e.sh"

request-abandon-e2e:
  bash "{{repo}}/scripts/request-abandon-e2e.sh"

stack-budget:
  bash "{{repo}}/scripts/ci-stack-budget.sh"

push-gate:
  bash "{{repo}}/scripts/push-gate.sh"

confidence lane='default':
  bash "{{repo}}/scripts/confidence-gate.sh" "{{lane}}"

confidence-fast:
  bash "{{repo}}/scripts/confidence-gate.sh" fast

confidence-broad:
  bash "{{repo}}/scripts/confidence-gate.sh" broad

confidence-full:
  bash "{{repo}}/scripts/confidence-gate.sh" full

# Opt-in durable-store and session-graph property soak. PostgreSQL executes
# when its standard LASH_POSTGRES_DATABASE_URL configuration is present.
store-contract-soak cases='256':
  LASH_STORE_CONTRACT_PROPTEST_CASES="{{cases}}" cargo test -p lash-core --locked store_contract_state_machine_properties -- --nocapture
  LASH_STORE_CONTRACT_PROPTEST_CASES="{{cases}}" cargo test -p lash-sqlite-store --locked --test conformance store_contract_state_machine_properties -- --nocapture
  LASH_STORE_CONTRACT_PROPTEST_CASES="{{cases}}" cargo test -p lash-postgres-store --locked --test conformance store_contract_state_machine_properties_when_configured -- --nocapture
  LASH_SESSION_GRAPH_PROPTEST_CASES="{{cases}}" cargo test -p lash-core --locked session_graph_state_machine_properties -- --nocapture
  LASH_SESSION_GRAPH_PROPTEST_CASES="{{cases}}" cargo test -p lash-sqlite-store --locked --test conformance session_graph_state_machine_properties -- --nocapture
  LASH_SESSION_GRAPH_PROPTEST_CASES="{{cases}}" cargo test -p lash-postgres-store --locked --test conformance session_graph_state_machine_properties_when_configured -- --nocapture

# Opt-in runtime-persistence property soak. PostgreSQL executes when its
# standard LASH_POSTGRES_DATABASE_URL configuration is present.
runtime-persistence-soak cases='256':
  LASH_RUNTIME_PERSISTENCE_PROPTEST_CASES="{{cases}}" cargo test -p lash-core --locked runtime_persistence_state_machine_properties -- --nocapture
  LASH_RUNTIME_PERSISTENCE_PROPTEST_CASES="{{cases}}" cargo test -p lash-sqlite-store --locked --test conformance runtime_persistence_state_machine_properties -- --nocapture
  LASH_RUNTIME_PERSISTENCE_PROPTEST_CASES="{{cases}}" cargo test -p lash-postgres-store --locked --test conformance runtime_persistence_state_machine_properties_when_configured -- --nocapture

# Opt-in three-backend raw durable-state soak. Requires the standard Postgres
# configuration and logs the operation kinds omitted by each bounded seed.
cross-backend-store-soak cases='64' seed='852':
  LASH_REQUIRE_POSTGRES=1 LASH_CROSS_BACKEND_CASES="{{cases}}" LASH_CROSS_BACKEND_SEED="{{seed}}" cargo test -p lash-sim --locked --test cross_backend_store_differential generated_cross_backend_surface_differential_agrees -- --nocapture

perf-guard:
  python3 "{{repo}}/scripts/profile_runtime.py" --profile quick --release --enforce-budgets --out "{{repo}}/.benchmarks/perf-guard/runtime-local.json"
  python3 "{{repo}}/scripts/profile_lashlang.py" --iterations 500 --profile-iterations 500 --enforce-budgets --out "{{repo}}/.benchmarks/perf-guard/lashlang-local.json"

release-version-test:
  python3 "{{repo}}/scripts/test_release_version.py"

release-automation-test:
  python3 "{{repo}}/scripts/test_release_version.py"
  python3 "{{repo}}/scripts/test_publish_workspace.py"

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

# Dry-run the two leaf crates (no internal deps) — quick sanity check.
# Non-leaf dry-runs only work after their deps are already on crates.io.
publish-dry-run:
  @echo "Dry-run on leaf crates (lash-sansio, lashlang)..."
  cargo publish --dry-run --locked -p lash-sansio
  cargo publish --dry-run --locked -p lashlang
  @echo "OK."

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
  ./scripts/check-production-file-size.sh
