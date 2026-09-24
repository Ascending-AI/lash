#!/usr/bin/env bash
# Run a command against a throwaway service container.
#
# This is the single owner of "what a service-backed suite needs to talk to":
# the image, the container port, the container environment, the readiness
# probe, the one-time setup, and the environment variable NAMES the tests read.
# Both callers go through it:
#
#   * CI's `postgres-store` and `s3-store` jobs, which wrap each
#     `scripts/ci/store-tests.sh <suite>` step in it, and
#   * a developer on this box, who runs the identical line by hand.
#
# There is therefore one code path and one copy of the facts. The container is
# published on a free ephemeral port of the loopback interface, never a fixed
# 5432 or 9000, so two lanes on one machine cannot end up sharing a database;
# every connection string interpolates the chosen port. The container is
# removed on success, on failure, and on Ctrl-C alike.
#
# Usage:
#   scripts/ci/with-service.sh                       # list services and exit
#   scripts/ci/with-service.sh --list
#   scripts/ci/with-service.sh <service> -- <command...>
#   scripts/ci/with-service.sh all -- <command...>   # each service in turn
#
# Example:
#   scripts/ci/with-service.sh pg16 -- bash scripts/ci/store-tests.sh pg-store
set -euo pipefail

readonly PROGRAM="scripts/ci/with-service.sh"

# ---------------------------------------------------------------------------
# The service table. Image tags are declared here and nowhere else; ci.yml no
# longer starts its own containers, so there is no second copy to drift from.
# scripts/test_with_service.py holds this table to ci.yml and to store-tests.sh.
# ---------------------------------------------------------------------------
# The S3 service's facts (Garage, pinned by digest) live in s3-service.sh,
# which the runbooks and gates share.
# shellcheck source=scripts/ci/s3-service.sh
source "$(dirname "${BASH_SOURCE[0]}")/s3-service.sh"
readonly SERVICES=(pg14 pg16 pg18 s3)
# Databases a PostgreSQL service carries beside the default `lash`, one per
# test that `scripts/ci/store-tests.sh pg-store` runs at once. Each is named
# `lash_slot_<index>`; `tools/bazel/postgres_slot_runner.sh` hands one to each
# test action so the sharded suites never share tables (FIG-3572).
readonly POSTGRES_SLOT_COUNT=4

service_description() {
  case "$1" in
    pg14) echo "PostgreSQL 14 compatibility lane (catalog artifact + version stamp)" ;;
    pg16) echo "PostgreSQL 16 primary lane (conformance, pool-wait, agent scenario, cross-backend)" ;;
    pg18) echo "PostgreSQL 18 compatibility lane (catalog artifact + version stamp)" ;;
    s3) echo "Garage S3 object store (S3 conformance + attachment blob-store differential)" ;;
  esac
}

service_image() {
  case "$1" in
    pg14) echo "postgres:14-alpine" ;;
    pg16) echo "postgres:16-alpine" ;;
    pg18) echo "postgres:18-alpine" ;;
    s3) echo "$LASH_S3_IMAGE" ;;
  esac
}

service_container_port() {
  case "$1" in
    pg*) echo 5432 ;;
    s3) echo "$LASH_S3_CONTAINER_PORT" ;;
  esac
}

# `docker run` arguments: container environment, then the image's own command.
# Sets RUN_ARGS and RUN_COMMAND.
service_run_spec() {
  case "$1" in
    pg*)
      RUN_ARGS=(
        --env POSTGRES_USER=lash
        --env POSTGRES_PASSWORD=lash
        --env POSTGRES_DB=lash
      )
      # A linguistic default collation, so the suites that compare key order
      # against the database's own locale have one to compare against: the
      # alpine images' libc locale sorts bytewise even when named en_US.utf8
      # (replay_key_collation, FIG-3586). ICU as the cluster's default
      # provider exists from PostgreSQL 15; PG14 runs only the catalog
      # compatibility checks and keeps its default.
      if [ "$1" != pg14 ]; then
        RUN_ARGS+=(--env "POSTGRES_INITDB_ARGS=--locale-provider=icu --icu-locale=en-US")
      fi
      # pg_stat_statements is what the statement-count tests measure through.
      # The default 100 connections and lock table fit one test process; the
      # store job runs POSTGRES_SLOT_COUNT at once, each with its own pools and
      # each applying the whole DDL artifact in one transaction.
      RUN_COMMAND=(
        -c shared_preload_libraries=pg_stat_statements
        -c "max_connections=$((100 * POSTGRES_SLOT_COUNT))"
        -c max_locks_per_transaction=256
      )
      ;;
    s3)
      mapfile -t RUN_ARGS < <(lash_s3_run_args)
      mapfile -t RUN_COMMAND < <(lash_s3_command)
      ;;
  esac
}

