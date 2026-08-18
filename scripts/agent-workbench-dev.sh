#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

state_dir="${AGENT_WORKBENCH_RUN_DIR:-.agent-workbench/run}"
mkdir -p "$state_dir"

log() {
  printf '[agent-workbench] %s\n' "$*" >&2
}

die() {
  printf '[agent-workbench] error: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'USAGE'
Usage:
  scripts/agent-workbench-dev.sh [up] [--port PORT | --addr HOST:PORT]
  scripts/agent-workbench-dev.sh foreground [--port PORT | --addr HOST:PORT]
  scripts/agent-workbench-dev.sh restart [--port PORT | --addr HOST:PORT]
  scripts/agent-workbench-dev.sh status [--port PORT | --addr HOST:PORT]
  scripts/agent-workbench-dev.sh logs [--port PORT | --addr HOST:PORT] [-f]
  scripts/agent-workbench-dev.sh down [--port PORT | --addr HOST:PORT]

Defaults:
  up is detached and idempotent.
  restart replaces only the workbench process and preserves managed services.
  down stops the workbench and any Restate or Postgres container it started.
  AGENT_WORKBENCH_POSTGRES=1 starts a port-isolated managed Postgres container
  unless AGENT_WORKBENCH_DATABASE_URL points at an existing database.
  --port PORT binds 127.0.0.1:PORT.
  Managed-service ports use a 10-port stride for each workbench-port step from
  3030, unless their environment variables override them.
  Without --port/--addr, AGENT_WORKBENCH_ADDR is used, then 127.0.0.1:3030.
  AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS sets the model context window; it
  defaults to 200000 and must be at least twice the plugin's compaction buffer
  (currently 20,000 tokens).
USAGE
}

url_host_port() {
  local url="${1#*://}"
  url="${url%%/*}"
  url="${url##*@}"
  local host="${url%:*}"
  local port="${url##*:}"
  if [[ -z "$host" || -z "$port" || "$host" = "$port" ]]; then
    die "expected URL with explicit host and port, got '$1'"
  fi
  printf '%s %s\n' "$host" "$port"
}

addr_host_port() {
  local addr="$1"
  local host="${addr%:*}"
  local port="${addr##*:}"
  if [[ -z "$host" || -z "$port" || "$host" = "$port" ]]; then
    die "expected address as host:port, got '$addr'"
  fi
  printf '%s %s\n' "$host" "$port"
}

validate_port() {
  local label="$1"
  local port="$2"
  [[ "$port" =~ ^[0-9]+$ ]] || die "$label port must be numeric, got '$port'"
  local port_number=$((10#$port))
  (( port_number >= 1 && port_number <= 65535 )) \
    || die "$label port must be between 1 and 65535, got '$port'"
}

tcp_ready() {
  local host="$1"
  local port="$2"
  timeout 1 bash -c "cat < /dev/null > /dev/tcp/$host/$port" >/dev/null 2>&1
}

wait_tcp() {
  local label="$1"
  local host="$2"
  local port="$3"
  local timeout_seconds="${4:-60}"
  local deadline=$((SECONDS + timeout_seconds))
  until tcp_ready "$host" "$port"; do
    if (( SECONDS >= deadline )); then
      log "$label did not become ready at $host:$port"
      return 1
    fi
    sleep 1
  done
}

json_string() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  printf '"%s"' "$value"
}

register_deployment() {
  local admin_url="$1"
  local endpoint_url="$2"
  local payload
  payload="$(printf '{"uri":%s,"force":true,"breaking":true}' "$(json_string "$endpoint_url")")"
  local deadline=$((SECONDS + 60))
  local last_response=""
  until last_response="$(
    curl --http2-prior-knowledge -fsS \
      -H 'content-type: application/json' \
      -X POST \
      --data "$payload" \
      "${admin_url%/}/deployments" 2>&1
  )"; do
    require_workbench_alive "during Restate deployment registration"
    if (( SECONDS >= deadline )); then
      printf '%s\n' "$last_response" >&2
      return 1
    fi
    sleep 1
  done
  require_workbench_alive "after Restate deployment registration"
}

