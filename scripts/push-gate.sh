#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"
lash_gate_acquire push-gate

ci_features="${LASH_CI_FEATURES:-}"
port_base="${LASH_PUSH_GATE_PORT_BASE:-$LASH_E2E_PORT_BASE}"
postgres_container=""
minio_container=""

cleanup() {
  if [ -n "$postgres_container" ]; then
    docker rm -f "$postgres_container" >/dev/null 2>&1 || true
  fi
  if [ -n "$minio_container" ]; then
    docker rm -f "$minio_container" >/dev/null 2>&1 || true
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
  step "Repository script tests"
  python3 scripts/test_check_durable_read_fixture_version.py
  python3 scripts/test_check_postgres_json_carrier_coverage.py
  python3 scripts/test_check_postgres_payload_shape_version.py
  python3 scripts/test_check_transcript_diff.py
  python3 scripts/test_release_version.py
  python3 scripts/test_publish_workspace.py
  python3 scripts/test_release_notes.py
}

resolve_release_notes_base() {
  local configured_base="${LASH_PR_BASE_REF:-${GITHUB_BASE_REF:-}}"
  if [ -z "$configured_base" ] && command -v gh >/dev/null 2>&1; then
    configured_base="$(gh pr view --json baseRefName --jq .baseRefName 2>/dev/null || true)"
  fi
  configured_base="${configured_base:-main}"

  local candidates
  case "$configured_base" in
    refs/* | origin/*) candidates=("$configured_base") ;;
    *) candidates=("origin/$configured_base" "$configured_base") ;;
  esac

  local candidate
  for candidate in "${candidates[@]}"; do
    if git rev-parse --verify --quiet "${candidate}^{commit}" >/dev/null; then
      printf '%s\n' "$candidate"
      return
    fi
  done

  echo "Cannot resolve release-note base ref: $configured_base" >&2
  return 1
}

check_current_branch_release_notes() {
  step "Current branch release notes"
  local base_ref merge_base
  base_ref="$(resolve_release_notes_base)"
  merge_base="$(git merge-base "$base_ref" HEAD)"
  python3 scripts/release_notes.py check-pr --range "${merge_base}..HEAD"
}

run_runtime_feature_boundary_check() {
  step "lash-runtime feature boundary"
  cargo check -p lash-runtime --no-default-features --locked
  cargo check -p lash-runtime --no-default-features --features testing --locked
  cargo test -p lash-runtime --no-default-features --locked
  count=$(cargo test -p lash-runtime --no-default-features --locked --lib -- --list | grep -c ': test$')
  [ "$count" -ge 130 ] || { echo "default-build lash-runtime tests regressed: $count"; exit 1; }

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
    env -u LASH_POSTGRES_DATABASE_URL -u LASH_REQUIRE_POSTGRES \
      cargo nextest run --workspace --locked ${ci_features}
  else
    echo "cargo-nextest is not installed; falling back to cargo test for local push gate." >&2
    # shellcheck disable=SC2086
    env -u LASH_POSTGRES_DATABASE_URL -u LASH_REQUIRE_POSTGRES \
      cargo test --workspace --locked ${ci_features}
  fi
}

run_postgres_conformance() {
  step "Postgres conformance"
  postgres_container="lash-postgres-push-gate-${LASH_GATE_WORKTREE_SLUG}"
  local port="${LASH_PUSH_GATE_POSTGRES_PORT:-$((port_base + 10))}"
  bash scripts/docker-pull-with-retry.sh postgres:16-alpine
  docker run -d --name "$postgres_container" \
    --label "$LASH_GATE_LABEL" \
    --network "$LASH_E2E_NETWORK" \
    -e POSTGRES_USER=lash \
    -e POSTGRES_PASSWORD=lash \
    -e POSTGRES_DB=lash \
    -p "127.0.0.1:${port}:5432" \
    postgres:16-alpine -c shared_preload_libraries=pg_stat_statements >/dev/null

  local deadline=$((SECONDS + 60))
  until docker exec "$postgres_container" pg_isready -U lash -d lash >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
      docker logs "$postgres_container" >&2 || true
      echo "Postgres did not become ready on port ${port}" >&2
      exit 1
    fi
    sleep 1
  done

  local database_url="postgres://lash:lash@127.0.0.1:${port}/lash"
  LASH_POSTGRES_DATABASE_URL="$database_url" \
    LASH_REQUIRE_POSTGRES=1 \
    cargo test -p lash-postgres-store --locked

  step "Cross-backend store differential"
  if cargo nextest --version >/dev/null 2>&1; then
    LASH_POSTGRES_DATABASE_URL="$database_url" \
      LASH_REQUIRE_POSTGRES=1 \
      LASH_CROSS_BACKEND_CASES="${LASH_CROSS_BACKEND_PR_CASES:-4}" \
      cargo nextest run -p lash-sim \
        --test cross_backend_store_differential \
        --locked -j1 --no-capture
  else
    LASH_POSTGRES_DATABASE_URL="$database_url" \
      LASH_REQUIRE_POSTGRES=1 \
      LASH_CROSS_BACKEND_CASES="${LASH_CROSS_BACKEND_PR_CASES:-4}" \
      cargo test -p lash-sim \
        --test cross_backend_store_differential \
        --locked -- --nocapture --test-threads=1
  fi
}

run_minio_conformance() {
  step "MinIO/S3 conformance"
  minio_container="lash-minio-push-gate-${LASH_GATE_WORKTREE_SLUG}"
  local port="${LASH_PUSH_GATE_MINIO_PORT:-$((port_base + 11))}"
  bash scripts/docker-pull-with-retry.sh minio/minio:RELEASE.2025-04-22T22-12-26Z
  bash scripts/docker-pull-with-retry.sh minio/mc:RELEASE.2025-04-16T18-13-26Z
  docker run -d --name "$minio_container" \
    --label "$LASH_GATE_LABEL" \
    --network "$LASH_E2E_NETWORK" \
    -e MINIO_ROOT_USER=minioadmin \
    -e MINIO_ROOT_PASSWORD=minioadmin \
    -p "127.0.0.1:${port}:9000" \
    minio/minio:RELEASE.2025-04-22T22-12-26Z server /data >/dev/null

  local endpoint="http://127.0.0.1:${port}"
  local deadline=$((SECONDS + 60))
  until docker run --rm --name "lash-minio-probe-${LASH_GATE_WORKTREE_SLUG}-$$" \
    --label "$LASH_GATE_LABEL" --network host minio/mc:RELEASE.2025-04-16T18-13-26Z \
    alias set fig831 "$endpoint" minioadmin minioadmin >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
      docker logs "$minio_container" >&2 || true
      echo "MinIO did not become ready on port ${port}" >&2
      exit 1
    fi
    sleep 1
  done
  docker run --rm --name "lash-minio-setup-${LASH_GATE_WORKTREE_SLUG}-$$" \
    --label "$LASH_GATE_LABEL" --network host --entrypoint /bin/sh \
    minio/mc:RELEASE.2025-04-16T18-13-26Z -c \
    "mc alias set fig831 '$endpoint' minioadmin minioadmin >/dev/null && mc mb --ignore-existing fig831/lash-attachments >/dev/null"

  LASH_MINIO_ENDPOINT="$endpoint" \
    LASH_REQUIRE_MINIO=1 \
    cargo test -p lash-s3-store --locked

  LASH_MINIO_ENDPOINT="$endpoint" \
    LASH_REQUIRE_MINIO=1 \
    cargo test -p lash-sim --test cross_backend_store_differential --locked \
      attachment_blob_store_differential_agrees -- --nocapture
}

configure_bindgen_headers

step "Formatting"
cargo fmt --all --check
python3 scripts/check_included_file_formatting.py
rustfmt --edition 2024 --check crates/lash-perf/src/runtime_perf/measurement/store_hardening.rs

step "Clippy"
# shellcheck disable=SC2086
cargo clippy --workspace --all-targets --locked ${ci_features} -- -D warnings

step "Restate handler panic boundary"
python3 scripts/check-restate-handler-panics.py

step "PostgreSQL payload-shape component version"
python3 scripts/check-postgres-json-carrier-coverage.py
python3 scripts/check-postgres-payload-shape-version.py

step "Core/UI boundary guard"
bash scripts/check-core-ui-boundary.sh

step "Workflow graph model guard"
bash scripts/check-workflow-graph-model.sh

step "Production file-size budget guard"
bash scripts/check-production-file-size.sh

step "Docs lint"
python3 scripts/lint_docs.py

step "Rustdoc lint"
bash scripts/check-rustdoc.sh

step "Test quarantine metadata"
python3 scripts/check_test_quarantines.py

run_release_script_tests
check_current_branch_release_notes

step "Workspace check"
# shellcheck disable=SC2086
cargo check --workspace --all-targets --locked ${ci_features}

run_runtime_feature_boundary_check
run_postgres_conformance
run_minio_conformance
run_workspace_tests

step "Workspace doctests"
# shellcheck disable=SC2086
cargo test --doc --workspace --locked ${ci_features}

step "Workflow graph example integration"
just workflow-graph-integration-verify

step "Restate e2e: agent-service"
RESTATE_ADMIN_PORT="${RESTATE_ADMIN_PORT:-$((port_base + 20))}" \
RESTATE_INGRESS_PORT="${RESTATE_INGRESS_PORT:-$((port_base + 21))}" \
RESTATE_NODE_PORT="${RESTATE_NODE_PORT:-$((port_base + 22))}" \
AGENT_SERVICE_E2E_ENDPOINT_BIND="${AGENT_SERVICE_E2E_ENDPOINT_BIND:-127.0.0.1:$((port_base + 23))}" \
AGENT_SERVICE_E2E_ENDPOINT_URL="${AGENT_SERVICE_E2E_ENDPOINT_URL:-http://127.0.0.1:$((port_base + 23))}" \
  just agent-service-restate-e2e

step "Restate e2e: agent-workbench"
AGENT_WORKBENCH_RESTATE_ADMIN_PORT="${AGENT_WORKBENCH_RESTATE_ADMIN_PORT:-$((port_base + 30))}" \
AGENT_WORKBENCH_RESTATE_INGRESS_PORT="${AGENT_WORKBENCH_RESTATE_INGRESS_PORT:-$((port_base + 31))}" \
AGENT_WORKBENCH_RESTATE_NODE_PORT="${AGENT_WORKBENCH_RESTATE_NODE_PORT:-$((port_base + 32))}" \
AGENT_WORKBENCH_E2E_ENDPOINT_BIND="${AGENT_WORKBENCH_E2E_ENDPOINT_BIND:-127.0.0.1:$((port_base + 33))}" \
AGENT_WORKBENCH_E2E_ENDPOINT_URL="${AGENT_WORKBENCH_E2E_ENDPOINT_URL:-http://127.0.0.1:$((port_base + 33))}" \
  just agent-workbench-restate-e2e

step "Restate/Postgres/MinIO workers e2e"
LASH_E2E_MINIO_PORT="${LASH_E2E_MINIO_PORT:-$((port_base + 40))}" \
  bash scripts/restate-postgres-workers-e2e.sh

step "Process operations e2e"
LASH_PROCESS_OPERATIONS_MINIO_PORT="${LASH_PROCESS_OPERATIONS_MINIO_PORT:-$((port_base + 41))}" \
LASH_PROCESS_OPERATIONS_MINIO_CONSOLE_PORT="${LASH_PROCESS_OPERATIONS_MINIO_CONSOLE_PORT:-$((port_base + 42))}" \
LASH_PROCESS_OPERATIONS_RESTATE_ADMIN_PORT="${LASH_PROCESS_OPERATIONS_RESTATE_ADMIN_PORT:-$((port_base + 43))}" \
LASH_PROCESS_OPERATIONS_RESTATE_INGRESS_PORT="${LASH_PROCESS_OPERATIONS_RESTATE_INGRESS_PORT:-$((port_base + 44))}" \
LASH_PROCESS_OPERATIONS_RESTATE_NODE_PORT="${LASH_PROCESS_OPERATIONS_RESTATE_NODE_PORT:-$((port_base + 45))}" \
LASH_PROCESS_OPERATIONS_POSTGRES_PORT="${LASH_PROCESS_OPERATIONS_POSTGRES_PORT:-$((port_base + 46))}" \
  bash scripts/process-operations-e2e.sh

step "Push gate passed"
