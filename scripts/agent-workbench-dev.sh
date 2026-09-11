#!/usr/bin/env bash
set -euo pipefail
umask 077

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

configured_data_dir="${AGENT_WORKBENCH_DATA_DIR:-.agent-workbench}"
data_dir_existed_before_invocation=0
if [[ -e "$configured_data_dir" || -L "$configured_data_dir" ]]; then
  data_dir_existed_before_invocation=1
fi
data_dir_created_this_attempt=$((1 - data_dir_existed_before_invocation))
state_dir="${AGENT_WORKBENCH_RUN_DIR:-.agent-workbench/run}"

started_workbench_this_attempt=0
started_workbench_pid=""
started_workbench_start_time=""
started_restate_this_attempt=0
external_restate_used_this_attempt=0
started_postgres_this_attempt=0
start_attempt_active=0
reset_committed=0
reset_destructive_started=0
reset_recovery_command=""

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
  scripts/agent-workbench-dev.sh restart --reset-dev-state [--port PORT | --addr HOST:PORT]
  scripts/agent-workbench-dev.sh status [--port PORT | --addr HOST:PORT]
  scripts/agent-workbench-dev.sh logs [--port PORT | --addr HOST:PORT] [-f]
  scripts/agent-workbench-dev.sh down [--port PORT | --addr HOST:PORT]

Defaults:
  up is detached and idempotent.
  restart refuses unless --reset-dev-state is present. The explicit reset is
  destructive: it replaces a wholly launcher-owned disposable stack, including
  its Restate journals and corresponding application data. External, mixed,
  legacy, or ambiguous ownership is refused before anything is stopped.
  down stops the workbench and any Restate or Postgres container it started.
  AGENT_WORKBENCH_POSTGRES=1 starts a port-isolated managed Postgres container
  unless AGENT_WORKBENCH_DATABASE_URL points at an existing database.
  --port PORT binds 127.0.0.1:PORT.
  Managed-service ports use a 10-port stride for each workbench-port step from
  3030 for workbench ports 2223 through 7676. Outside that range, set the
  managed-service endpoint environment variables explicitly.
  Without --port/--addr, AGENT_WORKBENCH_ADDR is used, then 127.0.0.1:3030.
  AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS sets the model context window; it
  defaults to 200000 and must be at least twice the plugin's compaction buffer
  (currently 20,000 tokens).
USAGE
}

url_host_port() {
  local parsed=""
  parsed="$(python3 -c '
import sys
from urllib.parse import urlsplit

try:
    url = urlsplit(sys.argv[1])
    host = url.hostname
    port = url.port
except ValueError:
    raise SystemExit(1)
if url.scheme not in {"http", "https", "postgres", "postgresql"} or not host or port is None:
    raise SystemExit(1)
print(f"{host} {port}")
' "$1")" || die "expected URL with explicit host and port"
  printf '%s\n' "$parsed"
}

url_is_diagnostic_safe() {
  python3 -c '
import sys
from urllib.parse import urlsplit

try:
    url = urlsplit(sys.argv[1])
except ValueError:
    raise SystemExit(1)
raise SystemExit(1 if url.username or url.password or url.query or url.fragment else 0)
' "$1"
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
  payload="$(printf '{"uri":%s,"force":false,"breaking":false}' "$(json_string "$endpoint_url")")"
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

deployment_uri_registered() {
  local admin_url="$1"
  local endpoint_url="$2"
  local response
  response="$(
    curl --http2-prior-knowledge -fsS \
      "${admin_url%/}/deployments"
  )" || return 2
  printf '%s' "$response" | python3 -c '
import json
import sys

target = sys.argv[1]
normalized_target = target.rstrip("/")

try:
    document = json.load(sys.stdin)
except (json.JSONDecodeError, UnicodeDecodeError):
    raise SystemExit(2)
if not isinstance(document, dict) or not isinstance(document.get("deployments"), list):
    raise SystemExit(2)
for deployment in document["deployments"]:
    if not isinstance(deployment, dict):
        raise SystemExit(2)
    uri = deployment.get("uri")
    if isinstance(uri, str) and uri.rstrip("/") == normalized_target:
        raise SystemExit(0)
raise SystemExit(1)
' "$endpoint_url"
}

require_unused_deployment_uri() {
  local admin_url="$1"
  local endpoint_url="$2"
  local status=0
  deployment_uri_registered "$admin_url" "$endpoint_url" || status=$?
  case "$status" in
    0)
      log "Restate deployment URI is already registered: $endpoint_url"
      return 1
      ;;
    1)
      return 0
      ;;
    *)
      log "could not verify that Restate deployment URI is unused: $endpoint_url"
      return 1
      ;;
  esac
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

new_ownership_token() {
  local token=""
  if [[ -r /proc/sys/kernel/random/uuid ]]; then
    token="$(< /proc/sys/kernel/random/uuid)"
  fi
  [[ "$token" =~ ^[0-9a-fA-F-]{36}$ ]] \
    || die "could not create a launcher ownership token"
  printf '%s\n' "$token"
}