open_browser() {
  local url="$1"
  case "${AGENT_WORKBENCH_OPEN:-1}" in
    0|false|False|FALSE|no|No|NO)
      return
      ;;
  esac
  if command -v xdg-open >/dev/null 2>&1; then
    xdg-open "$url" >/dev/null 2>&1 || true
  elif command -v open >/dev/null 2>&1; then
    open "$url" >/dev/null 2>&1 || true
  fi
}

port_owner() {
  local port="$1"
  if command -v lsof >/dev/null 2>&1; then
    lsof -nP -iTCP:"$port" -sTCP:LISTEN || true
  elif command -v ss >/dev/null 2>&1; then
    ss -ltnp 2>/dev/null | grep -E "[:.]$port[[:space:]]" || true
  else
    printf 'install lsof or ss to show the owning process for port %s\n' "$port"
  fi
}

process_start_time() {
  local pid="${1:-}"
  [[ "$pid" =~ ^[0-9]+$ ]] || return 1
  [[ -r "/proc/$pid/stat" ]] || return 1
  local start_time
  start_time="$(awk '{print $22}' "/proc/$pid/stat" 2>/dev/null || true)"
  [[ "$start_time" =~ ^[0-9]+$ ]] || return 1
  printf '%s\n' "$start_time"
}

read_pid_file() {
  local file="$1"
  [[ -f "$file" ]] || return 1
  local pid start_time extra
  read -r pid start_time extra < "$file" || return 1
  [[ "$pid" =~ ^[0-9]+$ && "$start_time" =~ ^[0-9]+$ && -z "$extra" ]] || return 1
  printf '%s %s\n' "$pid" "$start_time"
}

pid_identity_matches() {
  local pid="$1" expected_start_time="$2" current_start_time
  current_start_time="$(process_start_time "$pid" 2>/dev/null || true)"
  [[ -n "$current_start_time" && "$current_start_time" = "$expected_start_time" ]]
}

pid_file_identity() {
  local file="$1" record pid start_time
  record="$(read_pid_file "$file" 2>/dev/null || true)"
  [[ -n "$record" ]] || return 1
  read -r pid start_time <<<"$record"
  pid_identity_matches "$pid" "$start_time" || return 1
  printf '%s\n' "$record"
}

write_pid_file() {
  local file="$1" pid="$2" start_time
  start_time="$(process_start_time "$pid")" || return 1
  printf '%s %s\n' "$pid" "$start_time" > "$file"
}

remove_stale_pid_file() {
  local file="$1"
  if [[ -e "$file" ]]; then
    log "removing stale or mismatched PID file $file"
  fi
  rm -f "$file" "${file%.pid}.meta"
}

signal_verified_process() {
  local signal="$1" pid="$2" start_time="$3"
  pid_identity_matches "$pid" "$start_time" || return 1
  if kill "-$signal" "-$pid" >/dev/null 2>&1; then
    return 0
  fi
  pid_identity_matches "$pid" "$start_time" || return 1
  kill "-$signal" "$pid" >/dev/null 2>&1
}

tail_log() {
  if [[ -f "$log_file" ]]; then
    tail -n "${AGENT_WORKBENCH_LOG_LINES:-120}" "$log_file" >&2 || true
  else
    log "no log file yet at $log_file"
  fi
}

require_workbench_alive() {
  local phase="$1"
  local record="" pid="" start_time=""
  record="$(read_pid_file "$pid_file" 2>/dev/null || true)"
  if [[ -n "$record" ]]; then
    read -r pid start_time <<<"$record"
  fi
  if [[ -n "$pid" ]] && pid_identity_matches "$pid" "$start_time"; then
    return
  fi

  local exit_status="unknown"
  if [[ -n "$pid" ]]; then
    if wait "$pid" 2>/dev/null; then
      exit_status=0
    else
      exit_status=$?
    fi
  fi
  tail_log
  remove_stale_pid_file "$pid_file"
  if [[ -z "$pid" ]]; then
    die "workbench process metadata disappeared $phase"
  fi
  die "workbench process $pid exited with status $exit_status $phase"
}