# The environment the wrapped command sees. These are exactly the names
# `scripts/ci/store-tests.sh` forwards to the test spawn with `--test_env`.
# Sets TEST_ENV.
service_test_env() {
  local name="$1" port="$2"
  case "$name" in
    pg*)
      TEST_ENV=(
        "LASH_POSTGRES_DATABASE_URL=postgres://lash:lash@127.0.0.1:${port}/lash"
        "LASH_REQUIRE_POSTGRES=1"
        "LASH_POSTGRES_SLOT_COUNT=${POSTGRES_SLOT_COUNT}"
      )
      ;;
    s3)
      mapfile -t TEST_ENV < <(lash_s3_test_env "$port")
      ;;
  esac
}

# The readiness budget: 30 x 2s for PostgreSQL, 60 x 1s for the S3 service.
service_ready_attempts() {
  case "$1" in
    pg*) echo 30 ;;
    s3) echo 60 ;;
  esac
}

service_ready_interval() {
  case "$1" in
    pg*) echo 2 ;;
    s3) echo 1 ;;
  esac
}

# One readiness attempt. Exit status 0 means the service is up.
service_ready_probe() {
  local name="$1" container="$2" port="$3"
  case "$name" in
    pg*)
      docker exec "$container" pg_isready -U lash -d lash >/dev/null 2>&1
      ;;
    s3)
      lash_s3_ready "$container"
      ;;
  esac
}

# One-time preparation after readiness, before the command runs.
service_setup() {
  local name="$1" container="$2" port="$3"
  local index
  case "$name" in
    pg*)
      for ((index = 0; index < POSTGRES_SLOT_COUNT; index++)); do
        docker exec "$container" psql -U lash -d lash -v ON_ERROR_STOP=1 -q \
          -c "CREATE DATABASE lash_slot_${index}" >/dev/null
      done
      ;;
    *) : ;;
  esac
}

# The closing report: service-shaped work this wrapper does NOT cover, and the
# exact command that does, so a green run is never mistaken for full coverage.
# Moved here from the Kiln-side service manifest when the runner came into the repo.
not_covered() {
  cat <<'REPORT'

NOT covered by scripts/ci/with-service.sh -- run each of these yourself:
  * Test heavy suites
      why: the fault-matrix chunks fork real cargo test invocations of their own,
           so neither Bazel nor a container owns them
      run: cargo nextest run --profile ci-heavy --workspace --locked --no-fail-fast
  * Build worker E2E binaries
      why: staged between jobs rather than run against a service; trusted
           events take them from the shared build cache
      run: python3 scripts/ci/restate_suite.py stage-binaries //runbooks/restate-postgres-workers <dir>
  * Restate + Postgres + S3 Workers
      why: shell E2E drivers over release binaries rather than any Cargo or Bazel
           test label
      run: just restate-postgres-workers-e2e
  * slack-clone e2e feature
      why: the e2e feature is outside the resolved default workspace graph, so it
           has no Bazel label
      run: cargo clippy -p slack-clone --all-targets --features e2e --locked --no-deps -- -D warnings
  * Functional E2E process operations
      why: a compose runbook that stands up its own S3 service beside Restate and PostgreSQL
      run: bash scripts/process-operations-e2e.sh
REPORT
}

# ---------------------------------------------------------------------------
# Mechanics
# ---------------------------------------------------------------------------

note() { printf '%s: %s\n' "$PROGRAM" "$*" >&2; }

fail() {
  note "$*"
  exit 2
}

usage() {
  cat <<USAGE
usage: ${PROGRAM} [--list]
       ${PROGRAM} <${SERVICES[*]}|all> -- <command...>
USAGE
}

list_services() {
  local name
  for name in "${SERVICES[@]}"; do
    printf '%s\t%s\t%s\n' "$name" "$(service_image "$name")" "$(service_description "$name")"
  done
  not_covered
}

# Binds port 0 to learn a free port, then releases it. A container publishing a
# fixed host port collides with any other lane on this box; the kernel's
# ephemeral choice does not. The race between release and `docker run` is real
# but tiny, and a collision fails loudly at startup rather than silently
# sharing a database.
free_port() {
  python3 -c 'import socket
probe = socket.socket()
probe.bind(("127.0.0.1", 0))
print(probe.getsockname()[1])
probe.close()'
}

