#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"
lash_gate_acquire restate-postgres-workers-e2e

compose_project="${LASH_RESTATE_WORKERS_COMPOSE_PROJECT:-lash-restate-workers-${LASH_GATE_WORKTREE_SLUG}}"
compose=(docker compose -p "$compose_project" -f "$repo/runbooks/restate-postgres-workers/docker-compose.yml")
minio_port="${LASH_E2E_MINIO_PORT:-$((LASH_E2E_PORT_BASE + 40))}"
export LASH_E2E_MINIO_PORT="$minio_port"
test_output="$(mktemp "${TMPDIR:-/tmp}/lash-restate-postgres-workers-e2e-${LASH_GATE_WORKTREE_SLUG}.XXXXXX")"

# Binaries are built on the host (sharing the normal cargo cache) and
# bind-mounted into the compose services; see docker-compose.yml for the
# glibc compatibility note.
cargo build --locked --release -p lash-restate-postgres-workers-e2e --bins
export LASH_E2E_BIN_DIR="${CARGO_TARGET_DIR:-$repo/target}/release"

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
  fi
  rm -f "$test_output"
  exit "$status"
}
trap cleanup EXIT

# Several services share the host-binary runtime image. Pull it once with
# retries so a transient Docker Hub HEAD error doesn't fail compose startup.
bash scripts/docker-pull-with-retry.sh ubuntu:24.04
"${compose[@]}" up -d postgres minio minio-init restate mock-provider worker-a worker-b worker-proxy
deadline=$((SECONDS + 60))
while true; do
  minio_init_id="$("${compose[@]}" ps -a -q minio-init)"
  if [ -n "$minio_init_id" ]; then
    minio_init_status="$(docker inspect -f '{{.State.Status}}' "$minio_init_id")"
    if [ "$minio_init_status" = "exited" ]; then
      minio_init_exit="$(docker inspect -f '{{.State.ExitCode}}' "$minio_init_id")"
      if [ "$minio_init_exit" != "0" ]; then
        echo "minio-init exited with status $minio_init_exit" >&2
        exit 1
      fi
      break
    fi
  fi
  if ((SECONDS >= deadline)); then
    echo "minio-init did not complete before timeout" >&2
    exit 1
  fi
  sleep 1
done

LASH_MINIO_ENDPOINT="http://127.0.0.1:$minio_port" \
LASH_MINIO_BUCKET="lash-attachments" \
LASH_MINIO_REGION="us-east-1" \
LASH_MINIO_ACCESS_KEY="minioadmin" \
LASH_MINIO_SECRET_KEY="minioadmin" \
LASH_MINIO_PREFIX="conformance/restate-postgres-workers-${LASH_GATE_WORKTREE_SLUG}-$$" \
  cargo test --locked -p lash-s3-store -- --nocapture \
  2>&1 | tee "$test_output"

"${compose[@]}" --profile runner run --rm runner 2>&1 | tee -a "$test_output" &
runner_job=$!

if [ "${LASH_E2E_WAKE_RCA_ONLY:-0}" = "1" ]; then
  wait "$runner_job"
  "${compose[@]}" logs --no-color 2>&1 | tee -a "$test_output"
  if grep -Fn 'panicked at' "$test_output" >&2; then
    echo "panic gate: FAILED ('panicked at' found in Restate/Postgres workers E2E output)" >&2
    exit 1
  fi
  echo "panic gate: clean (no 'panicked at' lines in Restate/Postgres workers E2E output)"
  exit 0
fi

# The cold-process await-event vectors each wait for Restate to report a
# genuinely suspended invocation before killing their helper. Leave enough
# budget for those gates plus the existing engine-restart setup.
deadline=$((SECONDS + 480))
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
"${compose[@]}" logs --no-color 2>&1 | tee -a "$test_output"
if grep -Fn 'panicked at' "$test_output" >&2; then
  echo "panic gate: FAILED ('panicked at' found in Restate/Postgres workers E2E output)" >&2
  exit 1
fi
echo "panic gate: clean (no 'panicked at' lines in Restate/Postgres workers E2E output)"