workbench_ready() {
  curl -fsS --max-time 2 "$workbench_url/healthz" 2>/dev/null \
    | grep -q '"service":"agent-workbench"'
}

cleanup_stale_pid() {
  [[ -e "$pid_file" ]] || return 0
  if pid_file_identity "$pid_file" >/dev/null; then
    return
  fi
  remove_stale_pid_file "$pid_file"
}

stop_pid_file() {
  local file="$1"
  [[ -e "$file" ]] || return 0
  local record="" pid="" start_time=""
  record="$(pid_file_identity "$file" 2>/dev/null || true)"
  if [[ -z "$record" ]]; then
    remove_stale_pid_file "$file"
    return
  fi
  read -r pid start_time <<<"$record"

  log "stopping process $pid"
  if ! signal_verified_process TERM "$pid" "$start_time"; then
    remove_stale_pid_file "$file"
    return
  fi
  for _ in {1..30}; do
    pid_identity_matches "$pid" "$start_time" || break
    sleep 0.5
  done
  if pid_identity_matches "$pid" "$start_time"; then
    log "process $pid did not exit; sending SIGKILL"
    if ! signal_verified_process KILL "$pid" "$start_time"; then
      log "process identity changed before SIGKILL; refusing to signal PID $pid"
    fi
  fi

  rm -f "$file" "${file%.pid}.meta"
}

stop_started_restate() {
  if [[ -f "$restate_marker_file" ]]; then
    local container
    container="$(cat "$restate_marker_file")"
    if [[ -n "$container" ]]; then
      log "stopping Restate container $container"
      docker rm -f "$container" >/dev/null 2>&1 || true
    fi
    rm -f "$restate_marker_file"
  fi
}

stop_started_postgres() {
  if [[ -f "$postgres_marker_file" ]]; then
    local container
    container="$(cat "$postgres_marker_file")"
    if [[ -n "$container" ]]; then
      log "stopping Postgres container $container"
      docker rm -f "$container" >/dev/null 2>&1 || true
    fi
    rm -f "$postgres_marker_file"
  fi
}

stop_target() {
  stop_pid_file "$pid_file"
  stop_started_restate
  stop_started_postgres
}

stop_all_known() {
  local found=0
  local file
  for file in "$state_dir"/workbench-*.pid; do
    [[ -e "$file" ]] || continue
    found=1
    stop_pid_file "$file"
  done
  for file in "$state_dir"/restate-*.container; do
    [[ -e "$file" ]] || continue
    local container
    container="$(cat "$file")"
    if [[ -n "$container" ]]; then
      log "stopping Restate container $container"
      docker rm -f "$container" >/dev/null 2>&1 || true
    fi
    rm -f "$file"
  done
  for file in "$state_dir"/postgres-*.container; do
    [[ -e "$file" ]] || continue
    local container
    container="$(cat "$file")"
    if [[ -n "$container" ]]; then
      log "stopping Postgres container $container"
      docker rm -f "$container" >/dev/null 2>&1 || true
    fi
    rm -f "$file"
  done
  if (( ! found )); then
    log "no managed workbench processes found"
  fi
}

ensure_ports_available() {
  cleanup_stale_pid
  if workbench_ready; then
    log "already ready: $workbench_url"
    return 1
  fi
  if tcp_ready "$workbench_wait_host" "$workbench_port"; then
    port_owner "$workbench_port" >&2
    die "workbench UI port $workbench_host:$workbench_port is already in use by a non-workbench process"
  fi
  if tcp_ready "$endpoint_wait_host" "$endpoint_port"; then
    port_owner "$endpoint_port" >&2
    die "workbench Restate endpoint port $endpoint_host:$endpoint_port is already in use"
  fi
}