regular_private_file() {
  local file="$1"
  [[ -f "$file" && ! -L "$file" ]] || return 1
  [[ "$(stat -c '%u' "$file" 2>/dev/null || true)" = "$(id -u)" ]] || return 1
  local mode
  mode="$(stat -c '%a' "$file" 2>/dev/null || true)"
  [[ "$mode" =~ ^[0-7]{3,4}$ ]] || return 1
  (( (8#$mode & 0022) == 0 ))
}

private_owned_directory() {
  local directory="$1"
  [[ -d "$directory" && ! -L "$directory" ]] || return 1
  [[ "$(stat -c '%u' "$directory" 2>/dev/null || true)" = "$(id -u)" ]] || return 1
  local mode
  mode="$(stat -c '%a' "$directory" 2>/dev/null || true)"
  [[ "$mode" =~ ^[0-7]{3,4}$ ]] || return 1
  (( (8#$mode & 0022) == 0 ))
}

path_has_symlink_component() {
  local path="$1"
  local absolute
  if [[ "$path" = /* ]]; then
    absolute="$path"
  else
    absolute="$repo_root/$path"
  fi
  local current="/"
  local component
  IFS='/' read -r -a components <<<"${absolute#/}"
  for component in "${components[@]}"; do
    [[ -n "$component" && "$component" != "." ]] || continue
    if [[ "$component" = ".." ]]; then
      current="$(dirname "$current")"
      continue
    fi
    current="${current%/}/$component"
    [[ ! -L "$current" ]] || return 0
  done
  return 1
}

data_path_overlaps_reset_owner() {
  local candidate="$data_dir"
  while [[ "$candidate" != / ]]; do
    [[ -e "$candidate/.agent-workbench-dev-reset-owner" ]] && return 0
    candidate="$(dirname "$candidate")"
  done

  [[ -d "$data_dir" ]] || return 1
  local descendant=""
  descendant="$(
    find -P "$data_dir" -xdev -mindepth 2 \
      -name .agent-workbench-dev-reset-owner -print -quit 2>/dev/null
  )" || return 0
  [[ -n "$descendant" ]]
}

require_exclusive_data_path_for_start() {
  if data_path_overlaps_reset_owner; then
    die "application data path overlaps another launcher-owned disposable stack"
  fi
}

write_container_marker() {
  local file="$1" name="$2" id="$3" component="$4"
  printf '%s %s %s %s\n' "$name" "$id" "$ownership_token" "$component" > "$file"
  chmod 600 "$file"
}

read_container_marker() {
  local file="$1"
  regular_private_file "$file" || return 1
  local name id token component extra
  read -r name id token component extra < "$file" || return 1
  [[ -n "$name" && "$id" =~ ^[0-9a-fA-F]{12,64}$ ]] || return 1
  [[ "$token" =~ ^[0-9a-fA-F-]{36}$ && -n "$component" && -z "$extra" ]] || return 1
  printf '%s %s %s %s\n' "$name" "$id" "$token" "$component"
}

container_identity_matches() {
  local name="$1" expected_id="$2" expected_token="$3" expected_component="$4"
  local actual=""
  actual="$(docker inspect --format '{{.Id}} {{index .Config.Labels "com.lash.agent-workbench.owner"}} {{index .Config.Labels "com.lash.agent-workbench.component"}}' "$name" 2>/dev/null || true)"
  [[ "$actual" = "$expected_id $expected_token $expected_component" ]]
}

stop_owned_container_file() {
  local file="$1" expected_component="$2"
  [[ -e "$file" ]] || return 0
  local record name id token component
  record="$(read_container_marker "$file" 2>/dev/null || true)"
  if [[ -z "$record" ]]; then
    log "refusing to stop $expected_component: ownership marker is legacy or invalid at $file"
    return 1
  fi
  read -r name id token component <<<"$record"
  if [[ "$component" != "$expected_component" ]] \
    || ! container_identity_matches "$name" "$id" "$token" "$component"; then
    log "refusing to stop $expected_component: container identity does not match $file"
    return 1
  fi
  log "stopping $expected_component container $name"
  if ! docker rm -fv "$id" >/dev/null; then
    log "could not remove the exact owned $expected_component container $name"
    return 1
  fi
  if ! rm -f "$file"; then
    log "removed the owned $expected_component container but could not clear its ownership marker at $file"
    return 1
  fi
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
  if [[ -e "$pid_file" ]]; then
    log "removing stale or mismatched PID file $pid_file"
  fi
  rm -f "$pid_file"
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

stop_process_identity() {
  local pid="$1" start_time="$2"
  if ! pid_identity_matches "$pid" "$start_time"; then
    local current_start_time=""
    current_start_time="$(process_start_time "$pid" 2>/dev/null || true)"
    if [[ -z "$current_start_time" || "$current_start_time" != "$start_time" ]]; then
      return 0
    fi
    log "process identity could not be verified; refusing cleanup for PID $pid"
    return 1
  fi
  log "stopping process $pid"
  if ! signal_verified_process TERM "$pid" "$start_time"; then
    if [[ ! -e "/proc/$pid" ]]; then
      return 0
    fi
    log "process identity changed or could not be signaled; refusing cleanup for PID $pid"
    return 1
  fi
  for _ in {1..30}; do
    pid_identity_matches "$pid" "$start_time" || break
    sleep 0.5
  done
  if pid_identity_matches "$pid" "$start_time"; then
    log "process $pid did not exit; sending SIGKILL"
    if ! signal_verified_process KILL "$pid" "$start_time"; then
      log "process identity changed before SIGKILL; refusing to signal PID $pid"
      return 1
    fi
    for _ in {1..30}; do
      pid_identity_matches "$pid" "$start_time" || break
      sleep 0.1
    done
    if pid_identity_matches "$pid" "$start_time"; then
      log "process $pid still exists after SIGKILL; refusing cleanup"
      return 1
    fi
  fi
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

  stop_process_identity "$pid" "$start_time" || return 1

  rm -f "$file" "${file%.pid}.meta"
}

stop_attempt_workbench() {
  if [[ -n "$started_workbench_pid" && -n "$started_workbench_start_time" ]]; then
    stop_process_identity "$started_workbench_pid" "$started_workbench_start_time" \
      || return 1
    rm -f "$pid_file"
    return 0
  fi
  stop_pid_file "$pid_file"
}

stop_started_restate() {
  stop_owned_container_file "$restate_marker_file" restate
}

stop_started_postgres() {
  stop_owned_container_file "$postgres_marker_file" postgres
}

stop_target() {
  stop_pid_file "$pid_file"
  stop_started_restate
  stop_started_postgres
}

remove_attempt_reset_ownership() {
  if [[ "${created_reset_ownership_this_attempt:-0}" = 1 ]]; then
    rm -f "$reset_file" "$data_owner_file"
    created_reset_ownership_this_attempt=0
  fi
}

cleanup_start_attempt() {
  if (( started_workbench_this_attempt )); then
    if ! stop_attempt_workbench; then
      log "startup cleanup could not stop the owned workbench; retaining its engine, application state, and ownership metadata"
      return 1
    fi
    started_workbench_this_attempt=0
  fi
  if (( external_restate_used_this_attempt )); then
    log "startup cleanup cannot retire the external Restate engine; retaining application state, managed stores, and ownership metadata"
    return 1
  fi
  if (( started_restate_this_attempt )); then
    if ! stop_started_restate; then
      log "startup cleanup could not remove the exact owned Restate engine; retaining application state and ownership metadata"
      return 1
    fi
    started_restate_this_attempt=0
  fi
  if (( started_postgres_this_attempt )); then
    if ! stop_started_postgres; then
      log "startup cleanup could not remove the exact owned Postgres store; retaining application state and ownership metadata"
      return 1
    fi
    started_postgres_this_attempt=0
  fi
  if (( data_dir_created_this_attempt )) \
    && [[ "$data_dir" != / && "$data_dir" != "$repo_root" ]] \
    && ! path_has_symlink_component "$configured_data_dir"; then
    if ! rm -rf -- "$data_dir"; then
      log "startup cleanup could not remove the owned application data directory; retaining remaining ownership metadata"
      return 1
    fi
    rm -f "$pid_file" "$meta_file" "$log_file" "$reset_file" \
      "$restate_marker_file" "$postgres_marker_file"
  fi
  remove_attempt_reset_ownership
  rm -f "$pid_file" "$meta_file"
}

cleanup_failed_attempt() {
  local status=$?
  if (( status != 0 && start_attempt_active )); then
    local cleanup_complete=1
    if ! cleanup_start_attempt; then
      log "startup cleanup did not complete; retrying only the same verified attempt resources"
      if ! cleanup_start_attempt; then
        cleanup_complete=0
      fi
    fi
    if (( reset_committed )); then
      write_reset_recovery_file
      log "the disposable dev state was reset, but replacement startup failed"
      if (( cleanup_complete )); then
        log "stack is stopped; recovery command saved at $reset_recovery_file"
      else
        log "replacement cleanup is incomplete; retained its application state and ownership metadata"
        log "recovery command saved at $reset_recovery_file; run it only after the retained attempt is safely retired"
      fi
    elif (( reset_destructive_started )); then
      log "disposable reset started but did not complete; the stack may be partially stopped"
      log "no unverified or external resource was removed"
    elif (( ! cleanup_complete )); then
      log "startup cleanup is incomplete; retained its application state and ownership metadata"
    fi
  fi
  return "$status"
}
trap cleanup_failed_attempt EXIT

build_reset_recovery_command() {
  local postgres_setting=0
  if [[ "$owned_store_backend" = postgres ]]; then
    postgres_setting=1
  fi
  printf -v reset_recovery_command \
    'env AGENT_WORKBENCH_RUN_DIR=%q AGENT_WORKBENCH_DATA_DIR=%q AGENT_WORKBENCH_RESTATE_ADDR=%q AGENT_WORKBENCH_RESTATE_ENDPOINT_URL=%q RESTATE_INGRESS_URL=%q RESTATE_ADMIN_URL=%q AGENT_WORKBENCH_RESTATE_NODE_PORT=%q AGENT_WORKBENCH_RESTATE_CONTAINER=%q AGENT_WORKBENCH_DATABASE_URL=%q AGENT_WORKBENCH_POSTGRES=%q' \
    "$state_dir" "$data_dir" "$restate_endpoint_addr" "$(endpoint_url)" \
    "$restate_ingress_url" "$restate_admin_url" "$restate_node_port" "$restate_container" \
    "" "$postgres_setting"
  if [[ "$owned_store_backend" = postgres ]]; then
    printf -v reset_recovery_command '%s AGENT_WORKBENCH_POSTGRES=1 AGENT_WORKBENCH_POSTGRES_HOST=%q AGENT_WORKBENCH_POSTGRES_PORT=%q AGENT_WORKBENCH_POSTGRES_CONTAINER=%q' \
      "$reset_recovery_command" "$postgres_host" "$postgres_port" "$postgres_container"
  fi
  printf -v reset_recovery_command '%s %q up --addr %q' \
    "$reset_recovery_command" "$repo_root/scripts/agent-workbench-dev.sh" "$workbench_addr"
}

write_reset_recovery_file() {
  {
    printf '#!/usr/bin/env bash\n'
    printf 'set -euo pipefail\n'
    printf 'exec %s\n' "$reset_recovery_command"
  } > "$reset_recovery_file"
  chmod 600 "$reset_recovery_file"
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
    stop_owned_container_file "$file" restate || true
  done
  for file in "$state_dir"/postgres-*.container; do
    [[ -e "$file" ]] || continue
    stop_owned_container_file "$file" postgres || true
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
    external_restate_used_this_attempt=1
    log "using existing Restate at ingress=$restate_ingress_url admin=$restate_admin_url"
    return
  fi

  command -v docker >/dev/null 2>&1 || die "Restate is not running and docker is unavailable"
  if docker inspect "$restate_container" >/dev/null 2>&1; then
    die "Restate container name $restate_container already exists but is not a ready launcher-owned service"
  fi
  log "starting Restate container $restate_container from $restate_image"
  local container_id
  container_id="$(docker run -d \
    --name "$restate_container" \
    --network host \
    --label "com.lash.agent-workbench.owner=$ownership_token" \
    --label 'com.lash.agent-workbench.component=restate' \
    -e RESTATE_INGRESS__BIND_PORT="$ingress_port" \
    -e RESTATE_ADMIN__BIND_PORT="$admin_port" \
    -e RESTATE_BIND_PORT="$restate_node_port" \
    "$restate_image")"
  [[ "$container_id" =~ ^[0-9a-fA-F]{12,64}$ ]] \
    || die "Docker returned an invalid Restate container id"
  write_container_marker "$restate_marker_file" "$restate_container" "$container_id" restate
  started_restate_this_attempt=1

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
  if docker inspect "$postgres_container" >/dev/null 2>&1; then
    die "Postgres container name $postgres_container already exists but is not a ready launcher-owned service"
  fi
  log "starting Postgres container $postgres_container from $postgres_image"
  local container_id
  container_id="$(docker run -d \
    --name "$postgres_container" \
    --network host \
    --label "com.lash.agent-workbench.owner=$ownership_token" \
    --label 'com.lash.agent-workbench.component=postgres' \
    -e POSTGRES_USER=lash \
    -e POSTGRES_PASSWORD=lash \
    -e POSTGRES_DB=lash \
    "$postgres_image" -p "$postgres_port")"
  [[ "$container_id" =~ ^[0-9a-fA-F]{12,64}$ ]] \
    || die "Docker returned an invalid Postgres container id"
  write_container_marker "$postgres_marker_file" "$postgres_container" "$container_id" postgres
  started_postgres_this_attempt=1

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
    printf 'deployment_url=%q\n' "$(endpoint_url)"
    printf 'store_backend=%q\n' "$store_backend"
    printf 'data_dir=%q\n' "$data_dir"
    printf 'database_fingerprint=%q\n' "$database_fingerprint"
    printf 'ownership_token=%q\n' "$ownership_token"
    printf 'postgres_host=%q\n' "$postgres_host"
    printf 'postgres_port=%q\n' "$postgres_port"
    printf 'log_file=%q\n' "$log_file"
  } > "$meta_file"
  chmod 600 "$meta_file"
}

write_reset_metadata() {
  local pid_record restate_record postgres_record=""
  pid_record="$(pid_file_identity "$pid_file")" || return 1
  restate_record="$(read_container_marker "$restate_marker_file")" || return 1
  if [[ "$store_backend" = postgres ]]; then
    postgres_record="$(read_container_marker "$postgres_marker_file")" || return 1
  fi
  {
    printf 'reset_schema=2\n'
    printf 'owned_token=%q\n' "$ownership_token"
    printf 'owned_state_key=%q\n' "$state_key"
    printf 'owned_workbench_addr=%q\n' "$workbench_addr"
    printf 'owned_restate_endpoint_addr=%q\n' "$restate_endpoint_addr"
    printf 'owned_restate_ingress_url=%q\n' "$restate_ingress_url"
    printf 'owned_restate_admin_url=%q\n' "$restate_admin_url"
    printf 'owned_deployment_url=%q\n' "$(endpoint_url)"
    printf 'owned_restate_node_port=%q\n' "$restate_node_port"
    printf 'owned_data_dir=%q\n' "$data_dir"
    printf 'owned_data_identity=%q\n' "$(stat -c '%d:%i' "$data_dir")"
    printf 'owned_store_backend=%q\n' "$store_backend"
    printf 'owned_database_fingerprint=%q\n' "$database_fingerprint"
    printf 'owned_pid_record=%q\n' "$pid_record"
    printf 'owned_restate_record=%q\n' "$restate_record"
    printf 'owned_postgres_record=%q\n' "$postgres_record"
  } > "$reset_file"
  chmod 600 "$reset_file"
  {
    printf 'data_owner_schema=2\n'
    printf 'data_owner_token=%q\n' "$ownership_token"
    printf 'data_owner_state_key=%q\n' "$state_key"
    printf 'data_owner_path=%q\n' "$data_dir"
    printf 'data_owner_state_dir=%q\n' "$state_dir"
  } > "$data_owner_file"
  chmod 600 "$data_owner_file"
}

data_owner_matches() {
  regular_private_file "$data_owner_file" || return 1
  (
    data_owner_schema="" data_owner_token="" data_owner_state_key="" data_owner_path=""
    data_owner_state_dir=""
    # shellcheck disable=SC1090
    source "$data_owner_file"
    [[ "$data_owner_schema" = 2 \
      && "$data_owner_token" = "$owned_token" \
      && "$data_owner_state_key" = "$state_key" \
      && "$data_owner_path" = "$owned_data_dir" \
      && "$data_owner_state_dir" = "$state_dir" ]]
  )
}

prepare_reset_ownership() {
  created_reset_ownership_this_attempt=0
  if (( ! started_restate_this_attempt )); then
    log "reset unavailable: Restate was not created by this launcher run"
    return 0
  fi
  if (( data_dir_existed_before_invocation )); then
    log "reset unavailable: application data directory predated this launcher run"
    return 0
  fi
  if [[ "$store_backend" = postgres ]]; then
    if (( database_url_explicit || ! started_postgres_this_attempt )); then
      log "reset unavailable: Postgres is external or was not created by this launcher run"
      return 0
    fi
  elif [[ "$store_backend" != sqlite ]]; then
    log "reset unavailable: unsupported application store backend $store_backend"
    return 0
  fi
  if path_has_symlink_component "$configured_data_dir"; then
    log "reset unavailable: application data path contains a symlink"
    return 0
  fi
  [[ "$data_dir" != / && "$data_dir" != "$repo_root" ]] \
    || die "refusing unsafe resettable application data directory $data_dir"
  created_reset_ownership_this_attempt=1
  write_reset_metadata \
    || die "could not record complete disposable-stack ownership before registration"
}

validate_run_metadata() {
  regular_private_file "$meta_file" || return 1
  (
    unset workbench_addr workbench_url restate_endpoint_addr restate_ingress_url
    unset restate_admin_url deployment_url store_backend data_dir database_fingerprint ownership_token
    # shellcheck disable=SC1090
    source "$meta_file"
    [[ "$workbench_addr" = "$owned_workbench_addr" \
      && "$restate_endpoint_addr" = "$owned_restate_endpoint_addr" \
      && "$restate_ingress_url" = "$owned_restate_ingress_url" \
      && "$restate_admin_url" = "$owned_restate_admin_url" \
      && "$deployment_url" = "$owned_deployment_url" \
      && "$store_backend" = "$owned_store_backend" \
      && "$data_dir" = "$owned_data_dir" \
      && "$database_fingerprint" = "$owned_database_fingerprint" \
      && "$ownership_token" = "$owned_token" ]]
  )
}

validate_reset_ownership() {
  regular_private_file "$reset_file" \
    || die "reset refused: missing, legacy, or unsafe ownership record $reset_file"
  reset_schema="" owned_token="" owned_state_key="" owned_workbench_addr=""
  owned_restate_endpoint_addr="" owned_restate_ingress_url=""
  owned_restate_admin_url="" owned_deployment_url="" owned_restate_node_port="" owned_data_dir=""
  owned_data_identity=""
  owned_store_backend="" owned_database_fingerprint="" owned_pid_record=""
  owned_restate_record="" owned_postgres_record=""
  # shellcheck disable=SC1090
  source "$reset_file"
  [[ "$reset_schema" = 2 && "$owned_token" =~ ^[0-9a-fA-F-]{36}$ ]] \
    || die "reset refused: invalid ownership record $reset_file"
  [[ "$owned_state_key" = "$state_key" \
    && "$owned_workbench_addr" = "$workbench_addr" \
    && "$owned_restate_endpoint_addr" = "$restate_endpoint_addr" \
    && "$owned_restate_ingress_url" = "$restate_ingress_url" \
    && "$owned_restate_admin_url" = "$restate_admin_url" \
    && "$owned_deployment_url" = "$(endpoint_url)" \
    && "$owned_restate_node_port" = "$restate_node_port" \
    && "$owned_data_dir" = "$data_dir" \
    && "$owned_data_identity" = "$(stat -c '%d:%i' "$data_dir" 2>/dev/null || true)" \
    && "$owned_store_backend" = "$store_backend" \
    && "$owned_database_fingerprint" = "$database_fingerprint" ]] \
    || die "reset refused: current settings do not match the owned disposable stack"
  [[ "$owned_data_dir" != / && "$owned_data_dir" != "$repo_root" ]] \
    || die "reset refused: unsafe application data directory $owned_data_dir"
  path_has_symlink_component "$configured_data_dir" \
    && die "reset refused: application data path contains a symlink"
  [[ "$(realpath -m -- "$configured_data_dir")" = "$owned_data_dir" ]] \
    || die "reset refused: application data path does not resolve to the owned directory"
  data_owner_matches \
    || die "reset refused: application data ownership does not match launcher metadata"
  [[ "$(pid_file_identity "$pid_file" 2>/dev/null || true)" = "$owned_pid_record" ]] \
    || die "reset refused: workbench PID identity is missing or changed"
  validate_run_metadata \
    || die "reset refused: run metadata does not match disposable-stack ownership"

  local name id token component
  read -r name id token component <<<"$owned_restate_record"
  [[ "$name" = "$restate_container" \
    && "$token" = "$owned_token" && "$component" = restate \
    && "$(read_container_marker "$restate_marker_file" 2>/dev/null || true)" = "$owned_restate_record" ]] \
    || die "reset refused: Restate ownership marker does not match"
  container_identity_matches "$name" "$id" "$token" "$component" \
    || die "reset refused: Restate container identity does not match"

  if [[ "$owned_store_backend" = postgres ]]; then
    (( ! database_url_explicit )) \
      || die "reset refused: explicit database URL is external or ambiguous"
    read -r name id token component <<<"$owned_postgres_record"
    [[ "$name" = "$postgres_container" \
      && "$token" = "$owned_token" && "$component" = postgres \
      && "$(read_container_marker "$postgres_marker_file" 2>/dev/null || true)" = "$owned_postgres_record" ]] \
      || die "reset refused: Postgres ownership marker does not match"
    container_identity_matches "$name" "$id" "$token" "$component" \
      || die "reset refused: Postgres container identity does not match"
  elif [[ "$owned_store_backend" != sqlite || -n "$agent_workbench_database_url" ]]; then
    die "reset refused: application database ownership is external or ambiguous"
  fi
}

start_detached() {
  log "building agent-workbench (profile: judged)"
  local -a feature_args=()
  if [[ "${AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO:-}" = "valid-empty-completion" ]]; then
    feature_args=(--features provider-wire-fixtures)
  fi
  cargo build -p agent-workbench --profile judged "${feature_args[@]}"

  # Launch the binary cargo just built: honor CARGO_TARGET_DIR, or a stale
  # binary in the repo-local target/ boots instead of the fresh build.
  local workbench_bin="${CARGO_TARGET_DIR:-$repo_root/target}/judged/agent-workbench"
  local -a workbench_env=(
    "AGENT_WORKBENCH_ADDR=$workbench_addr"
    "AGENT_WORKBENCH_RESTATE_ADDR=$restate_endpoint_addr"
    "AGENT_WORKBENCH_DATABASE_URL=$agent_workbench_database_url"
    "RESTATE_INGRESS_URL=$restate_ingress_url"
    "RESTATE_ADMIN_URL=$restate_admin_url"
    "AGENT_WORKBENCH_DATA_DIR=$data_dir"
  )
  printf '\n[%s] starting agent-workbench at %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$workbench_url" >> "$log_file"
  if command -v setsid >/dev/null 2>&1; then
    (
      exec {launcher_lock_fd}>&-
      exec {launcher_data_lock_fd}>&-
      exec setsid env "${workbench_env[@]}" "$workbench_bin"
    ) >> "$log_file" 2>&1 < /dev/null &
  else
    (
      exec {launcher_lock_fd}>&-
      exec {launcher_data_lock_fd}>&-
      exec nohup env "${workbench_env[@]}" "$workbench_bin"
    ) >> "$log_file" 2>&1 < /dev/null &
  fi
  local pid="$!"
  write_pid_file "$pid_file" "$pid" || die "could not record process identity for $pid"
  started_workbench_pid="$pid"
  started_workbench_start_time="$(process_start_time "$pid")" \
    || die "could not retain process identity for $pid"
  started_workbench_this_attempt=1
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
      cleanup_start_attempt || true
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
      cleanup_start_attempt || true
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
  require_exclusive_data_path_for_start
  mkdir -p "$state_dir"
  start_attempt_active=1
  ensure_restate
  local deployment_url
  deployment_url="$(endpoint_url)"
  if ! require_unused_deployment_uri "$restate_admin_url" "$deployment_url"; then
    cleanup_start_attempt || true
    die "refusing to replace an existing Restate deployment; use restart --reset-dev-state only for a wholly launcher-owned disposable stack"
  fi
  ensure_postgres
  start_detached
  wait_workbench_ready 90
  wait_workbench_endpoint_ready 90
  prepare_reset_ownership
  log "registering Restate deployment $deployment_url"
  if ! register_deployment "$restate_admin_url" "$deployment_url"; then
    tail_log
    cleanup_start_attempt || true
    die "failed to register Restate deployment $deployment_url through $restate_admin_url"
  fi
  require_workbench_alive "before reporting ready"
  start_attempt_active=0
  rm -f "$reset_recovery_file"
  log "ready: $workbench_url"
  open_browser "$workbench_url"
}

run_reset_dev_state() {
  validate_reset_ownership

  build_reset_recovery_command
  reset_destructive_started=1
  start_attempt_active=1
  log "resetting wholly launcher-owned disposable stack at $workbench_addr"

  stop_pid_file "$pid_file"
  stop_owned_container_file "$restate_marker_file" restate
  if [[ "$owned_store_backend" = postgres ]]; then
    stop_owned_container_file "$postgres_marker_file" postgres
  fi

  [[ "$owned_data_identity" = "$(stat -c '%d:%i' "$owned_data_dir" 2>/dev/null || true)" ]] \
    || die "reset stopped before data deletion: application data directory identity changed"
  data_owner_matches \
    || die "reset stopped before data deletion: application ownership marker changed"
  path_has_symlink_component "$configured_data_dir" \
    && die "reset stopped before data deletion: application data path became a symlink"
  [[ "$(realpath -m -- "$configured_data_dir")" = "$owned_data_dir" ]] \
    || die "reset stopped before data deletion: application data path changed"
  rm -rf -- "$owned_data_dir"
  rm -f "$pid_file" "$meta_file" "$log_file" "$reset_file" \
    "$restate_marker_file" "$postgres_marker_file"
  mkdir -p "$state_dir"
  reset_committed=1

  ownership_token="$(new_ownership_token)"
  data_dir_existed_before_invocation=0
  data_dir_created_this_attempt=1
  started_workbench_this_attempt=0
  started_workbench_pid=""
  started_workbench_start_time=""
  started_restate_this_attempt=0
  external_restate_used_this_attempt=0
  started_postgres_this_attempt=0
  created_reset_ownership_this_attempt=0
  log "disposable dev state cleared; starting a fresh stack"
  run_up
  rm -f "$reset_recovery_file"
}

run_foreground() {
  if ! ensure_ports_available; then
    return
  fi
  require_exclusive_data_path_for_start
  mkdir -p "$state_dir"
  start_attempt_active=1
  ensure_restate

  local deployment_url
  deployment_url="$(endpoint_url)"
  if ! require_unused_deployment_uri "$restate_admin_url" "$deployment_url"; then
    cleanup_start_attempt || true
    die "refusing to replace an existing Restate deployment at $deployment_url"
  fi
  ensure_postgres

  local started_pid="" started_start_time=""
  cleanup_foreground() {
    if [[ -n "$started_pid" ]]; then
      if ! stop_process_identity "$started_pid" "$started_start_time"; then
        log "foreground cleanup could not stop the owned workbench; retaining its engine and application state"
        return 1
      fi
      rm -f "$pid_file"
      wait "$started_pid" >/dev/null 2>&1 || true
    fi
    if (( external_restate_used_this_attempt )); then
      log "foreground cleanup cannot retire the external Restate engine; retaining application state, managed stores, and ownership metadata"
      return 1
    fi
    if (( started_restate_this_attempt )); then
      if ! stop_started_restate; then
        log "foreground cleanup could not remove the exact owned Restate engine; retaining application state"
        return 1
      fi
    fi
    if (( started_postgres_this_attempt )); then
      if ! stop_started_postgres; then
        log "foreground cleanup could not remove the exact owned Postgres store; retaining application state"
        return 1
      fi
    fi
    rm -f "$pid_file" "$meta_file"
  }
  trap cleanup_foreground EXIT INT TERM

  log "starting workbench at $workbench_url"
  local -a feature_args=()
  if [[ "${AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO:-}" = "valid-empty-completion" ]]; then
    feature_args=(--features provider-wire-fixtures)
  fi
  local -a workbench_env=(
    "AGENT_WORKBENCH_ADDR=$workbench_addr"
    "AGENT_WORKBENCH_RESTATE_ADDR=$restate_endpoint_addr"
    "AGENT_WORKBENCH_DATABASE_URL=$agent_workbench_database_url"
    "RESTATE_INGRESS_URL=$restate_ingress_url"
    "RESTATE_ADMIN_URL=$restate_admin_url"
    "AGENT_WORKBENCH_DATA_DIR=$data_dir"
  )
  env "${workbench_env[@]}" cargo run -p agent-workbench --profile judged "${feature_args[@]}" &
  started_pid="$!"
  write_pid_file "$pid_file" "$started_pid" || die "could not record process identity for $started_pid"
  started_start_time="$(process_start_time "$started_pid")"
  write_meta

  wait_workbench_ready 90
  wait_workbench_endpoint_ready 90
  log "registering Restate deployment $deployment_url"
  register_deployment "$restate_admin_url" "$deployment_url" \
    || die "failed to register Restate deployment $deployment_url through $restate_admin_url"

  require_workbench_alive "before reporting ready"
  log "ready: $workbench_url"
  open_browser "$workbench_url"
  wait "$started_pid"
  start_attempt_active=0
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
reset_dev_state=0
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
    --reset-dev-state)
      reset_dev_state=1
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

if (( reset_dev_state )) && [[ "$action" != restart ]]; then
  die "--reset-dev-state is valid only with restart"
fi

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
# These bounds are the workbench ports for which every stride-derived managed
# service port remains in 1..65535. Outside them, explicit endpoint settings
# are required so the run metadata can never contain an impossible port.
managed_service_port_min_workbench=2223
managed_service_port_max_workbench=7676
if ((
  workbench_port_number < managed_service_port_min_workbench ||
    workbench_port_number > managed_service_port_max_workbench
)); then
  default_restate_endpoint_port=""
  default_restate_ingress_port=""
  default_restate_admin_port=""
  default_restate_node_port=""
  default_postgres_port=""
else
  port_offset=$(((workbench_port_number - 3030) * managed_service_port_stride))
  default_restate_endpoint_port=$((9081 + port_offset))
  default_restate_ingress_port=$((8080 + port_offset))
  default_restate_admin_port=$((19070 + port_offset))
  default_restate_node_port=$((19071 + port_offset))
  default_postgres_port=$((15432 + port_offset))
fi

restate_endpoint_addr="${AGENT_WORKBENCH_RESTATE_ADDR:-127.0.0.1:$default_restate_endpoint_port}"
restate_ingress_url="${RESTATE_INGRESS_URL:-http://127.0.0.1:$default_restate_ingress_port}"
restate_admin_url="${RESTATE_ADMIN_URL:-http://127.0.0.1:${AGENT_WORKBENCH_RESTATE_ADMIN_PORT:-$default_restate_admin_port}}"
restate_image="${AGENT_WORKBENCH_RESTATE_IMAGE:-restatedev/restate:1.7.0}"
restate_node_port="${AGENT_WORKBENCH_RESTATE_NODE_PORT:-$default_restate_node_port}"
configured_endpoint_url="${AGENT_WORKBENCH_RESTATE_ENDPOINT_URL:-}"
url_is_diagnostic_safe "$restate_ingress_url" \
  || die "RESTATE_INGRESS_URL must not contain credentials, a query, or a fragment"
url_is_diagnostic_safe "$restate_admin_url" \
  || die "RESTATE_ADMIN_URL must not contain credentials, a query, or a fragment"
if [[ -n "$configured_endpoint_url" ]]; then
  url_is_diagnostic_safe "$configured_endpoint_url" \
    || die "AGENT_WORKBENCH_RESTATE_ENDPOINT_URL must not contain credentials, a query, or a fragment"
fi

postgres_requested="${AGENT_WORKBENCH_POSTGRES:-0}"
agent_workbench_database_url="${AGENT_WORKBENCH_DATABASE_URL:-}"
database_url_explicit=0
if [[ -n "$agent_workbench_database_url" ]]; then
  database_url_explicit=1
fi
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
  if [[ "$action" = restart && "$reset_dev_state" = 1 ]]; then
    die "reset refused: explicit database URL is external or ambiguous"
  fi
  read -r postgres_host postgres_port < <(url_host_port "$agent_workbench_database_url")
fi

if ((
  workbench_port_number < managed_service_port_min_workbench ||
    workbench_port_number > managed_service_port_max_workbench
)); then
  managed_service_override_hint="AGENT_WORKBENCH_RESTATE_ADDR, RESTATE_INGRESS_URL, RESTATE_ADMIN_URL (or AGENT_WORKBENCH_RESTATE_ADMIN_PORT), and AGENT_WORKBENCH_RESTATE_NODE_PORT"
  if (( postgres_enabled )) && [[ -z "$agent_workbench_database_url" ]]; then
    managed_service_override_hint+="; also AGENT_WORKBENCH_POSTGRES_PORT"
  fi
  managed_service_overrides_complete=1
  [[ -n "${AGENT_WORKBENCH_RESTATE_ADDR:-}" ]] || managed_service_overrides_complete=0
  [[ -n "${RESTATE_INGRESS_URL:-}" ]] || managed_service_overrides_complete=0
  if [[ -z "${RESTATE_ADMIN_URL:-}" &&
    -z "${AGENT_WORKBENCH_RESTATE_ADMIN_PORT:-}" ]]; then
    managed_service_overrides_complete=0
  fi
  [[ -n "${AGENT_WORKBENCH_RESTATE_NODE_PORT:-}" ]] || managed_service_overrides_complete=0
  if (( postgres_enabled )) && [[ -z "$agent_workbench_database_url" ]] &&
    [[ -z "${AGENT_WORKBENCH_POSTGRES_PORT:-}" ]]; then
    managed_service_overrides_complete=0
  fi
  if (( ! managed_service_overrides_complete )); then
    die "cannot derive managed service ports for workbench port $workbench_port; set $managed_service_override_hint explicitly"
  fi
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
database_fingerprint=""
if [[ -n "$agent_workbench_database_url" ]]; then
  database_fingerprint="$(printf '%s' "$agent_workbench_database_url" | sha256sum | awk '{print $1}')"
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
data_dir="$(realpath -m -- "$configured_data_dir")"
ownership_token="$(new_ownership_token)"

state_key="$(printf '%s' "$workbench_addr" | tr -c 'A-Za-z0-9_.-' '_')"
pid_file="$state_dir/workbench-$state_key.pid"
meta_file="$state_dir/workbench-$state_key.meta"
log_file="$state_dir/workbench-$state_key.log"
restate_marker_file="$state_dir/restate-$state_key.container"
postgres_marker_file="$state_dir/postgres-$state_key.container"
reset_file="$state_dir/reset-$state_key.meta"
data_owner_file="$data_dir/.agent-workbench-dev-reset-owner"
created_reset_ownership_this_attempt=0

case "$action" in
  up|start|foreground|run|restart|down|stop)
    command -v flock >/dev/null 2>&1 || die "flock is required for launcher lifecycle operations"
    launcher_lock_hash="$(printf '%s' "$repo_root" | sha256sum | awk '{print $1}')"
    launcher_lock_root="${XDG_RUNTIME_DIR:-/tmp}/lash-agent-workbench-$UID"
    if [[ ! -e "$launcher_lock_root" ]]; then
      mkdir -m 700 -- "$launcher_lock_root"
    fi
    private_owned_directory "$launcher_lock_root" \
      || die "unsafe launcher lock directory $launcher_lock_root"
    launcher_lock_file="$launcher_lock_root/$launcher_lock_hash.lock"
    launcher_data_lock_file="$launcher_lock_root/data-ownership.lock"
    reset_recovery_file="$launcher_lock_root/$launcher_lock_hash-$state_key-recover.sh"
    if [[ -e "$launcher_lock_file" ]] && ! regular_private_file "$launcher_lock_file"; then
      die "unsafe launcher lock file $launcher_lock_file"
    fi
    exec {launcher_lock_fd}>"$launcher_lock_file"
    chmod 600 "$launcher_lock_file"
    flock -n "$launcher_lock_fd" \
      || die "another launcher lifecycle command is already running for this workbench checkout"
    if [[ -e "$launcher_data_lock_file" ]] && ! regular_private_file "$launcher_data_lock_file"; then
      die "unsafe launcher data lock file $launcher_data_lock_file"
    fi
    exec {launcher_data_lock_fd}>"$launcher_data_lock_file"
    chmod 600 "$launcher_data_lock_file"
    flock -n "$launcher_data_lock_fd" \
      || die "another launcher lifecycle command is updating application data ownership"
    ;;
esac

case "$action" in
  up|start)
    run_up
    ;;
  foreground|run)
    run_foreground
    ;;
  restart)
    if (( ! reset_dev_state )); then
      die "restart cannot replace a replayable deployment; use restart --reset-dev-state only for a wholly launcher-owned disposable stack"
    fi
    run_reset_dev_state
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
