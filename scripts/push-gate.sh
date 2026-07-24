#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

ci_features="${LASH_CI_FEATURES:-}"
port_base="${LASH_PUSH_GATE_PORT_BASE:-$((20000 + ($$ % 20000)))}"
postgres_container=""

cleanup() {
  if [ -n "$postgres_container" ]; then
    docker rm -f "$postgres_container" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

step() {
  printf '\n==> %s\n' "$*"
}


configure_bindgen_headers() {
  if [ -n "${BINDGEN_EXTRA_CLANG_ARGS:-}" ]; then
    return
  fi
  local stddef gcc_include
  stddef="$(find /usr/lib/gcc -path '*/include/stddef.h' 2>/dev/null | sort -V | tail -n 1)"
  if [ -n "$stddef" ]; then
    gcc_include="$(dirname "$stddef")"
    export BINDGEN_EXTRA_CLANG_ARGS="-I${gcc_include}"
  fi
}

run_release_script_tests() {
  step "Release automation script tests"
  python3 scripts/test_release_version.py
  python3 scripts/test_publish_workspace.py
  python3 scripts/test_release_notes.py
}

run_runtime_feature_boundary_check() {
  step "lash-runtime feature boundary"
  cargo check -p lash-runtime --no-default-features --locked
  cargo check -p lash-runtime --no-default-features --features testing --locked

  if cargo tree -p lash-runtime -e normal --no-default-features --locked \
    | grep -E 'lash-protocol-rlm|lash-lashlang-runtime|lashlang'; then
    echo "default-off lash-runtime pulled RLM/Lashlang dependencies" >&2
    exit 1
  fi

  if cargo tree -p lash-runtime -e normal --locked \
    | grep -E 'lash-protocol-rlm|lash-lashlang-runtime|lashlang'; then
    echo "default lash-runtime pulled RLM/Lashlang dependencies" >&2
    exit 1
  fi

  if cargo tree -p lash-runtime -e normal --no-default-features --features testing --locked \
    | grep -E 'lash-protocol-rlm|lash-lashlang-runtime|lashlang'; then
    echo "testing-only lash-runtime pulled RLM/Lashlang dependencies" >&2
    exit 1
  fi
}

run_workspace_tests() {
  step "Workspace tests"
  if cargo nextest --version >/dev/null 2>&1; then
    # shellcheck disable=SC2086
    cargo nextest run --workspace --locked ${ci_features}
  else
    echo "cargo-nextest is not installed; falling back to cargo test for local push gate." >&2
    # shellcheck disable=SC2086
    cargo test --workspace --locked ${ci_features}
  fi
}

run_postgres_conformance() {
  step "Postgres conformance"
  postgres_container="lash-postgres-push-gate-$$"
  local port="${LASH_PUSH_GATE_POSTGRES_PORT:-$((port_base + 10))}"
  docker rm -f "$postgres_container" >/dev/null 2>&1 || true
  bash scripts/docker-pull-with-retry.sh postgres:16-alpine
  docker run -d --name "$postgres_container" \
    -e POSTGRES_USER=lash \
    -e POSTGRES_PASSWORD=lash \
    -e POSTGRES_DB=lash \
    -p "127.0.0.1:${port}:5432" \
    postgres:16-alpine >/dev/null

  local deadline=$((SECONDS + 60))
  until docker exec "$postgres_container" pg_isready -U lash -d lash >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
      docker logs "$postgres_container" >&2 || true
      echo "Postgres did not become ready on port ${port}" >&2
      exit 1
    fi
    sleep 1
  done

  LASH_POSTGRES_DATABASE_URL="postgres://lash:lash@127.0.0.1:${port}/lash" \
    cargo test -p lash-postgres-store --locked
}

configure_bindgen_headers

step "Formatting"
cargo fmt --all --check

step "Clippy"
# shellcheck disable=SC2086
cargo clippy --workspace --all-targets --locked ${ci_features} -- -D warnings

step "Core/UI boundary guard"
bash scripts/check-core-ui-boundary.sh

step "Workflow graph model guard"
bash scripts/check-workflow-graph-model.sh

step "Production file-size budget guard"
bash scripts/check-production-file-size.sh

step "Docs lint"
python3 scripts/lint_docs.py

step "Test quarantine metadata"
python3 scripts/check_test_quarantines.py

run_release_script_tests

step "Workspace check"
# shellcheck disable=SC2086
cargo check --workspace --all-targets --locked ${ci_features}

run_runtime_feature_boundary_check
run_workspace_tests

step "Workspace doctests"
# shellcheck disable=SC2086
cargo test --doc --workspace --locked ${ci_features}

run_postgres_conformance

step "Workflow graph example integration"
just workflow-graph-integration-verify

step "Restate e2e: agent-service"
RESTATE_ADMIN_PORT="$((port_base + 20))" \
RESTATE_INGRESS_PORT="$((port_base + 21))" \
RESTATE_NODE_PORT="$((port_base + 22))" \
AGENT_SERVICE_E2E_ENDPOINT_BIND="127.0.0.1:$((port_base + 23))" \
AGENT_SERVICE_E2E_ENDPOINT_URL="http://127.0.0.1:$((port_base + 23))" \
  just agent-service-restate-e2e

step "Restate e2e: agent-workbench"
AGENT_WORKBENCH_RESTATE_ADMIN_PORT="$((port_base + 30))" \
AGENT_WORKBENCH_RESTATE_INGRESS_PORT="$((port_base + 31))" \
AGENT_WORKBENCH_RESTATE_NODE_PORT="$((port_base + 32))" \
AGENT_WORKBENCH_E2E_ENDPOINT_BIND="127.0.0.1:$((port_base + 33))" \
AGENT_WORKBENCH_E2E_ENDPOINT_URL="http://127.0.0.1:$((port_base + 33))" \
  just agent-workbench-restate-e2e

step "Restate/Postgres/MinIO workers e2e"
LASH_E2E_MINIO_PORT="$((port_base + 40))" \
  bash scripts/restate-postgres-workers-e2e.sh

step "Push gate passed"