ensure_restate() {
  if tcp_ready "$ingress_host" "$ingress_port" && tcp_ready "$admin_host" "$admin_port"; then
    log "using existing Restate at ingress=$restate_ingress_url admin=$restate_admin_url"
    return
  fi

  command -v docker >/dev/null 2>&1 || die "Restate is not running and docker is unavailable"
  log "starting Restate container $restate_container from $restate_image"
  docker rm -f "$restate_container" >/dev/null 2>&1 || true
  docker run -d \
    --name "$restate_container" \
    --network host \
    -e RESTATE_INGRESS__BIND_PORT="$ingress_port" \
    -e RESTATE_ADMIN__BIND_PORT="$admin_port" \
    -e RESTATE_BIND_PORT="$restate_node_port" \
    "$restate_image" >/dev/null
  printf '%s\n' "$restate_container" > "$restate_marker_file"

  if ! wait_tcp "Restate ingress" "$ingress_host" "$ingress_port" 60; then
    docker logs "$restate_container" >&2 || true
    stop_started_restate
    die "Restate ingress did not become ready at $restate_ingress_url"
  fi
  if ! wait_tcp "Restate admin" "$admin_host" "$admin_port" 60; then
    docker logs "$restate_container" >&2 || true
    stop_started_restate
    die "Restate admin did not become ready at $restate_admin_url"
  fi
}

ensure_postgres() {
  (( postgres_enabled )) || return 0
  if tcp_ready "$postgres_host" "$postgres_port"; then
    log "using existing Postgres at $postgres_host:$postgres_port"
    return
  fi

  command -v docker >/dev/null 2>&1 || die "Postgres is not running and docker is unavailable"
  log "starting Postgres container $postgres_container from $postgres_image"
  docker rm -f "$postgres_container" >/dev/null 2>&1 || true
  docker run -d \
    --name "$postgres_container" \
    --network host \
    -e POSTGRES_USER=lash \
    -e POSTGRES_PASSWORD=lash \
    -e POSTGRES_DB=lash \
    "$postgres_image" -p "$postgres_port" >/dev/null
  printf '%s\n' "$postgres_container" > "$postgres_marker_file"

  if ! wait_tcp "Postgres" "$postgres_host" "$postgres_port" 60; then
    docker logs "$postgres_container" >&2 || true
    stop_started_postgres
    die "Postgres did not become ready at $postgres_host:$postgres_port"
  fi
}

endpoint_url() {
  if [[ -n "$configured_endpoint_url" ]]; then
    printf '%s\n' "$configured_endpoint_url"
  elif [[ "$endpoint_host" = "0.0.0.0" ]]; then
    printf 'http://127.0.0.1:%s\n' "$endpoint_port"
  else
    printf 'http://%s:%s\n' "$endpoint_host" "$endpoint_port"
  fi
}

write_meta() {
  {
    printf 'workbench_addr=%q\n' "$workbench_addr"
    printf 'workbench_url=%q\n' "$workbench_url"
    printf 'restate_endpoint_addr=%q\n' "$restate_endpoint_addr"
    printf 'restate_ingress_url=%q\n' "$restate_ingress_url"
    printf 'restate_admin_url=%q\n' "$restate_admin_url"
    printf 'store_backend=%q\n' "$store_backend"
    printf 'postgres_host=%q\n' "$postgres_host"
    printf 'postgres_port=%q\n' "$postgres_port"
    printf 'log_file=%q\n' "$log_file"
  } > "$meta_file"
}

start_detached() {
  log "building agent-workbench (profile: judged)"
  cargo build -p agent-workbench --profile judged

  # Launch the binary cargo just built: honor CARGO_TARGET_DIR, or a stale
  # binary in the repo-local target/ boots instead of the fresh build.
  local workbench_bin="${CARGO_TARGET_DIR:-$repo_root/target}/judged/agent-workbench"
  local -a workbench_env=(
    "AGENT_WORKBENCH_ADDR=$workbench_addr"
    "AGENT_WORKBENCH_RESTATE_ADDR=$restate_endpoint_addr"
    "AGENT_WORKBENCH_DATABASE_URL=$agent_workbench_database_url"
    "RESTATE_INGRESS_URL=$restate_ingress_url"
    "RESTATE_ADMIN_URL=$restate_admin_url"
  )
  if [[ -n "${AGENT_WORKBENCH_DATA_DIR:-}" ]]; then
    workbench_env+=("AGENT_WORKBENCH_DATA_DIR=$AGENT_WORKBENCH_DATA_DIR")
  fi
  printf '\n[%s] starting agent-workbench at %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$workbench_url" >> "$log_file"
  if command -v setsid >/dev/null 2>&1; then
    setsid env "${workbench_env[@]}" \
      "$workbench_bin" >> "$log_file" 2>&1 < /dev/null &
  else
    nohup env "${workbench_env[@]}" \
      "$workbench_bin" >> "$log_file" 2>&1 < /dev/null &
  fi
  local pid="$!"
  write_pid_file "$pid_file" "$pid" || die "could not record process identity for $pid"
  write_meta
  log "started process $pid; log: $log_file"
}