# A transient registry error is not a test failure: retry before giving up.
pull_image() {
  bash "${repo_root}/scripts/docker-pull-with-retry.sh" "$1" >/dev/null
}

containers=()

remove_containers() {
  local container
  for container in "${containers[@]+"${containers[@]}"}"; do
    docker rm --force "$container" >/dev/null 2>&1 || true
  done
  containers=()
}

on_interrupt() {
  remove_containers
  note "interrupted; containers removed"
  exit 130
}

wait_ready() {
  local name="$1" container="$2" port="$3"
  local interval attempts attempt
  interval="$(service_ready_interval "$name")"
  attempts="$(service_ready_attempts "$name")"
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if service_ready_probe "$name" "$container" "$port"; then
      note "${name}: ready after ${attempt} probe(s) on port ${port}"
      return 0
    fi
    sleep "$interval"
  done
  docker logs --tail 50 "$container" >&2 || true
  note "${name}: never became ready on port ${port}"
  return 1
}

run_with_service() {
  local name="$1"
  shift
  local port container image started rc
  port="$(free_port)"
  container="with-service-${name}-$$-${RANDOM}"
  image="$(service_image "$name")"

  pull_image "$image"

  service_run_spec "$name"
  containers+=("$container")
  if ! docker run --detach --name "$container" \
    --publish "127.0.0.1:${port}:$(service_container_port "$name")" \
    "${RUN_ARGS[@]}" "$image" "${RUN_COMMAND[@]}" >/dev/null; then
    note "${name}: docker run failed"
    remove_containers
    return 1
  fi
  note "${name}: ${image} on 127.0.0.1:${port} as ${container}"

  started="$SECONDS"
  rc=0
  if wait_ready "$name" "$container" "$port"; then
    if service_setup "$name" "$container" "$port"; then
      service_test_env "$name" "$port"
      set +e
      env "${TEST_ENV[@]}" "$@"
      rc=$?
      set -e
    else
      note "${name}: setup failed"
      rc=1
    fi
  else
    rc=1
  fi
  remove_containers

  if [ "$rc" -eq 0 ]; then
    note "${name}: command passed in $((SECONDS - started))s"
  else
    note "${name}: FAILED (exit ${rc}) after $((SECONDS - started))s"
  fi
  return "$rc"
}

main() {
  local list=0
  local -a requested=()
  while [ $# -gt 0 ]; do
    case "$1" in
      --) shift; break ;;
      --list) list=1; shift ;;
      -h | --help) usage; exit 0 ;;
      -*) fail "unknown option: $1" ;;
      *) requested+=("$1"); shift ;;
    esac
  done

  if [ "$list" -eq 1 ] || [ ${#requested[@]} -eq 0 ]; then
    if [ $# -gt 0 ]; then
      fail "a command needs a service: ${SERVICES[*]}, all"
    fi
    list_services
    return 0
  fi
  if [ $# -eq 0 ]; then
    usage >&2
    fail "no command given; separate it from the service with --"
  fi

  # Expand `all`, reject unknown names, and keep the declared order.
  local -a chosen=()
  local name want
  for want in "${requested[@]}"; do
    if [ "$want" = all ]; then
      continue
    fi
    if ! printf '%s\n' "${SERVICES[@]}" | grep -qx -- "$want"; then
      fail "unknown service '${want}'; available: ${SERVICES[*]}, all"
    fi
  done
  for name in "${SERVICES[@]}"; do
    for want in "${requested[@]}"; do
      if [ "$want" = all ] || [ "$want" = "$name" ]; then
        chosen+=("$name")
        break
      fi
    done
  done

  if ! docker version >/dev/null 2>&1; then
    fail "docker is unavailable; the service suites need a container runtime"
  fi

  # store-tests.sh refuses to guess which build path it is on. CI always sets
  # this from the plan job; a local run takes the trusted, shared-cache path,
  # the same one `kiln build` uses.
  export BAZEL_TRUSTED="${BAZEL_TRUSTED:-true}"

  trap remove_containers EXIT
  trap on_interrupt INT TERM

  local -a failed=()
  for name in "${chosen[@]}"; do
    if ! run_with_service "$name" "$@"; then
      failed+=("$name")
    fi
  done

  # In CI every step wraps one suite and the report would repeat on each; the
  # coverage question a closing report answers is a local one.
  if [ -z "${GITHUB_ACTIONS:-}" ]; then
    not_covered >&2
  fi

  if [ ${#failed[@]} -gt 0 ]; then
    note "failed: ${failed[*]}"
    return 1
  fi
  note "passed: ${chosen[*]}"
  return 0
}

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"
main "$@"
