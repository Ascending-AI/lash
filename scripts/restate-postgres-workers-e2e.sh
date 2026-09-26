#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"

# The compose file bind-mounts each of these host binaries into its services.
e2e_bin_names=(lash-e2e-worker lash-e2e-mock-provider lash-e2e-runner lash-e2e-await-event-helper)
if [ -n "${LASH_E2E_PREBUILT_BIN_DIR:-}" ]; then
  LASH_E2E_BIN_DIR="$(cd "$LASH_E2E_PREBUILT_BIN_DIR" && pwd)"
  export LASH_E2E_BIN_DIR
  lash_gate_require_mounted_bins "$LASH_E2E_BIN_DIR" "${e2e_bin_names[@]}"
else
  export LASH_E2E_BIN_DIR="${CARGO_TARGET_DIR:-$repo/target}/release"
  # The build supplies the binaries, so a missing file here is not yet a
  # failure — but a daemon-created directory at a mount path is, and it also
  # breaks the build's own outputs. Refuse it before anything else runs.
  stale_bin_dirs=()
  for name in "${e2e_bin_names[@]}"; do
    [ -d "$LASH_E2E_BIN_DIR/$name" ] && stale_bin_dirs+=("$name")
  done
  ((${#stale_bin_dirs[@]} == 0)) \
    || lash_gate_require_mounted_bins "$LASH_E2E_BIN_DIR" "${stale_bin_dirs[@]}"
fi

compose_project="${LASH_RESTATE_WORKERS_COMPOSE_PROJECT:-lash-restate-workers-${LASH_GATE_WORKTREE_SLUG}}"
export RESTATE_AUTHORITY_ID="${RESTATE_AUTHORITY_ID:-restate-workers:${compose_project}}"
compose=(docker compose -p "$compose_project" -f "$repo/runbooks/restate-postgres-workers/docker-compose.yml")
# The S3 service's image, credentials and bucket, which the compose file reads.
# shellcheck source=scripts/ci/s3-service.sh
source "$repo/scripts/ci/s3-service.sh"
# The Postgres readiness probe: TCP only, so it answers against the image's
# final server, never its socket-only temporary init server.
# shellcheck source=scripts/ci/pg-service.sh
source "$repo/scripts/ci/pg-service.sh"
s3_port="${LASH_E2E_S3_PORT:-$((LASH_E2E_PORT_BASE + 40))}"
export LASH_E2E_S3_PORT="$s3_port"
trace_volume="${compose_project}_trace-output"
workflow_segment="${LASH_E2E_WORKFLOW_SEGMENT:-}"
case "$workflow_segment" in
  "" | 1 | 2) ;;
  *)
    echo "LASH_E2E_WORKFLOW_SEGMENT must be 1 or 2, got '$workflow_segment'" >&2
    exit 1
    ;;
esac

completed_manifest="${LASH_E2E_COMPLETED_WORKFLOW_MANIFEST:-}"
manifest_dir="${CARGO_TARGET_DIR:-$repo/target}/restate-postgres-workers-e2e-manifests"
manifest_name=""
if [ -n "$completed_manifest" ]; then
  case "$completed_manifest" in
    /*) ;;
    *) completed_manifest="$repo/$completed_manifest" ;;
  esac
  manifest_dir="$(dirname "$completed_manifest")"
  manifest_name="$(basename "$completed_manifest")"
fi
mkdir -p "$manifest_dir"
export LASH_E2E_WORKFLOW_MANIFEST_DIR="$manifest_dir"
unset LASH_E2E_COMPLETED_WORKFLOW_MANIFEST_CONTAINER
if [ -n "$manifest_name" ]; then
  rm -f "$completed_manifest" "$manifest_dir/workflow-inventory.tsv"
  export LASH_E2E_COMPLETED_WORKFLOW_MANIFEST_CONTAINER="/e2e-manifests/$manifest_name"
fi

# Acquire ownership before touching this fixed project, then scrub the entire
# previous project including its append-only trace volume. The generic
# leftover check runs after this lane-owned cleanup, so unrelated leftovers in
# the same worktree still produce a refusal rather than being removed.
lash_gate_acquire_locks restate-postgres-workers-e2e
"${compose[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
if docker volume inspect "$trace_volume" >/dev/null 2>&1; then
  echo "Pre-run cleanup left stale trace volume '$trace_volume'." >&2
  exit 1
fi
lash_gate_prepare_docker

if [ "${LASH_E2E_TRACE_SCRUB_PROBE:-0}" = "1" ]; then
  docker volume create "$trace_volume" >/dev/null
  trace_probe_contents="$(docker run --rm -v "$trace_volume:/e2e-traces" postgres:16-alpine \
    sh -c 'find /e2e-traces -mindepth 1 -print -quit')"
  docker volume rm "$trace_volume" >/dev/null
  if [ -n "$trace_probe_contents" ]; then
    echo "Fresh trace volume '$trace_volume' was not empty." >&2
    exit 1
  fi
  echo "trace scrub regression probe passed: stale evidence removed; fresh assertion input empty"
  exit 0
fi

test_output="$(mktemp "${TMPDIR:-/tmp}/lash-restate-postgres-workers-e2e-${LASH_GATE_WORKTREE_SLUG}.XXXXXX")"

# Binaries are built on the host (sharing the normal cargo cache) and
# bind-mounted into the compose services; see docker-compose.yml for the
# glibc compatibility note.
if [ -z "${LASH_E2E_PREBUILT_BIN_DIR:-}" ]; then
  cargo build --locked --release -p lash-restate-postgres-workers-e2e --bins
fi
# Re-check before the first compose call that could create a missing bind
# source (config --images, up, run); the earlier `down` creates nothing.
lash_gate_require_mounted_bins "$LASH_E2E_BIN_DIR" "${e2e_bin_names[@]}"
if [ -n "$manifest_name" ]; then
  "$LASH_E2E_BIN_DIR/lash-e2e-runner" --workflow-inventory \
    > "$manifest_dir/workflow-inventory.tsv"
fi

cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then
    echo "distributed workers E2E failed with status $status; dumping compose diagnostics" >&2
    "${compose[@]}" ps -a >&2 || true
    while IFS= read -r container_id; do
      [ -n "$container_id" ] || continue
      service="$(docker inspect -f '{{index .Config.Labels "com.docker.compose.service"}}' "$container_id" 2>/dev/null || echo unknown)"
      echo "===== service=$service container=$container_id logs =====" >&2
      docker logs --timestamps "$container_id" >&2 || true
      echo "===== service=$service container=$container_id processes =====" >&2
      docker top "$container_id" -eo pid,ppid,stat,wchan:28,etime,cmd >&2 || true
    done < <("${compose[@]}" ps -a -q 2>/dev/null || true)
  fi
  if [ "${LASH_E2E_KEEP:-0}" != "1" ]; then
    "${compose[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
    lash_gate_cleanup
  fi
  rm -f "$test_output"
  exit "$status"
}
trap cleanup EXIT

# Several services share the host-binary runtime image. Pull it once with
# retries so a transient Docker Hub HEAD error doesn't fail compose startup.
for image in $("${compose[@]}" config --images); do bash scripts/docker-pull-with-retry.sh "$image"; done
# Worker open never provisions the schema (FIG-3797): bringing Postgres up
# alone first lets the harness apply the committed schema artifact before any
# worker starts — the same step `lash migrate` performs in a deployment.
"${compose[@]}" up -d postgres
# Wait for the final server, not the image's temporary init server: that one
# listens on the unix socket alone, so only a TCP answer counts. A unix-socket
# pg_isready can pass against the temporary server and release the schema
# apply below into its shutdown ("the database system is shutting down").
if ! lash_pg_wait postgres 90 "${compose[@]}" exec -T postgres; then
  "${compose[@]}" logs postgres >&2 || true
  exit 1
fi
"${compose[@]}" exec -T postgres psql -U lash -d lash -v ON_ERROR_STOP=1 -q \
  < "$repo/crates/lash-postgres-store/schema.sql"
"${compose[@]}" up -d s3 restate mock-provider worker-a worker-b worker-proxy
lash_s3_wait "$("${compose[@]}" ps -q s3)" 60

if [ "$workflow_segment" != "2" ] && [ "${LASH_E2E_TURN_CONTROL_ONLY:-0}" != "1" ]; then
  # LASH_REQUIRE_S3 makes the live S3 laws run rather than skip.
  mapfile -t s3_test_env < <(lash_s3_test_env "$s3_port")
  env "${s3_test_env[@]}" \
    LASH_S3_PREFIX="conformance/restate-postgres-workers-${LASH_GATE_WORKTREE_SLUG}-$$" \
    cargo test --locked -p lash-internal-s3-store -- --nocapture \
    2>&1 | tee "$test_output"
fi

"${compose[@]}" --profile runner run --rm runner 2>&1 | tee -a "$test_output" &
runner_job=$!

if [ "${LASH_E2E_WAKE_RCA_ONLY:-0}" = "1" ] \
  || [ "${LASH_E2E_TURN_CONTROL_ONLY:-0}" = "1" ] \
  || [ "$workflow_segment" = "1" ]; then
  wait "$runner_job"
  if [ -n "$completed_manifest" ] && [ ! -s "$completed_manifest" ]; then
    echo "runner did not write completed-workflow manifest '$completed_manifest'" >&2
    exit 1
  fi
  "${compose[@]}" logs --no-color 2>&1 | tee -a "$test_output"
  if grep -Fn 'panicked at' "$test_output" >&2; then
    echo "panic gate: FAILED (a Rust panic marker found in Restate/Postgres workers E2E output)" >&2
    exit 1
  fi
  echo "panic gate: clean (no Rust panic markers in Restate/Postgres workers E2E output)"
  exit 0
fi

# The cold-process await-event vectors each wait for Restate to report a
# genuinely suspended invocation before killing their helper, and the
# frame-switch crash waits out the killed worker's session lease before
# Restate's redelivery can admit. Leave enough budget for those gates plus
# the existing engine-restart setup.
deadline=$((SECONDS + 600))
until signal_ready="$(
  "${compose[@]}" exec -T postgres \
    psql -U lash -d lash -Atqc \
    "SELECT EXISTS(SELECT 1 FROM lash_e2e_harness_signals WHERE signal_name = 'engine-restart-ready')" \
    2>/dev/null || true
)" && [[ "$signal_ready" = "t" ]]; do
  if ! kill -0 "$runner_job" >/dev/null 2>&1; then
    wait "$runner_job"
    echo "runner exited before the engine-restart ready gate" >&2
    exit 1
  fi
  if ((SECONDS >= deadline)); then
    echo "timed out waiting for the engine-restart ready gate" >&2
    exit 1
  fi
  sleep 1
done

echo "engine-restart harness: parked turn observed; stopping only Restate"
for service in worker-a worker-b worker-proxy mock-provider; do
  [[ "$("${compose[@]}" ps --status running -q "$service")" ]] \
    || { echo "$service was not running before Restate restart" >&2; exit 1; }
done
"${compose[@]}" stop restate
[[ -z "$("${compose[@]}" ps --status running -q restate)" ]] \
  || { echo "Restate remained running after stop" >&2; exit 1; }
"${compose[@]}" start restate
for service in worker-a worker-b worker-proxy mock-provider; do
  [[ "$("${compose[@]}" ps --status running -q "$service")" ]] \
    || { echo "$service stopped during Restate restart" >&2; exit 1; }
done
"${compose[@]}" exec -T postgres \
  psql -U lash -d lash -v ON_ERROR_STOP=1 -c \
  "INSERT INTO lash_e2e_harness_signals (signal_name, created_at_ms)
   VALUES ('engine-restart-complete', (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT)
   ON CONFLICT (signal_name) DO UPDATE SET created_at_ms = EXCLUDED.created_at_ms" \
  >/dev/null
echo "engine-restart harness: Restate started; workers remained running"

wait "$runner_job"
if [ -n "$completed_manifest" ] && [ ! -s "$completed_manifest" ]; then
  echo "runner did not write completed-workflow manifest '$completed_manifest'" >&2
  exit 1
fi
"${compose[@]}" logs --no-color 2>&1 | tee -a "$test_output"
if grep -Fn 'panicked at' "$test_output" >&2; then
  echo "panic gate: FAILED (a Rust panic marker found in Restate/Postgres workers E2E output)" >&2
  exit 1
fi
echo "panic gate: clean (no Rust panic markers in Restate/Postgres workers E2E output)"