wait_workbench_ready() {
  local timeout_seconds="${1:-90}"
  local deadline=$((SECONDS + timeout_seconds))
  until workbench_ready; do
    require_workbench_alive "before becoming ready"
    if (( SECONDS >= deadline )); then
      tail_log
      stop_target
      die "workbench did not become healthy at $workbench_url/healthz"
    fi
    sleep 1
  done
  require_workbench_alive "after the health check became ready"
}

wait_workbench_endpoint_ready() {
  local timeout_seconds="${1:-90}"
  local deadline=$((SECONDS + timeout_seconds))
  until tcp_ready "$endpoint_wait_host" "$endpoint_port"; do
    require_workbench_alive "while waiting for its Restate endpoint"
    if (( SECONDS >= deadline )); then
      tail_log
      stop_target
      die "workbench Restate endpoint did not become ready at $restate_endpoint_addr"
    fi
    sleep 1
  done
  require_workbench_alive "after its Restate endpoint became ready"
}

run_up() {
  if ! ensure_ports_available; then
    return
  fi
  ensure_restate
  ensure_postgres
  start_detached
  wait_workbench_ready 90
  wait_workbench_endpoint_ready 90
  local deployment_url
  deployment_url="$(endpoint_url)"
  log "registering Restate deployment $deployment_url"
  if ! register_deployment "$restate_admin_url" "$deployment_url"; then
    tail_log
    stop_target
    die "failed to register Restate deployment $deployment_url through $restate_admin_url"
  fi
  require_workbench_alive "before reporting ready"
  log "ready: $workbench_url"
  open_browser "$workbench_url"
}

run_foreground() {
  if ! ensure_ports_available; then
    return
  fi
  ensure_restate
  ensure_postgres

  local started_pid="" started_start_time=""
  cleanup_foreground() {
    if [[ -n "$started_pid" ]]; then
      if pid_identity_matches "$started_pid" "$started_start_time"; then
        kill "$started_pid" >/dev/null 2>&1 || true
      else
        log "process identity changed before foreground cleanup; refusing to signal PID $started_pid"
      fi
      wait "$started_pid" >/dev/null 2>&1 || true
    fi
    stop_started_restate
    stop_started_postgres
  }
  trap cleanup_foreground EXIT INT TERM

  log "starting workbench at $workbench_url"
  local -a workbench_env=(
    "AGENT_WORKBENCH_ADDR=$workbench_addr"
    "AGENT_WORKBENCH_RESTATE_ADDR=$restate_endpoint_addr"
    "AGENT_WORKBENCH_DATABASE_URL=$agent_workbench_database_url"
    "RESTATE_INGRESS_URL=$restate_ingress_url"
    "RESTATE_ADMIN_URL=$restate_admin_url"
  )
  if [[ -n "${AGENT_WORKBENCH_DATA_DIR:-}" ]]; then
    workbench_env+=("AGENT_WORKBENCH_DATA_DIR=$AGENT_WORKBENCH_DATA_DIR")
  fi
  env "${workbench_env[@]}" cargo run -p agent-workbench --profile judged &
  started_pid="$!"
  write_pid_file "$pid_file" "$started_pid" || die "could not record process identity for $started_pid"
  started_start_time="$(process_start_time "$started_pid")"
  write_meta

  wait_workbench_ready 90
  wait_workbench_endpoint_ready 90
  local deployment_url
  deployment_url="$(endpoint_url)"
  log "registering Restate deployment $deployment_url"
  register_deployment "$restate_admin_url" "$deployment_url" \
    || die "failed to register Restate deployment $deployment_url through $restate_admin_url"

  require_workbench_alive "before reporting ready"
  log "ready: $workbench_url"
  open_browser "$workbench_url"
  wait "$started_pid"
}

