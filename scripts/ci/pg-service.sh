# shellcheck shell=bash
# How a container of the official PostgreSQL image is known to be FINALLY
# ready, shared by every harness that starts one.
#
# The image's entrypoint starts a temporary server with
# `-c listen_addresses=''` to run the initdb scripts over the unix socket,
# shuts it down, and only then starts the real server. A readiness check
# against the container's unix socket answers against the temporary server
# too, so a client let through on it meets the restart:
# `FATAL: the database system is shutting down`. The temporary server never
# listens on TCP, so a probe only TCP can satisfy passes against the final
# server alone — the init restart is waited out by construction, with no
# sleeps, no log counting and no catch-up retries on the client.
#
# LASH_PG_READY_PROBE is the check: a `sh` snippet run INSIDE the container,
# which reads POSTGRES_USER, POSTGRES_DB and POSTGRES_PASSWORD from the
# container's environment — the way every harness here starts the image. The
# compose files' healthchecks carry the same probe literally with `$$`
# escapes, so it can be checked against this variable; keep them in sync.
#
# lash_pg_ready <exec prefix...> probes once through the caller's exec:
#   lash_pg_ready docker exec "$container"
#   lash_pg_ready "${compose[@]}" exec -T postgres
#
# lash_pg_wait <label> <seconds> <exec prefix...> polls the probe until it
# passes or the deadline expires, then fails loudly naming the label; the
# caller dumps the container's log.

# shellcheck disable=SC2016 # "$POSTGRES_*" expand inside the container's sh, not here.
LASH_PG_READY_PROBE='pg_isready -h 127.0.0.1 -p 5432 -U "$POSTGRES_USER" -d "$POSTGRES_DB" && PGPASSWORD=$POSTGRES_PASSWORD psql -h 127.0.0.1 -p 5432 -U "$POSTGRES_USER" -d "$POSTGRES_DB" -Atqc "SELECT 1"'

# One probe of a Postgres container through the caller's exec prefix: the
# arguments are a command that runs `sh -c` inside the container, so
# `docker exec <name>` and `docker compose ... exec -T <service>` both fit.
lash_pg_ready() {
  "$@" sh -c "$LASH_PG_READY_PROBE" >/dev/null 2>&1
}

# Poll a Postgres container's TCP readiness until `seconds` elapse. On
# expiry the label is printed and the call fails, so the caller can dump
# the container's log; a passing probe returns at once.
lash_pg_wait() {
  local label="$1" seconds="$2" deadline
  shift 2
  deadline=$((SECONDS + seconds))
  until lash_pg_ready "$@"; do
    if ((SECONDS >= deadline)); then
      echo "${label}: PostgreSQL did not accept TCP connections within ${seconds}s" >&2
      return 1
    fi
    sleep 1
  done
}
