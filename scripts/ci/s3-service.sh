# shellcheck shell=bash
# lash's S3-compatible test service: Garage, the S3 implementation figments
# runs in production, so lash's S3 suites test against the same server.
#
# This file is sourced. It is the one owner of the service's facts -- the
# image, the configuration, the throwaway credentials, the bucket -- and of
# how a container of it is started and known to be ready. Every harness that
# needs S3 goes through it: `scripts/ci/with-service.sh s3`, the process
# operations and Restate-workers runbooks (their compose files read the
# exported LASH_S3_* values), `scripts/gate-container-smoke.sh` and
# `scripts/push-gate.sh`.
#
# No bootstrap step: `garage server --single-node --default-bucket` lays out
# the one-node cluster, creates the access key from GARAGE_DEFAULT_ACCESS_KEY
# and GARAGE_DEFAULT_SECRET_KEY, and creates the bucket from
# GARAGE_DEFAULT_BUCKET with that key's access, all at startup. The service is
# ready once the bucket is.

_lash_s3_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# Pinned by digest as well as tag. Bump with figments' dev-infra Garage.
LASH_S3_IMAGE="dxflrs/garage:v2.3.0@sha256:866bd13ed2038ba7e7190e840482bc27234c4afaf77be8cfa439ae088c1e4690"
LASH_S3_CONFIG="${_lash_s3_root}/scripts/ci/garage.toml"
LASH_S3_CONTAINER_PORT=3900
LASH_S3_REGION="us-east-1"
LASH_S3_BUCKET="lash-attachments"
# Throwaway credentials for a loopback test server; Garage requires a key id
# of `GK` and 24 hex digits and 64-hex secrets.
LASH_S3_ACCESS_KEY="GK1a5b000000000000000000e5"
LASH_S3_SECRET_KEY="1a5b0000000000000000000000000000000000000000000000000000000000e5"
LASH_S3_RPC_SECRET="1a5b0000000000000000000000000000000000000000000000000000000000c0"
export LASH_S3_IMAGE LASH_S3_CONFIG LASH_S3_REGION LASH_S3_BUCKET \
  LASH_S3_ACCESS_KEY LASH_S3_SECRET_KEY LASH_S3_RPC_SECRET

# The `docker run` arguments before the image, and the command after it.
lash_s3_run_args() {
  printf '%s\n' \
    --env "GARAGE_RPC_SECRET=${LASH_S3_RPC_SECRET}" \
    --env "GARAGE_DEFAULT_ACCESS_KEY=${LASH_S3_ACCESS_KEY}" \
    --env "GARAGE_DEFAULT_SECRET_KEY=${LASH_S3_SECRET_KEY}" \
    --env "GARAGE_DEFAULT_BUCKET=${LASH_S3_BUCKET}" \
    --volume "${LASH_S3_CONFIG}:/etc/garage.toml:ro"
}
lash_s3_command() {
  printf '%s\n' /garage server --single-node --default-bucket
}

# One readiness probe of a running container: its bucket exists.
lash_s3_ready() {
  docker exec "$1" /garage bucket info "${LASH_S3_BUCKET}" >/dev/null 2>&1
}

# Wait for a container to be ready: lash_s3_wait <container> [seconds].
lash_s3_wait() {
  local container="$1" deadline=$((SECONDS + ${2:-60}))
  until lash_s3_ready "$container"; do
    if ((SECONDS >= deadline)); then
      docker logs --tail 50 "$container" >&2 || true
      echo "S3 service ${container} did not become ready" >&2
      return 1
    fi
    sleep 0.5
  done
}

# Start a detached container publishing the S3 API on a loopback port:
# lash_s3_start <container> <host-port> [extra docker run args...].
lash_s3_start() {
  local container="$1" port="$2"
  shift 2
  local -a run_args command
  mapfile -t run_args < <(lash_s3_run_args)
  mapfile -t command < <(lash_s3_command)
  docker run --detach --name "$container" "$@" \
    --publish "127.0.0.1:${port}:${LASH_S3_CONTAINER_PORT}" \
    "${run_args[@]}" "${LASH_S3_IMAGE}" "${command[@]}" >/dev/null
}

# The environment an S3-gated test reads, for a server on a loopback port.
lash_s3_test_env() {
  printf '%s\n' \
    "LASH_REQUIRE_S3=1" \
    "LASH_S3_ENDPOINT=http://127.0.0.1:$1" \
    "LASH_S3_REGION=${LASH_S3_REGION}" \
    "LASH_S3_BUCKET=${LASH_S3_BUCKET}" \
    "LASH_S3_ACCESS_KEY=${LASH_S3_ACCESS_KEY}" \
    "LASH_S3_SECRET_KEY=${LASH_S3_SECRET_KEY}"
}