run_status_one() {
  cleanup_stale_pid
  local record="" pid="" start_time=""
  record="$(pid_file_identity "$pid_file" 2>/dev/null || true)"
  if [[ -n "$record" ]]; then
    read -r pid start_time <<<"$record"
  fi
  if workbench_ready; then
    if [[ -n "$pid" ]]; then
      log "running: $workbench_url (pid $pid, log $log_file)"
    else
      log "running: $workbench_url (unmanaged process)"
    fi
    return 0
  fi
  if [[ -n "$pid" ]] && pid_identity_matches "$pid" "$start_time"; then
    log "process $pid exists but health check failed: $workbench_url/healthz"
    return 1
  fi
  log "stopped: $workbench_url"
  return 1
}

run_status_all() {
  local found=0
  local file
  for file in "$state_dir"/workbench-*.pid; do
    [[ -e "$file" ]] || continue
    found=1
    pid_file="$file"
    meta_file="${file%.pid}.meta"
    # shellcheck disable=SC1090
    [[ -f "$meta_file" ]] && source "$meta_file"
    run_status_one || true
  done
  if (( ! found )); then
    run_status_one
  fi
}

run_logs() {
  if [[ ! -f "$log_file" && -z "${explicit_target:-}" ]]; then
    local count=0
    local only=""
    local file
    for file in "$state_dir"/workbench-*.log; do
      [[ -e "$file" ]] || continue
      only="$file"
      count=$((count + 1))
    done
    if (( count == 1 )); then
      log_file="$only"
    elif (( count > 1 )); then
      ls -1 "$state_dir"/workbench-*.log >&2
      die "multiple workbench logs found; pass --port or --addr"
    fi
  fi
  [[ -f "$log_file" ]] || die "no log file found at $log_file"
  if (( follow_logs )); then
    tail -f "$log_file"
  else
    tail -n "${AGENT_WORKBENCH_LOG_LINES:-120}" "$log_file"
  fi
}

action="up"
if (($#)); then
  case "$1" in
    up|start|foreground|run|restart|status|logs|down|stop)
      action="$1"
      shift
      ;;
    help|-h|--help)
      usage
      exit 0
      ;;
  esac
fi

