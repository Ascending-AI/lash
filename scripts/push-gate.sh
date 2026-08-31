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
  lash_gate_cleanup
}
trap cleanup EXIT

step() {
  printf '\n==> %s\n' "$*"
}

# Build-heavy legs run through `heavy-slot` when the developer box provides it.
#
# The gate lock this script already holds serialises push gates *within* one
# worktree. It says nothing about the rest of the machine, and a box running
# several agent lanes at once has several of these gates compiling the whole
# workspace simultaneously — each sized from `nproc`, each evicting the others'
# pages. `heavy-slot` is a box-wide semaphore (a small number of `flock`ed
# slots) that caps how many compile-shaped gates are resident at once; it waits
# for a slot rather than failing, and it is re-entrant, so wrapping a leg that
# already runs inside a slot is a no-op rather than a deadlock.
#
# It is feature-detected on purpose. CI runners have one job per runner and no
# such tool, and a gate that only works on one developer's machine is a broken
# gate: with `heavy-slot` absent, `heavy` expands to nothing and every leg below
# runs exactly as it did before.
heavy_slot_cmd=()
if command -v heavy-slot >/dev/null 2>&1; then
  heavy_slot_cmd=(heavy-slot)
fi

heavy() {
  "${heavy_slot_cmd[@]}" "$@"
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

# The self-tests of the gates this script and the prek hooks actually run.
# CI's Repository-gates job runs a longer list, and the remainder there covers
# gates that only exist in CI (see the omissions block below) — a self-test
# whose gate nobody runs locally proves nothing about a local push.
#
# `test_gate_scope.py` is deliberately absent: it guards the decider that
# decides whether this leg runs at all, so it is dispatched unscoped near the
# top of the script instead.
run_release_script_tests() {
  step "Repository script tests"
  python3 scripts/test_check_facade_external_types.py
  python3 scripts/test_check_facade_only_examples.py
  python3 scripts/test_check_durable_read_fixture_version.py
  python3 scripts/test_check_judged_build_geometry.py
  python3 scripts/test_check_postgres_json_carrier_coverage.py
  python3 scripts/test_check_postgres_payload_shape_version.py
  python3 scripts/test_check_service_gate_pinning.py
  python3 scripts/test_check_transcript_diff.py
  python3 scripts/test_check_version_bump_fixtures.py
  python3 scripts/test_release_version.py
  python3 scripts/test_publish_workspace.py
}

# Path-scoped gate selection. `scripts/gate_scope.py` maps this branch's
# touched paths onto gate families and only ever answers `skip` for a family no
# touched path can reach; every ambiguity -- a shared input, an unrecognised
# path, an empty path set, its own failure -- answers `run`. Its table is
# printed verbatim below, because a skipped leg that leaves no trace in the
# gate log is a leg a reviewer cannot audit.
# Resolve the base ref this branch's touched paths are computed against:
# the configured PR base when one is known, falling back to main.
resolve_gate_base() {
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
      printf '%s
' "$candidate"
      return
    fi
  done

  echo "Cannot resolve gate base ref: $configured_base" >&2
  return 1
}

gate_scope_apply() {
  step "Gate scope"
  GATE_RUN_RUST_COMPILE=1
  GATE_RUN_SCRIPTS=1
  GATE_RUN_REGISTRY=1
  GATE_RUN_WORKFLOWS=1
  GATE_SCOPE_CLASSIFICATION="run-everything"

  local base_ref env_output
  if ! base_ref="$(resolve_gate_base)"; then
    echo "gate scope: base ref unresolved; running every gate family"
    return
  fi
  # Two invocations of the same pure classification: the first is the audit
  # trail a human reads, the second is the machine form this shell consumes.
  if ! python3 scripts/gate_scope.py --base "$base_ref"; then
    echo "gate scope: classification failed; running every gate family"
    return
  fi
  if ! env_output="$(python3 scripts/gate_scope.py --base "$base_ref" --format env)"; then
    echo "gate scope: env classification failed; running every gate family"
    return
  fi
  eval "$env_output"
}

gate_family_runs() {
  local variable="GATE_RUN_$1"
  [ "${!variable:-1}" = "1" ]
}

# scoped <FAMILY> <label> <command...>
scoped() {
  local family="$1" label="$2"
  shift 2
  if gate_family_runs "$family"; then
    "$@"
    return
  fi
  printf '\n==> skipped: %s (gate scope: no touched path affects %s)\n' \
    "$label" "$family"
}

run_formatting_gates() {
  step "Formatting"
  cargo fmt --all --check
  python3 scripts/check_included_file_formatting.py
  rustfmt --edition 2024 --check crates/lash-perf/src/runtime_perf/measurement/store_hardening.rs
}

run_clippy_gate() {
  step "Clippy"
  # shellcheck disable=SC2086
  heavy cargo clippy --workspace --all-targets --locked ${ci_features} -- -D warnings
}

# Guards whose inputs are Rust sources, the crate-adjacent schema artefacts
# beside them, or the recipes that build them.
run_rust_source_guards() {
  step "Facade-only example imports"
  python3 scripts/check_facade_only_examples.py

  step "Facade external types"
  heavy python3 scripts/check_facade_external_types.py

  step "Restate handler panic boundary"
  python3 scripts/check-restate-handler-panics.py

  step "PostgreSQL payload-shape component version"
  python3 scripts/check-postgres-json-carrier-coverage.py
  python3 scripts/check-postgres-payload-shape-version.py

  step "Core/UI boundary guard"
  bash scripts/check-core-ui-boundary.sh

  step "Substrate boundary guard"
  bash scripts/check-substrate-boundary.sh

  step "Workflow graph model guard"
  bash scripts/check-workflow-graph-model.sh

  step "Test quarantine metadata"
  python3 scripts/check_test_quarantines.py

  step "Judged build geometry"
  python3 scripts/check_judged_build_geometry.py

  step "Production file-size budget guard"
  bash scripts/check-production-file-size.sh
}

run_workflow_gates() {
  step "Service gate pinning"
  python3 scripts/check_service_gate_pinning.py
}

run_workspace_check() {
  step "Workspace check"
  # shellcheck disable=SC2086
  heavy cargo check --workspace --all-targets --locked ${ci_features}
}

run_workspace_doctests() {
  step "Workspace doctests"
  # shellcheck disable=SC2086
  heavy cargo test --doc --workspace --locked ${ci_features}
}

run_workflow_graph_integration() {
  step "Workflow graph example integration"
  just workflow-graph-integration-verify
}

run_e2e_suite() {
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

  step "Restate e2e: effect-group conformance"
  EFFECT_GROUP_RESTATE_ADMIN_PORT="${EFFECT_GROUP_RESTATE_ADMIN_PORT:-$((port_base + 35))}" \
  EFFECT_GROUP_RESTATE_INGRESS_PORT="${EFFECT_GROUP_RESTATE_INGRESS_PORT:-$((port_base + 36))}" \
  EFFECT_GROUP_RESTATE_NODE_PORT="${EFFECT_GROUP_RESTATE_NODE_PORT:-$((port_base + 37))}" \
  EG_RESTATE_ENDPOINT_BIND="${EG_RESTATE_ENDPOINT_BIND:-127.0.0.1:$((port_base + 38))}" \
  EG_RESTATE_ENDPOINT_URL="${EG_RESTATE_ENDPOINT_URL:-http://127.0.0.1:$((port_base + 38))}" \
  EG0_RESTATE_ENDPOINT_BIND="${EG0_RESTATE_ENDPOINT_BIND:-127.0.0.1:$((port_base + 39))}" \
  EG0_RESTATE_ENDPOINT_URL="${EG0_RESTATE_ENDPOINT_URL:-http://127.0.0.1:$((port_base + 39))}" \
    just effect-group-conformance-e2e

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
}

run_runtime_feature_boundary_check() {
  step "lash-runtime feature boundary"
  cargo check -p lash-runtime --no-default-features --locked
  cargo check -p lash-runtime --no-default-features --features testing --locked
  cargo test -p lash-runtime --no-default-features --locked
  count=$(cargo test -p lash-runtime --no-default-features --locked --lib -- --list | grep -c ': test$')
  [ "$count" -ge 130 ] || { echo "default-build lash-runtime tests regressed: $count"; exit 1; }

  if cargo tree -p lash-runtime -e normal --no-default-features --locked \
    | grep -E 'lash-internal-protocol-rlm|lash-internal-lashlang-runtime|lash-internal-lashlang'; then
    echo "default-off lash-runtime pulled RLM/Lashlang dependencies" >&2
    exit 1
  fi

  if cargo tree -p lash-runtime -e normal --locked \
    | grep -E 'lash-internal-protocol-rlm|lash-internal-lashlang-runtime|lash-internal-lashlang'; then
    echo "default lash-runtime pulled RLM/Lashlang dependencies" >&2
    exit 1
  fi

  if cargo tree -p lash-runtime -e normal --no-default-features --features testing --locked \
    | grep -E 'lash-internal-protocol-rlm|lash-internal-lashlang-runtime|lash-internal-lashlang'; then
    echo "testing-only lash-runtime pulled RLM/Lashlang dependencies" >&2
    exit 1
  fi
}

run_workspace_tests() {
  step "Workspace tests"
  if cargo nextest --version >/dev/null 2>&1; then
    # shellcheck disable=SC2086
    heavy env -u LASH_POSTGRES_DATABASE_URL -u LASH_REQUIRE_POSTGRES \
      cargo nextest run --workspace --locked ${ci_features}
  else
    echo "cargo-nextest is not installed; falling back to cargo test for local push gate." >&2
    # shellcheck disable=SC2086
    heavy env -u LASH_POSTGRES_DATABASE_URL -u LASH_REQUIRE_POSTGRES \
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
    cargo test -p lash-internal-postgres-store --locked

  step "Cross-backend store differential"
  if cargo nextest --version >/dev/null 2>&1; then
    LASH_POSTGRES_DATABASE_URL="$database_url" \
      LASH_REQUIRE_POSTGRES=1 \
      LASH_CROSS_BACKEND_CASES="${LASH_CROSS_BACKEND_PR_CASES:-4}" \
      cargo nextest run -p lash-sim \
        --test cross_backend_store_differential \
        --locked -j1 --no-capture --run-ignored all
  else
    LASH_POSTGRES_DATABASE_URL="$database_url" \
      LASH_REQUIRE_POSTGRES=1 \
      LASH_CROSS_BACKEND_CASES="${LASH_CROSS_BACKEND_PR_CASES:-4}" \
      cargo test -p lash-sim \
        --test cross_backend_store_differential \
        --locked -- --nocapture --test-threads=1 --include-ignored
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
    cargo test -p lash-internal-s3-store --locked

  LASH_MINIO_ENDPOINT="$endpoint" \
    LASH_REQUIRE_MINIO=1 \
    cargo test -p lash-sim --test cross_backend_store_differential --locked \
      attachment_blob_store_differential_agrees -- --nocapture --include-ignored
}

# Gates CI runs that this script deliberately does not, and why. Between this
# file, `.pre-commit-config.yaml`, and `.github/workflows/ci.yml` there are
# three readable lists and no dispatcher; this block is what keeps the third
# list from silently drifting away from the first two. A CI gate that appears
# in none of the three places is an omission, not a decision.
#
#   scripts/check_version_bumps.py --base <merge-base>
#     Base-relative, and the guarantee is CI's rather than the tree's: the
#     check assumes it is looking at a current merge ref whose target-branch
#     parent is the latest protected-branch tip (see the script's docstring).
#     A local merge-base goes stale the moment main moves, so a local answer
#     is not the answer the merge gets.
#
#   scripts/check-transcript-diff.py --enforce
#     Run above in `--advisory` mode. The enforcing form asks about the pull
#     request body, which does not exist yet at push time; see the comment at
#     the invocation.
#
#   scripts/check-durable-read-fixture-version.py,
#   scripts/check_version_bump_fixtures.py,
#   scripts/lint_orchestrating_tools.py, actionlint
#     Owned by the prek hooks in `.pre-commit-config.yaml`: file-scoped, run
#     on every commit that touches their inputs, and not worth a second full
#     pass here. The two that have self-tests still run them above, because a
#     hook that has stopped working is invisible from the hook itself.
#
#   scripts/api_surface.py check
#     Builds rustdoc JSON for the facade crate — its own compile of the
#     dependency graph, and its own full-profile CI job for that reason.
#
#   scripts/ci-stack-budget.sh, scripts/confidence-gate.sh fast shards,
#   scripts/profile_runtime.py, scripts/profile_lashlang.py,
#   scripts/graceful-drain-e2e.sh, scripts/request-abandon-e2e.sh,
#   cargo clippy -p slack-clone --features e2e, the Postgres 14/18 majors,
#   the browser E2E leg, and the package feature checks
#     Breadth this gate trades for wall-clock. Each resolves a feature graph
#     or a container stack of its own — a `-p` build is a different
#     feature-unified graph, not a slice of the workspace one — and CI shards
#     them across jobs that this script would have to run in series. What it
#     runs instead is the workspace suite, the Postgres-16 and MinIO store
#     lanes, and the Restate E2Es above.
#
#   scripts/test-worktree-gate-env.sh
#     Exercises the gate lock this script is currently holding, so running it
#     from inside the gate would test a state no push ever has.

configure_bindgen_headers

# Unscoped by construction, and deliberately ahead of the classification it
# validates: this is the self-test of the code that decides what the rest of
# this script runs. Dispatching it under `scoped SCRIPTS` would mean that on
# every branch which touches neither `scripts/**` nor `.github/**` nor a
# manifest, the gate guarding the gate-decider executes nowhere locally. A
# decider nobody tests is a decider nobody can audit, so it runs on every push
# and its failure aborts before a single skip decision is taken.
step "Gate scope self-test"
python3 scripts/test_gate_scope.py

gate_scope_apply

scoped RUST_COMPILE "Formatting" run_formatting_gates
scoped RUST_COMPILE "Clippy" run_clippy_gate
scoped RUST_COMPILE "Rust source guards" run_rust_source_guards
scoped WORKFLOWS "Workflow guards" run_workflow_gates

# The two gates below are commit-scoped, not path-scoped: they read the commit
# range's messages and the semantics of the diff rather than the set of paths
# it touches, so every non-empty change affects them and no classification can
# narrow them away.
step "Durable transcript classification"
# CI runs this gate as `--enforce` (ci.yml, Lint). Enforcement is a question
# about the pull request, not about the tree: the `Transcript:` justification
# lives in the PR body, which the gate reads from the event payload or from the
# API with GITHUB_TOKEN. At push time the pull request usually does not exist
# yet and neither source is present, so `--enforce` here would fail every
# justified change and train people to skip the gate. Advisory prints the same
# classification — the part that is actionable before pushing — and CI still
# decides.
python3 scripts/check-transcript-diff.py --advisory

scoped SCRIPTS "Repository script tests" run_release_script_tests

scoped RUST_COMPILE "Workspace check" run_workspace_check
scoped RUST_COMPILE "lash-runtime feature boundary" run_runtime_feature_boundary_check
scoped RUST_COMPILE "Postgres conformance" run_postgres_conformance
scoped RUST_COMPILE "MinIO/S3 conformance" run_minio_conformance
scoped RUST_COMPILE "Workspace tests" run_workspace_tests
scoped RUST_COMPILE "Workspace doctests" run_workspace_doctests
scoped RUST_COMPILE "Workflow graph example integration" run_workflow_graph_integration
scoped RUST_COMPILE "Restate and process e2e suite" run_e2e_suite

step "Push gate passed (scope: ${GATE_SCOPE_CLASSIFICATION})"
