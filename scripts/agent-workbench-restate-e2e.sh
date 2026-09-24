#!/usr/bin/env bash
# The agent-workbench live Restate suite.
#
# `scripts/ci/restate_suite.py` builds the workbench test binary on the shared
# pool and runs its `live_restate_` laws beside pinned `restate-server`s, one
# law per process over several shards (the suite is registered in
# `scripts/restate-suites.toml`). The Postgres-backed laws share one
# `scripts/ci/with-service.sh pg16` server, one slot database per shard. This
# driver owns what the laws leave behind: the tokenized fixture data
# directories they record, the recovery children they spawn and the endpoints
# they bind, and it runs the SQLite memory-store companion law afterwards.
set -euo pipefail
umask 077

agent_workbench_port_open() {
  local host="$1" port="$2"
  timeout 2 bash -c "echo >/dev/tcp/$host/$port" >/dev/null 2>&1
}

# Reap what the laws left behind. Every Restate server has stopped by the
# time this runs (the runner stops its shards before it returns), so a fixture
# data directory whose marker carries this run's token is ours to remove.
agent_workbench_cleanup() {
  local run_status="$1" cleanup_failed=0
  local addr host port pid canonical_path temp_root retained_path marker
  set +e

  while IFS= read -r pid; do
    [ -n "$pid" ] || continue
    if [ -e "/proc/$pid" ] && tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null \
      | grep -Fxq 'AGENT_WORKBENCH_RECOVERY_E2E_CHILD=1'; then
      kill -KILL "$pid" >/dev/null 2>&1 || true
      for _attempt in $(seq 1 50); do
        [ ! -e "/proc/$pid" ] && break
        sleep 0.1
      done
    fi
    if [ -e "/proc/$pid" ]; then
      printf 'owned_child_reaped=false pid=%s\n' "$pid" >>"$cleanup_log"
      cleanup_failed=1
    else
      printf 'owned_child_reaped=true pid=%s\n' "$pid" >>"$cleanup_log"
    fi
  done < <(sort -u "$child_manifest")

  while IFS= read -r addr; do
    [ -n "$addr" ] || continue
    host="${addr%:*}"
    port="${addr##*:}"
    if agent_workbench_port_open "$host" "$port"; then
      printf 'owned_endpoint_closed=false addr=%s\n' "$addr" >>"$cleanup_log"
      cleanup_failed=1
    else
      printf 'owned_endpoint_closed=true addr=%s\n' "$addr" >>"$cleanup_log"
    fi
  done < <(sort -u "$endpoint_manifest")

  temp_root="$(realpath -e "${TMPDIR:-/tmp}")"
  while IFS= read -r retained_path; do
    [ -n "$retained_path" ] || continue
    if [ ! -e "$retained_path" ]; then
      echo 'owned_data_already_removed=true' >>"$cleanup_log"
      continue
    fi
    canonical_path="$(realpath -e "$retained_path")"
    marker="$canonical_path/.agent-workbench-fixture-owner"
    if [ "$(dirname "$canonical_path")" != "$temp_root" ] \
      || [ "$(cat "$marker" 2>/dev/null)" != "$cleanup_token" ]; then
      echo 'owned_data_preimage_valid=false' >>"$cleanup_log"
      cleanup_failed=1
      continue
    fi
    rm -rf -- "$canonical_path"
    if [ -e "$canonical_path" ]; then
      echo 'owned_data_removed=false' >>"$cleanup_log"
      cleanup_failed=1
    else
      echo 'owned_data_removed=true' >>"$cleanup_log"
    fi
  done < <(sort -u "$data_manifest")

  chmod -R go-rwx "$artifact_dir" 2>/dev/null || true
  if ((cleanup_failed != 0)); then
    echo "agent-workbench Restate E2E cleanup failed; artifacts retained at $artifact_dir" >&2
    return 97
  fi
  if ((run_status != 0)); then
    echo "agent-workbench Restate E2E failed; artifacts retained at $artifact_dir" >&2
    return "$run_status"
  fi
  if [ "${AGENT_WORKBENCH_E2E_KEEP_ARTIFACTS:-0}" != 1 ] && [ -z "${AGENT_WORKBENCH_E2E_ARTIFACT_DIR:-}" ]; then
    rm -rf -- "$artifact_dir"
  fi
}