port_override=""
addr_override=""
explicit_target=""
follow_logs=0
while (($#)); do
  case "$1" in
    --port)
      [[ $# -ge 2 ]] || die "--port requires a value"
      port_override="$2"
      explicit_target=1
      shift 2
      ;;
    --addr)
      [[ $# -ge 2 ]] || die "--addr requires a value"
      addr_override="$2"
      explicit_target=1
      shift 2
      ;;
    -f|--follow)
      follow_logs=1
      shift
      ;;
    [0-9]*)
      port_override="$1"
      explicit_target=1
      shift
      ;;
    help|-h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

if [[ -n "$addr_override" ]]; then
  workbench_addr="$addr_override"
elif [[ -n "$port_override" ]]; then
  workbench_addr="127.0.0.1:$port_override"
else
  workbench_addr="${AGENT_WORKBENCH_ADDR:-127.0.0.1:3030}"
fi

read -r workbench_host workbench_port < <(addr_host_port "$workbench_addr")
validate_port "workbench" "$workbench_port"
workbench_port_number=$((10#$workbench_port))
managed_service_port_stride=10
port_offset=$(((workbench_port_number - 3030) * managed_service_port_stride))
default_restate_endpoint_port=$((9081 + port_offset))
default_restate_ingress_port=$((8080 + port_offset))
default_restate_admin_port=$((19070 + port_offset))
default_restate_node_port=$((19071 + port_offset))
default_postgres_port=$((15432 + port_offset))

restate_endpoint_addr="${AGENT_WORKBENCH_RESTATE_ADDR:-127.0.0.1:$default_restate_endpoint_port}"
restate_ingress_url="${RESTATE_INGRESS_URL:-http://127.0.0.1:$default_restate_ingress_port}"
restate_admin_url="${RESTATE_ADMIN_URL:-http://127.0.0.1:${AGENT_WORKBENCH_RESTATE_ADMIN_PORT:-$default_restate_admin_port}}"
restate_image="${AGENT_WORKBENCH_RESTATE_IMAGE:-restatedev/restate:1.7.0}"
restate_node_port="${AGENT_WORKBENCH_RESTATE_NODE_PORT:-$default_restate_node_port}"
configured_endpoint_url="${AGENT_WORKBENCH_RESTATE_ENDPOINT_URL:-}"

postgres_requested="${AGENT_WORKBENCH_POSTGRES:-0}"
agent_workbench_database_url="${AGENT_WORKBENCH_DATABASE_URL:-}"
postgres_enabled=0
case "$postgres_requested" in
  1|true|True|TRUE|yes|Yes|YES) postgres_enabled=1 ;;
  0|false|False|FALSE|no|No|NO|'') ;;
  *) die "AGENT_WORKBENCH_POSTGRES must be true or false, got '$postgres_requested'" ;;
esac
if [[ -n "$agent_workbench_database_url" ]]; then
  postgres_enabled=1
fi
postgres_port="${AGENT_WORKBENCH_POSTGRES_PORT:-$default_postgres_port}"
postgres_host="${AGENT_WORKBENCH_POSTGRES_HOST:-127.0.0.1}"
postgres_image="${AGENT_WORKBENCH_POSTGRES_IMAGE:-postgres:16-alpine}"
postgres_container="${AGENT_WORKBENCH_POSTGRES_CONTAINER:-lash-agent-workbench-dev-postgres-$workbench_port}"
if (( postgres_enabled )) && [[ -z "$agent_workbench_database_url" ]]; then
  agent_workbench_database_url="postgres://lash:lash@$postgres_host:$postgres_port/lash"
elif [[ -n "$agent_workbench_database_url" ]]; then
  read -r postgres_host postgres_port < <(url_host_port "$agent_workbench_database_url")
fi

restate_container="${AGENT_WORKBENCH_RESTATE_CONTAINER:-lash-agent-workbench-dev-restate-$workbench_port}"
read -r endpoint_host endpoint_port < <(addr_host_port "$restate_endpoint_addr")
read -r ingress_host ingress_port < <(url_host_port "$restate_ingress_url")
read -r admin_host admin_port < <(url_host_port "$restate_admin_url")
validate_port "Restate endpoint" "$endpoint_port"
validate_port "Restate ingress" "$ingress_port"
validate_port "Restate admin" "$admin_port"
validate_port "Restate node" "$restate_node_port"
if (( postgres_enabled )); then
  validate_port "Postgres" "$postgres_port"
fi
store_backend="sqlite"
if (( postgres_enabled )); then
  store_backend="postgres"
fi

workbench_wait_host="$workbench_host"
endpoint_wait_host="$endpoint_host"
if [[ "$workbench_wait_host" = "0.0.0.0" ]]; then
  workbench_wait_host="127.0.0.1"
fi
if [[ "$endpoint_wait_host" = "0.0.0.0" ]]; then
  endpoint_wait_host="127.0.0.1"
fi
workbench_url="http://$workbench_wait_host:$workbench_port"

state_key="$(printf '%s' "$workbench_addr" | tr -c 'A-Za-z0-9_.-' '_')"
pid_file="$state_dir/workbench-$state_key.pid"
meta_file="$state_dir/workbench-$state_key.meta"
log_file="$state_dir/workbench-$state_key.log"
restate_marker_file="$state_dir/restate-$state_key.container"
postgres_marker_file="$state_dir/postgres-$state_key.container"

case "$action" in
  up|start)
    run_up
    ;;
  foreground|run)
    run_foreground
    ;;
  restart)
    stop_pid_file "$pid_file"
    run_up
    ;;
  status)
    if [[ -z "$explicit_target" ]]; then
      run_status_all
    else
      run_status_one
    fi
    ;;
  logs)
    run_logs
    ;;
  down|stop)
    if [[ -z "$explicit_target" ]]; then
      stop_all_known
    else
      stop_target
    fi
    ;;
  *)
    die "unknown command: $action"
    ;;
esac