if [ "${BASH_SOURCE[0]}" != "$0" ]; then
  return 0
fi

repo="$(cd "$(dirname "$0")/.." && pwd -P)"
cd "$repo"
# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"
lash_gate_acquire_locks agent-workbench-restate-e2e

if [ -n "${AGENT_WORKBENCH_E2E_ARTIFACT_DIR:-}" ]; then
  artifact_dir="$AGENT_WORKBENCH_E2E_ARTIFACT_DIR"
  case "$artifact_dir" in
    /*) ;;
    *) artifact_dir="$repo/$artifact_dir" ;;
  esac
  mkdir -p "$artifact_dir"
  chmod 700 "$artifact_dir"
else
  artifact_dir="$(mktemp -d "${TMPDIR:-/tmp}/lash-agent-workbench-restate-e2e-${LASH_GATE_WORKTREE_SLUG}.XXXXXX")"
fi
test_output="$artifact_dir/test.log"
cleanup_log="$artifact_dir/cleanup.log"
data_manifest="$artifact_dir/fixture-data.manifest"
child_manifest="$artifact_dir/fixture-children.manifest"
endpoint_manifest="$artifact_dir/fixture-endpoints.manifest"
cleanup_token="$(cat /proc/sys/kernel/random/uuid)"
: >"$data_manifest"
: >"$child_manifest"
: >"$endpoint_manifest"
: >"$cleanup_log"
export AGENT_WORKBENCH_FIXTURE_DATA_MANIFEST="$data_manifest"
export AGENT_WORKBENCH_FIXTURE_CHILD_MANIFEST="$child_manifest"
export AGENT_WORKBENCH_FIXTURE_ENDPOINT_MANIFEST="$endpoint_manifest"
export AGENT_WORKBENCH_FIXTURE_CLEANUP_TOKEN="$cleanup_token"
# Each shard's Restate trust domain is `agent-workbench-e2e:<run>:<shard>`.
AGENT_WORKBENCH_E2E_RUN_ID="${LASH_GATE_WORKTREE_SLUG}-$(date +%s)-$$"
export AGENT_WORKBENCH_E2E_RUN_ID
export AGENT_WORKBENCH_E2E_SUITE_ARTIFACTS="$artifact_dir"
ulimit -c 0

cleanup_trap() {
  local run_status="$?" cleanup_status
  trap - EXIT
  agent_workbench_cleanup "$run_status"
  cleanup_status="$?"
  exit "$cleanup_status"
}
trap cleanup_trap EXIT

# The companion law's binary builds first, so a compile error fails before
# any service starts.
companion_executable="$(python3 "$repo/scripts/ci/restate_suite.py" build \
  //crates/lash-sqlite-store:conformance_memory__test)"

set +e
# shellcheck disable=SC2016 # expanded by the inner shell, after with-service exports the URL
bash "$repo/scripts/ci/with-service.sh" pg16 -- bash -c '
  set -euo pipefail
  export AGENT_WORKBENCH_E2E_POSTGRES_BASE_URL="${LASH_POSTGRES_DATABASE_URL%/*}"
  exec python3 scripts/ci/restate_suite.py suite agent-workbench \
    --leg "${AGENT_WORKBENCH_E2E_LEG:-live}" \
    --artifacts "$AGENT_WORKBENCH_E2E_SUITE_ARTIFACTS"
' 2>&1 | tee "$test_output"
test_status="${PIPESTATUS[0]}"
set -e
if ((test_status != 0)); then
  exit "$test_status"
fi

companion_output="$artifact_dir/companion.log"
(cd crates/lash-sqlite-store && "$companion_executable" \
  turn_input_claims_supersede_across_session_lease_generations \
  --test-threads=1) 2>&1 | tee "$companion_output"
if ! grep -Fq 'test result: ok. 1 passed; 0 failed' "$companion_output"; then
  echo 'companion gate: FAILED (expected one passing turn-input lease-generation test)' >&2
  exit 1
fi
if grep -Fn 'panicked at' "$companion_output" >&2; then
  echo "panic gate: FAILED (a Rust panic marker found in the companion law's output)" >&2
  exit 1
fi
echo "panic gate: clean (the suite's laws are panic-gated one by one by the runner)"
