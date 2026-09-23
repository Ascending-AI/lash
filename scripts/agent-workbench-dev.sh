#!/usr/bin/env bash
set -euo pipefail
umask 077

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

configured_data_dir="${AGENT_WORKBENCH_DATA_DIR:-.agent-workbench}"
data_dir_existed_before_invocation=1
data_dir_created_this_attempt=0
data_creation_identity=""
data_creation_receipt_record=""
configured_state_dir="${AGENT_WORKBENCH_RUN_DIR:-.agent-workbench/run}"
state_dir="$(realpath -m -- "$configured_state_dir")"

started_workbench_this_attempt=0
started_workbench_pid=""
workbench_bin=""
started_workbench_start_time=""
started_restate_this_attempt=0
started_restate_name=""
started_restate_id=""
external_restate_used_this_attempt=0
started_postgres_this_attempt=0
started_postgres_name=""
started_postgres_id=""
start_attempt_active=0
reset_committed=0
reset_destructive_started=0
reset_finalization_active=0
reset_finalization_phase=""
# `down` retires a stack's process, engine, managed services, leases and
# receipts but keeps the records that say who owns the application data, so the
# durable state survives. Those records name this very stack, stopped: `up`
# resumes it and `reset` clears it, instead of refusing it as another stack's.
resuming_stopped_stack=0
stopped_stack_token=""
stopped_stack_reset_record=""
stopped_stack_data_owner_record=""
stopped_stack_meta_record=""
reset_stack_already_retired=0
start_finalization_phase=""
reset_recovery_command=""
created_restate_ingress_service_lease_this_attempt=0
created_restate_admin_service_lease_this_attempt=0
created_postgres_service_lease_this_attempt=0
created_run_owner_this_attempt=0
created_meta_this_attempt=0
restate_service_lease_record=""
postgres_service_lease_record=""
run_owner_record=""
registered_deployment_id=""
restate_registry_hash=""
restate_retirement_authorized=0
foreground_cleanup_done=0
foreground_cleanup_status=0
process_observation_uncertain=0
# A non-destructive restart owns only the replacement process. It inherits the
# managed-service facts of the stack it continues so the run metadata it
# rewrites keeps naming the engine, stores and deployment that outlive it.
replacing_process=0
inherited_service_facts=0
inherited_restate_managed=""
inherited_restate_name=""
inherited_restate_id=""
inherited_postgres_managed=""
inherited_postgres_name=""
inherited_postgres_id=""
inherited_retirement_authorized=""
inherited_deployment_id=""
inherited_registry_hash=""

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
  scripts/agent-workbench-dev.sh restart --reset-dev-state [--port PORT | --addr HOST:PORT]
  scripts/agent-workbench-dev.sh status [--port PORT | --addr HOST:PORT]
  scripts/agent-workbench-dev.sh logs [--port PORT | --addr HOST:PORT] [-f]
  scripts/agent-workbench-dev.sh down [--port PORT | --addr HOST:PORT]

Defaults:
  up is detached and idempotent.
  restart without a flag is non-destructive: it replaces only the workbench
  process, at the same address, endpoints, store and RESTATE_AUTHORITY_ID, and
  keeps the Restate engine and its journals, any managed Postgres, the
  registered deployment, and the application data directory. It refuses unless
  this launcher's own run metadata proves it owns a matching stack here, and it
  never re-registers a deployment, so a rebuild that changes the Restate service
  surface needs --reset-dev-state instead. Interrupting it is retryable: rerun
  the same command.
  restart --reset-dev-state is destructive: it replaces a wholly launcher-owned
  disposable stack, including its Restate journals and corresponding application
  data. External, mixed, legacy, or ambiguous ownership is refused before
  anything is stopped.
  down stops the workbench and any Restate or Postgres container it started.
  AGENT_WORKBENCH_POSTGRES=1 starts a port-isolated managed Postgres container
  unless AGENT_WORKBENCH_DATABASE_URL points at an existing database.
  --port PORT binds 127.0.0.1:PORT.
  Managed-service ports use a 10-port stride for each workbench-port step from
  3030 for workbench ports 2223 through 7676. Outside that range, set the
  managed-service endpoint environment variables explicitly.
  Restate ingress/admin and PostgreSQL service hosts must use numeric loopback
  addresses or localhost so managed-service identity remains unambiguous.
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
authority = url.netloc.rsplit("@", 1)[-1]
if authority.startswith("["):
    if "]:" not in authority:
        raise SystemExit(1)
    port_text = authority.rsplit("]:", 1)[1]
else:
    if ":" not in authority:
        raise SystemExit(1)
    port_text = authority.rsplit(":", 1)[1]
if not port_text.isdigit() or port_text != str(port):
    raise SystemExit(1)
print(f"{host} {port}")
' "$1")" || die "expected URL with explicit host and port"
  printf '%s\n' "$parsed"
}

canonical_service_host() {
  local host="$1"
  if [[ "$host" = 127.0.0.1 || "$host" = ::1 ]]; then
    printf 'loopback\n'
    return 0
  fi
  python3 -c '
import ipaddress
import socket
import sys

host = sys.argv[1]
try:
    address = ipaddress.ip_address(host)
except ValueError:
    address = None
if address is not None:
    mapped = getattr(address, "ipv4_mapped", None)
    if address.is_loopback or (mapped is not None and mapped.is_loopback):
        print("loopback")
        raise SystemExit(0)
    raise SystemExit(1)
if host.rstrip(".").lower() != "localhost":
    raise SystemExit(1)
try:
    addresses = [
        ipaddress.ip_address(item[4][0])
        for item in socket.getaddrinfo(host, None, type=socket.SOCK_STREAM)
    ]
except (OSError, ValueError):
    raise SystemExit(1)
if addresses and all(address.is_loopback for address in addresses):
    print("loopback")
else:
    raise SystemExit(1)
' "$host" || die "service host must be a numeric loopback address or localhost"
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
  [[ "$port" = "$port_number" ]] \
    || die "$label port must use canonical decimal notation"
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
  registered_deployment_id="$(printf '%s' "$last_response" | python3 -c '
import json
import re
import sys

try:
    document = json.load(sys.stdin)
except (json.JSONDecodeError, UnicodeDecodeError):
    raise SystemExit(1)
deployment_id = document.get("id") if isinstance(document, dict) else None
if not isinstance(deployment_id, str) or not re.fullmatch(r"dp_[A-Za-z0-9]+", deployment_id):
    raise SystemExit(1)
print(deployment_id)
')" || return 1
  require_workbench_alive "after Restate deployment registration"
}

deployment_registry_records() {
  local admin_url="$1"
  local response
  response="$(
    curl --http2-prior-knowledge -fsS --max-time 5 \
      "${admin_url%/}/deployments"
  )" || return 1
  printf '%s' "$response" | python3 -c '
import json
import re
import sys

try:
    document = json.load(sys.stdin)
except (json.JSONDecodeError, UnicodeDecodeError):
    raise SystemExit(1)
if not isinstance(document, dict) or not isinstance(document.get("deployments"), list):
    raise SystemExit(1)
records = []
for deployment in document["deployments"]:
    if not isinstance(deployment, dict):
        raise SystemExit(1)
    deployment_id = deployment.get("id")
    uri = deployment.get("uri")
    if not isinstance(deployment_id, str) or not re.fullmatch(r"dp_[A-Za-z0-9]+", deployment_id):
        raise SystemExit(1)
    if isinstance(uri, str):
        identity = uri.rstrip("/")
    elif isinstance(deployment.get("arn"), str):
        identity = "<lambda>"
    else:
        raise SystemExit(1)
    records.append((deployment_id, identity))
for deployment_id, uri in sorted(records):
    print(f"{deployment_id}\t{uri}")
'
}

deployment_uri_registered() {
  local admin_url="$1"
  local endpoint_url="$2"
  local response
  response="$(deployment_registry_records "$admin_url")" || return 2
  printf '%s\n' "$response" | awk -F '\t' -v target="${endpoint_url%/}" '
    $2 == target { found = 1 }
    END { exit(found ? 0 : 1) }
  '
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
  [[ "$(process_identity_observation "$1" "$2")" = running ]]
}

process_identity_observation() {
  local pid="${1:-}" expected_start_time="${2:-}" current_start_time=""
  if [[ ! "$pid" =~ ^[0-9]+$ || ! "$expected_start_time" =~ ^[0-9]+$ ]]; then
    printf 'unknown\n'
    return
  fi
  if [[ ! -e "/proc/$pid" ]]; then
    printf 'retired\n'
    return
  fi
  if ! current_start_time="$(process_start_time "$pid" 2>/dev/null)"; then
    if [[ ! -e "/proc/$pid" ]]; then
      printf 'retired\n'
    else
      printf 'unknown\n'
    fi
    return
  fi
  if [[ "$current_start_time" = "$expected_start_time" ]]; then
    printf 'running\n'
  else
    printf 'mismatch\n'
  fi
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
  local file="$1" pid="$2" start_time="$3"
  [[ "$pid" =~ ^[0-9]+$ && "$start_time" =~ ^[0-9]+$ ]] || return 1
  printf '%s %s\n' "$pid" "$start_time" \
    | publish_private_record create "$file"
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
  local owner mode
  read -r owner mode <<< "$(stat -c '%u %a' "$file" 2>/dev/null)" || return 1
  [[ "$owner" = "$EUID" ]] || return 1
  [[ "$mode" =~ ^[0-7]{3,4}$ ]] || return 1
  (( (8#$mode & 0022) == 0 ))
}

publish_private_record() {
  local publication="$1" file="$2"
  [[ "$publication" =~ ^(create|replace)$ ]] || return 1
  python3 -c '
import os
import errno
import secrets
import stat
import sys

publication, path = sys.argv[1:]
data = sys.stdin.buffer.read()
directory = os.path.dirname(path) or "."
name = os.path.basename(path)
if not name or name in {".", ".."}:
    raise SystemExit(1)
directory_fd = os.open(
    directory,
    os.O_RDONLY | os.O_DIRECTORY | getattr(os, "O_NOFOLLOW", 0),
)
temporary = f".{name}.tmp.{secrets.token_hex(16)}"
temporary_created = False
try:
    fd = os.open(
        temporary,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
        0o600,
        dir_fd=directory_fd,
    )
    temporary_created = True
    try:
        view = memoryview(data)
        while view:
            written = os.write(fd, view)
            view = view[written:]
        os.fchmod(fd, 0o600)
        os.fsync(fd)
    finally:
        os.close(fd)
    if publication == "create":
        os.link(
            temporary,
            name,
            src_dir_fd=directory_fd,
            dst_dir_fd=directory_fd,
            follow_symlinks=False,
        )
    else:
        try:
            current = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
        except FileNotFoundError:
            current = None
        if current is not None:
            if not stat.S_ISREG(current.st_mode) or current.st_uid != os.getuid() \
                    or current.st_mode & 0o022:
                raise PermissionError("unsafe destination")
        os.replace(temporary, name, src_dir_fd=directory_fd, dst_dir_fd=directory_fd)
        temporary_created = False
    try:
        os.fsync(directory_fd)
    except OSError as error:
        if error.errno not in {errno.EINVAL, errno.EROFS}:
            raise
finally:
    if temporary_created:
        try:
            os.unlink(temporary, dir_fd=directory_fd)
        except FileNotFoundError:
            pass
    os.close(directory_fd)
' "$publication" "$file"
}

private_owned_directory() {
  local directory="$1"
  [[ -d "$directory" && ! -L "$directory" ]] || return 1
  local owner mode
  read -r owner mode <<< "$(stat -c '%u %a' "$directory" 2>/dev/null)" || return 1
  [[ "$owner" = "$EUID" ]] || return 1
  [[ "$mode" =~ ^[0-7]{3,4}$ ]] || return 1
  (( (8#$mode & 0022) == 0 ))
}

stable_launcher_runtime_root() {
  printf '/tmp/lash-agent-workbench-%s\n' "$UID"
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

path_overlaps_reset_owner() {
  local path="$1"
  local candidate="$path"
  while [[ "$candidate" != / ]]; do
    if [[ -e "$candidate/.agent-workbench-dev-reset-owner" ]]; then
      # The stopped stack this command is resuming owns its own application
      # data: its marker is this stack's, not another stack's.
      if (( ! resuming_stopped_stack )) \
        || [[ "$candidate/.agent-workbench-dev-reset-owner" != "$data_owner_file" ]]; then
        return 0
      fi
    fi
    candidate="$(dirname "$candidate")"
  done

  [[ -d "$path" ]] || return 1
  local descendant=""
  descendant="$(
    find -P "$path" -xdev -mindepth 2 \
      -name .agent-workbench-dev-reset-owner -print -quit 2>/dev/null
  )" || return 0
  [[ -n "$descendant" ]]
}

path_contains_reset_footprint_record() {
  local path="$1"
  [[ -d "$path" ]] || return 1
  local record=""
  local -a own_records=()
  if (( resuming_stopped_stack )); then
    # The run footprint of the stopped stack being resumed is this stack's own.
    own_records=(! -path "$run_owner_file")
  fi
  record="$(
    find -P "$path" -xdev -mindepth 1 ${own_records[@]+"${own_records[@]}"} \
      \( -name '.agent-workbench-dev-run-owner-*' \
      -o -name '.agent-workbench-dev-attempt-owner' \
      -o -name 'workbench-*.process-retired' \
      -o -name 'workbench-*.teardown' \
      -o -name '*-*.service-retired' \
      -o -name 'restate-*.lease' -o -name 'postgres-*.lease' \
      -o -name '*-recover.sh' \) \
      -print -quit 2>/dev/null
  )" || return 0
  [[ -n "$record" ]]
}

path_contains_path() {
  local parent="$1" child="$2"
  [[ "$child" = "$parent" || "$child" = "$parent/"* ]]
}

path_overlaps_reset_finalization() {
  local path="$1" file
  for file in "$launcher_lock_root"/*-reset-finalizing; do
    [[ -e "$file" || -L "$file" ]] || continue
    regular_private_file "$file" || return 0
    if ! (
      reset_finalization_schema="" reset_finalization_phase=""
      owned_data_dir="" owned_state_dir=""
      # shellcheck disable=SC1090
      source "$file"
      [[ "$reset_finalization_schema" = 1 \
        && "$reset_finalization_phase" =~ ^(retired|data-removed)$ \
        && "$owned_data_dir" = /* && "$owned_state_dir" = /* ]]
    ); then
      return 0
    fi
    if (
      reset_finalization_schema="" owned_data_dir="" owned_state_dir=""
      # shellcheck disable=SC1090
      source "$file"
      [[ "$reset_finalization_schema" = 1 \
        && ( "$path" = "$owned_data_dir" || "$path" = "$owned_data_dir/"* \
          || "$owned_data_dir" = "$path/"* \
          || "$path" = "$owned_state_dir" || "$path" = "$owned_state_dir/"* \
          || "$owned_state_dir" = "$path/"* ) ]]
    ); then
      return 0
    fi
  done
  return 1
}

path_overlaps_start_finalization() {
  local path="$1" file
  for file in "$launcher_lock_root"/*-start-finalizing; do
    [[ -e "$file" || -L "$file" ]] || continue
    regular_private_file "$file" || return 0
    if ! (
      start_finalization_schema="" start_finalization_phase=""
      start_finalization_data_dir="" start_finalization_state_dir=""
      start_finalization_data_action=""
      # shellcheck disable=SC1090
      source "$file"
      [[ "$start_finalization_schema" = 2 \
        && "$start_finalization_phase" =~ ^(retired|data-finalized)$ \
        && "$start_finalization_data_action" =~ ^(remove|preserve)$ \
        && "$start_finalization_data_dir" = /* \
        && "$start_finalization_state_dir" = /* ]]
    ); then
      return 0
    fi
    if (
      start_finalization_schema="" start_finalization_data_dir=""
      start_finalization_state_dir=""
      # shellcheck disable=SC1090
      source "$file"
      [[ "$start_finalization_schema" = 2 \
        && ( "$path" = "$start_finalization_data_dir" \
          || "$path" = "$start_finalization_data_dir/"* \
          || "$start_finalization_data_dir" = "$path/"* \
          || "$path" = "$start_finalization_state_dir" \
          || "$path" = "$start_finalization_state_dir/"* \
          || "$start_finalization_state_dir" = "$path/"* ) ]]
    ); then
      return 0
    fi
  done
  return 1
}

# Every live resource a launched stack publishes — its process metadata and
# retirement receipt, its teardown transaction, its container ownership markers,
# its service retirement receipts and its endpoint service leases — proven gone
# for the given ownership token. This is the state `down` leaves: a stack with
# nothing left to stop.
stack_resources_fully_retired() {
  local token="$1" retired="" lease="" record="" lease_token=""
  for retired in "$pid_file" "$process_retirement_receipt_file" \
    "$teardown_transaction_file" "$restate_marker_file" "$postgres_marker_file" \
    "$restate_service_retirement_receipt_file" "$postgres_service_retirement_receipt_file"; do
    [[ ! -e "$retired" && ! -L "$retired" ]] || return 1
  done
  for lease in "$restate_ingress_service_lease_file" "$restate_admin_service_lease_file" \
    "$legacy_restate_service_lease_file" "$postgres_service_lease_file"; do
    record="$(read_service_lease "$lease" 2>/dev/null || true)"
    [[ -n "$record" ]] || continue
    read -r _ _ lease_token _ <<<"$record"
    [[ "$lease_token" != "$token" ]] || return 1
  done
}

# The exact shape `down` leaves behind on a wholly launcher-owned stack: the
# application-data owner marker, the disposable-stack ownership record, the run
# footprint and the run metadata, all naming one ownership token, with every
# live resource of that stack proven gone. Nothing here is a liveness guess —
# each retired resource is proven absent — so a half-torn-down stack, or one
# that has been started again, is not resumable and keeps every refusal.
stopped_stack_records_resumable() {
  stopped_stack_token=""
  local expected_addr="$workbench_addr" expected_data_dir="$data_dir"
  local token=""
  regular_private_file "$data_owner_file" || return 1
  regular_private_file "$reset_file" || return 1
  regular_private_file "$meta_file" || return 1
  token="$(
    data_owner_schema="" data_owner_token="" data_owner_state_key=""
    data_owner_path="" data_owner_state_dir=""
    # shellcheck disable=SC1090
    source "$data_owner_file"
    [[ "$data_owner_schema" = 5 && "$data_owner_token" =~ ^[0-9a-fA-F-]{36}$ \
      && "$data_owner_state_key" = "$state_key" \
      && "$data_owner_path" = "$expected_data_dir" \
      && "$data_owner_state_dir" = "$state_dir" ]] || exit 1
    printf '%s\n' "$data_owner_token"
  )" || return 1
  (
    reset_schema="" owned_token="" owned_state_key="" owned_state_dir=""
    owned_workbench_addr="" owned_data_dir="" owned_data_identity="" owned_run_owner=""
    # shellcheck disable=SC1090
    source "$reset_file"
    [[ "$reset_schema" = 6 && "$owned_token" = "$token" \
      && "$owned_state_key" = "$state_key" \
      && "$owned_state_dir" = "$state_dir" \
      && "$owned_workbench_addr" = "$expected_addr" \
      && "$owned_data_dir" = "$expected_data_dir" \
      && "$owned_run_owner" = "1 $token $state_key $data_path_hash" \
      && "$owned_data_identity" = "$(stat -c '%d:%i' "$expected_data_dir" 2>/dev/null || true)" ]]
  ) || return 1
  (
    meta_schema="" ownership_token=""
    # shellcheck disable=SC1090
    source "$meta_file"
    [[ "$meta_schema" = 3 && "$ownership_token" = "$token" \
      && "$workbench_addr" = "$expected_addr" && "$data_dir" = "$expected_data_dir" ]]
  ) || return 1
  [[ "$(read_run_owner "$run_owner_file" 2>/dev/null || true)" \
    = "1 $token $state_key $data_path_hash" ]] || return 1
  [[ ! -e "$data_creation_receipt_file" && ! -L "$data_creation_receipt_file" ]] || return 1
  stack_resources_fully_retired "$token" || return 1
  private_owned_directory "$data_dir" || return 1
  ! path_has_symlink_component "$configured_data_dir" || return 1
  [[ "$data_dir" != / && "$data_dir" != "$repo_root" ]] || return 1
  stopped_stack_token="$token"
}

stopped_stack_authority_digest() {
  regular_private_file "$meta_file" || return 1
  (
    meta_schema="" restate_authority_digest=""
    # shellcheck disable=SC1090
    source "$meta_file"
    [[ "$meta_schema" = 3 && -n "$restate_authority_digest" ]] || exit 1
    printf '%s\n' "$restate_authority_digest"
  )
}

# Continues the stopped stack's ownership instead of minting a new token: its
# application data, run footprint and ownership records all carry that token,
# and its durable Restate state is bound to one trust domain. The records this
# resume inherits are never destroyed by a failed attempt — cleanup restores
# them, so the same `up` is always retryable.
adopt_stopped_stack() {
  (( resuming_stopped_stack )) || return 0
  local recorded_authority=""
  recorded_authority="$(stopped_stack_authority_digest)" \
    || die "up refused: the stopped stack at $workbench_addr has unreadable run metadata; discard it with scripts/agent-workbench-dev.sh restart --reset-dev-state --addr $workbench_addr"
  [[ "$recorded_authority" = "$(current_restate_authority_digest)" ]] \
    || die "up refused: RESTATE_AUTHORITY_ID does not match the durable trust domain the stopped stack at $workbench_addr is bound to; export the original value and run the same up again, or discard that durable state with scripts/agent-workbench-dev.sh restart --reset-dev-state --addr $workbench_addr"
  stopped_stack_reset_record="$(cat -- "$reset_file")" \
    || die "up refused: could not read the stopped stack's ownership record $reset_file"
  stopped_stack_data_owner_record="$(cat -- "$data_owner_file")" \
    || die "up refused: could not read the stopped stack's application data ownership record $data_owner_file"
  stopped_stack_meta_record="$(cat -- "$meta_file")" \
    || die "up refused: could not read the stopped stack's run metadata $meta_file"
  ownership_token="$stopped_stack_token"
  log "resuming the stopped disposable stack at $workbench_addr; its application data at $data_dir and its durable state are retained"
}

require_exclusive_data_path_for_start() {
  if [[ -e "$start_finalization_file" || -L "$start_finalization_file" ]]; then
    die "requested workbench identity has a startup cleanup awaiting finalization"
  fi
  if path_overlaps_reset_finalization "$data_dir" \
    || path_overlaps_reset_finalization "$state_dir"; then
    die "application or run path overlaps a reset awaiting finalization"
  fi
  if path_overlaps_start_finalization "$data_dir" \
    || path_overlaps_start_finalization "$state_dir"; then
    die "application or run path overlaps a startup cleanup awaiting finalization"
  fi
  if stopped_stack_records_resumable; then
    resuming_stopped_stack=1
  fi
  if path_overlaps_reset_owner "$data_dir"; then
    die "application data path overlaps another launcher-owned disposable stack; stop that stack with scripts/agent-workbench-dev.sh down --addr <its address>, discard it with scripts/agent-workbench-dev.sh restart --reset-dev-state --addr <its address>, or point AGENT_WORKBENCH_DATA_DIR at a path this stack owns"
  fi
  if path_contains_reset_footprint_record "$data_dir"; then
    die "application data path encloses another launcher-owned reset footprint; discard that stack with scripts/agent-workbench-dev.sh restart --reset-dev-state --addr <its address>, or point AGENT_WORKBENCH_DATA_DIR at a path this stack owns"
  fi
  if path_contains_path "$data_dir" "$launcher_lock_root"; then
    die "application data path encloses launcher private runtime state"
  fi
  if path_overlaps_reset_owner "$state_dir"; then
    die "launcher run path overlaps another launcher-owned disposable stack; discard that stack with scripts/agent-workbench-dev.sh restart --reset-dev-state --addr <its address>, or point AGENT_WORKBENCH_RUN_DIR at a path this stack owns"
  fi
  if path_overlaps_reset_owner "$launcher_lock_root"; then
    die "launcher private runtime path overlaps another launcher-owned disposable stack"
  fi
}

read_data_creation_receipt() {
  local file="$1"
  regular_private_file "$file" || return 1
  local schema token path_hash identity extra
  read -r schema token path_hash identity extra < "$file" || return 1
  [[ "$schema" = 1 && "$token" =~ ^[0-9a-fA-F-]{36}$ \
    && "$path_hash" = "$data_path_hash" && "$identity" =~ ^[0-9]+:[0-9]+$ \
    && -z "$extra" ]] || return 1
  printf '%s %s %s %s\n' "$schema" "$token" "$path_hash" "$identity"
}

claim_data_directory() {
  data_dir_existed_before_invocation=1
  data_dir_created_this_attempt=0
  data_creation_identity=""
  data_creation_receipt_record=""
  if [[ -e "$configured_data_dir" || -L "$configured_data_dir" ]]; then
    return 0
  fi
  path_has_symlink_component "$configured_data_dir" \
    && die "application data path contains a symlink"
  mkdir -p -- "$(dirname "$data_dir")"
  if ! mkdir -m 700 -- "$data_dir"; then
    die "application data path changed during launcher admission"
  fi
  data_creation_identity="$(stat -c '%d:%i' "$data_dir")" \
    || die "could not capture the created application data directory identity"
  data_creation_receipt_record="1 $ownership_token $data_path_hash $data_creation_identity"
  [[ ! -e "$data_creation_receipt_file" && ! -L "$data_creation_receipt_file" ]] \
    || die "application data directory creation metadata already exists"
  (set -C; printf '%s\n' "$data_creation_receipt_record" > "$data_creation_receipt_file") \
    || die "could not record exclusive application data directory creation"
  chmod 600 "$data_creation_receipt_file" \
    || die "could not protect application data directory creation metadata"
  data_dir_existed_before_invocation=0
  data_dir_created_this_attempt=1
}

data_creation_receipt_matches() {
  (( data_dir_created_this_attempt )) || return 1
  [[ "$data_dir" != / && "$data_dir" != "$repo_root" \
    && "$data_creation_identity" = "$(stat -c '%d:%i' "$data_dir" 2>/dev/null || true)" \
    && "$(read_data_creation_receipt "$data_creation_receipt_file" 2>/dev/null || true)" \
      = "$data_creation_receipt_record" ]]
}

release_data_creation_receipt() {
  (( data_dir_created_this_attempt )) || return 0
  data_creation_receipt_matches || return 1
  rm -f "$data_creation_receipt_file" || return 1
  [[ ! -e "$data_creation_receipt_file" && ! -L "$data_creation_receipt_file" ]] || return 1
  data_dir_created_this_attempt=0
}

read_service_lease() {
  local file="$1"
  regular_private_file "$file" || return 1
  local schema component token id extra
  read -r schema component token id extra < "$file" || return 1
  [[ "$schema" = 1 && "$component" =~ ^(restate|postgres)$ \
    && "$token" =~ ^[0-9a-fA-F-]{36}$ \
    && "$id" =~ ^[0-9a-fA-F]{12,64}$ && -z "$extra" ]] || return 1
  printf '%s %s %s %s\n' "$schema" "$component" "$token" "$id"
}

read_run_owner() {
  local file="$1"
  regular_private_file "$file" || return 1
  local schema token key data_hash extra
  read -r schema token key data_hash extra < "$file" || return 1
  [[ "$schema" = 1 && "$token" =~ ^[0-9a-fA-F-]{36}$ \
    && -n "$key" && "$data_hash" =~ ^[0-9a-f]{64}$ && -z "$extra" ]] || return 1
  printf '%s %s %s %s\n' "$schema" "$token" "$key" "$data_hash"
}

write_run_owner() {
  [[ ! -e "$run_owner_file" && ! -L "$run_owner_file" ]] || return 1
  printf '1 %s %s %s\n' "$ownership_token" "$state_key" "$data_path_hash" \
    | publish_private_record create "$run_owner_file"
}

claim_run_footprint() {
  run_owner_record="1 $ownership_token $state_key $data_path_hash"
  if (( resuming_stopped_stack )) \
    && [[ "$(read_run_owner "$run_owner_file" 2>/dev/null || true)" = "$run_owner_record" ]]; then
    # The resumed stack's own footprint, written by the run this one continues.
    # It predates this attempt, so a failed attempt must leave it in place.
    created_run_owner_this_attempt=0
    return 0
  fi
  write_run_owner || die "launcher run path is already reserved by another workbench stack"
  created_run_owner_this_attempt=1
}

write_service_lease() {
  local file="$1" component="$2" id="$3"
  [[ ! -e "$file" && ! -L "$file" ]] || return 1
  printf '1 %s %s %s\n' "$component" "$ownership_token" "$id" \
    | publish_private_record create "$file"
}

publish_attempt_service_lease() {
  local file="$1" component="$2" id="$3" flag_name="$4"
  local expected="1 $component $ownership_token $id"
  if write_service_lease "$file" "$component" "$id"; then
    printf -v "$flag_name" '%s' 1
    return 0
  fi
  if [[ "$(read_service_lease "$file" 2>/dev/null || true)" = "$expected" ]]; then
    # Publication can report an error after the atomic destination became visible.
    # Retain the exact observable claim so cleanup cannot strand it.
    printf -v "$flag_name" '%s' 1
  fi
  return 1
}

remove_service_lease() {
  local file="$1" expected="$2"
  [[ "$(read_service_lease "$file" 2>/dev/null || true)" = "$expected" ]] \
    || return 1
  rm -f "$file" || return 1
  [[ ! -e "$file" && ! -L "$file" ]]
}

restate_service_leases_match() {
  local expected="$1" file
  for file in "$restate_ingress_service_lease_file" "$restate_admin_service_lease_file"; do
    [[ "$(read_service_lease "$file" 2>/dev/null || true)" = "$expected" ]] || return 1
  done
}

remove_attempt_service_lease() {
  local file="$1" expected="$2" flag_name="$3"
  local -n outstanding="$flag_name"
  (( outstanding )) || return 0
  if [[ ! -e "$file" && ! -L "$file" ]]; then
    outstanding=0
    return 0
  fi
  remove_service_lease "$file" "$expected" || return 1
  outstanding=0
}

remove_attempt_restate_service_leases() {
  local expected="$1" failed=0
  remove_attempt_service_lease "$restate_ingress_service_lease_file" "$expected" \
    created_restate_ingress_service_lease_this_attempt || failed=1
  remove_attempt_service_lease "$restate_admin_service_lease_file" "$expected" \
    created_restate_admin_service_lease_this_attempt || failed=1
  (( ! failed ))
}

require_service_unreserved() {
  local file="$1" component="$2"
  [[ ! -e "$file" && ! -L "$file" ]] \
    || die "$component service is reserved by another launcher-owned disposable stack"
}

write_container_marker() {
  local file="$1" name="$2" id="$3" component="$4"
  [[ ! -e "$file" && ! -L "$file" ]] || return 1
  printf '%s %s %s %s\n' "$name" "$id" "$ownership_token" "$component" \
    | publish_private_record create "$file"
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

container_identity_observation() {
  local reference="$1" expected_id="$2" expected_token="$3" expected_component="$4"
  local actual="" status=0
  actual="$(docker inspect --format '{{.Id}} {{index .Config.Labels "com.lash.agent-workbench.owner"}} {{index .Config.Labels "com.lash.agent-workbench.component"}}' "$reference" 2>&1)" \
    || status=$?
  if (( status == 0 )); then
    if [[ "$actual" = "$expected_id $expected_token $expected_component" ]]; then
      printf 'running\n'
    else
      printf 'mismatch\n'
    fi
  elif [[ "$actual" =~ [Nn]o[[:space:]]such[[:space:]](object|container) ]]; then
    printf 'retired\n'
  else
    printf 'unknown\n'
  fi
}

stop_owned_container_file() {
  local file="$1" expected_component="$2"
  if [[ ! -e "$file" && ! -L "$file" ]]; then
    log "refusing to stop $expected_component: ownership marker is missing at $file"
    return 1
  fi
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
  [[ ! -e "$file" && ! -L "$file" ]] || return 1
}

stop_captured_container() {
  local name="$1" id="$2" token="$3" component="$4" marker_file="$5"
  if [[ -z "$name" || ! "$id" =~ ^[0-9a-fA-F]{12,64}$ \
    || ! "$token" =~ ^[0-9a-fA-F-]{36}$ ]]; then
    log "refusing to stop $component: captured container identity is incomplete"
    return 1
  fi
  if ! container_identity_matches "$name" "$id" "$token" "$component"; then
    log "refusing to stop $component: captured container identity no longer matches"
    return 1
  fi
  log "stopping $component container $name"
  if ! docker rm -fv "$id" >/dev/null; then
    log "could not remove the exact owned $component container $name"
    return 1
  fi
  if [[ -e "$marker_file" || -L "$marker_file" ]]; then
    if [[ "$(read_container_marker "$marker_file" 2>/dev/null || true)" \
      != "$name $id $token $component" ]]; then
      log "removed the exact owned $component container but retained changed ownership metadata at $marker_file"
      return 1
    fi
    rm -f "$marker_file" || {
      log "removed the exact owned $component container but could not clear its ownership marker at $marker_file"
      return 1
    }
    [[ ! -e "$marker_file" && ! -L "$marker_file" ]] || return 1
  fi
}

remove_stale_pid_file() {
  local file="$1"
  if [[ -e "$file" ]]; then
    log "removing stale or mismatched PID file $file"
  fi
  rm -f "$file" || return 1
  [[ ! -e "$file" && ! -L "$file" ]]
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
  local record="" pid="" start_time="" observation="unknown"
  record="$(read_pid_file "$pid_file" 2>/dev/null || true)"
  if [[ -z "$record" ]]; then
    tail_log
    die "workbench process metadata is missing or invalid $phase"
  fi
  read -r pid start_time <<<"$record"
  if [[ -n "$started_workbench_pid" \
    && "$record" != "$started_workbench_pid $started_workbench_start_time" ]]; then
    tail_log
    die "workbench process metadata changed from its captured launch identity $phase"
  fi
  observation="$(process_identity_observation "$pid" "$start_time")"
  [[ "$observation" = running ]] && return 0

  local exit_status="unknown"
  if [[ "$observation" = retired ]]; then
    if wait "$pid" 2>/dev/null; then
      exit_status=0
    else
      exit_status=$?
    fi
  fi
  tail_log
  case "$observation" in
    retired) die "workbench process $pid exited with status $exit_status $phase" ;;
    mismatch) die "workbench process identity changed $phase" ;;
    *)
      process_observation_uncertain=1
      die "workbench process identity could not be observed $phase"
      ;;
  esac
}

workbench_ready() {
  curl -fsS --max-time 2 "$workbench_url/healthz" 2>/dev/null \
    | grep -q '"service":"agent-workbench"'
}

cleanup_stale_pid() {
  [[ -e "$pid_file" ]] || return 0
  local record="" pid="" start_time="" observation=""
  record="$(read_pid_file "$pid_file" 2>/dev/null || true)"
  if [[ -z "$record" ]]; then
    remove_stale_pid_file "$pid_file"
    return
  fi
  read -r pid start_time <<<"$record"
  observation="$(process_identity_observation "$pid" "$start_time")"
  case "$observation" in
    running) return 0 ;;
    retired|mismatch) remove_stale_pid_file "$pid_file" ;;
    *) die "workbench process identity could not be observed; retaining PID metadata" ;;
  esac
}

stop_process_identity() {
  local pid="$1" start_time="$2" observation=""
  observation="$(process_identity_observation "$pid" "$start_time")"
  case "$observation" in
    retired|mismatch) return 0 ;;
    running) ;;
    *)
      log "process identity could not be observed; refusing cleanup for PID $pid"
      return 1
      ;;
  esac
  log "stopping process $pid"
  if ! signal_verified_process TERM "$pid" "$start_time"; then
    observation="$(process_identity_observation "$pid" "$start_time")"
    case "$observation" in
      retired|mismatch) return 0 ;;
      *)
        log "process identity changed or could not be signaled; refusing cleanup for PID $pid"
        return 1
        ;;
    esac
  fi
  for _ in {1..30}; do
    observation="$(process_identity_observation "$pid" "$start_time")"
    [[ "$observation" = running ]] || break
    sleep 0.5
  done
  observation="$(process_identity_observation "$pid" "$start_time")"
  if [[ "$observation" = unknown ]]; then
    log "process retirement could not be observed; refusing dependent cleanup for PID $pid"
    return 1
  fi
  if [[ "$observation" = running ]]; then
    log "process $pid did not exit; sending SIGKILL"
    if ! signal_verified_process KILL "$pid" "$start_time"; then
      log "process identity changed before SIGKILL; refusing to signal PID $pid"
      return 1
    fi
    for _ in {1..30}; do
      observation="$(process_identity_observation "$pid" "$start_time")"
      [[ "$observation" = running ]] || break
      sleep 0.1
    done
    observation="$(process_identity_observation "$pid" "$start_time")"
    if [[ "$observation" = running ]]; then
      log "process $pid still exists after SIGKILL; refusing cleanup"
      return 1
    fi
  fi
  if [[ "$observation" = unknown ]]; then
    log "process retirement could not be observed; refusing dependent cleanup for PID $pid"
    return 1
  fi
}

stop_pid_file() {
  local file="$1"
  [[ -e "$file" ]] || return 0
  local record="" pid="" start_time="" observation=""
  record="$(read_pid_file "$file" 2>/dev/null || true)"
  if [[ -z "$record" ]]; then
    remove_stale_pid_file "$file"
    return
  fi
  read -r pid start_time <<<"$record"
  observation="$(process_identity_observation "$pid" "$start_time")"
  if [[ "$observation" = unknown ]]; then
    log "process identity could not be observed; retaining PID metadata at $file"
    return 1
  fi
  if [[ "$observation" = running ]]; then
    stop_process_identity "$pid" "$start_time" || return 1
  elif [[ "$observation" = mismatch ]]; then
    log "removing stale or mismatched PID file $file"
  fi

  rm -f "$file" || return 1
  [[ ! -e "$file" && ! -L "$file" ]]
}

read_process_retirement_receipt() {
  local file="$1"
  regular_private_file "$file" || return 1
  local schema phase token pid start_time extra
  read -r schema phase token pid start_time extra < "$file" || return 1
  [[ "$schema" = 2 && "$phase" =~ ^(prepared|retired)$ \
    && "$token" =~ ^[0-9a-fA-F-]{36}$ \
    && "$pid" =~ ^[0-9]+$ && "$start_time" =~ ^[0-9]+$ && -z "$extra" ]] \
    || return 1
  printf '%s %s %s %s %s\n' "$schema" "$phase" "$token" "$pid" "$start_time"
}

write_process_retirement_receipt() {
  local file="$1" phase="$2" token="$3" pid="$4" start_time="$5"
  local expected="2 $phase $token $pid $start_time" existing=""
  [[ "$phase" =~ ^(prepared|retired)$ ]] || return 1
  if [[ -e "$file" || -L "$file" ]]; then
    existing="$(read_process_retirement_receipt "$file" 2>/dev/null || true)"
    [[ -n "$existing" ]] || return 1
    if [[ "$existing" = "$expected" ]]; then
      return 0
    fi
    if [[ "$phase" = retired && "$existing" = "2 prepared $token $pid $start_time" ]]; then
      printf '%s\n' "$expected" | publish_private_record replace "$file" \
        || [[ "$(read_process_retirement_receipt "$file" 2>/dev/null || true)" = "$expected" ]]
      return
    fi
    return 1
  fi
  [[ "$phase" = prepared ]] || return 1
  printf '%s\n' "$expected" | publish_private_record create "$file" \
    || [[ "$(read_process_retirement_receipt "$file" 2>/dev/null || true)" = "$expected" ]]
}

validate_persisted_process_state() {
  local pid_file="$1" receipt_file="$2" token="$3" expected_pid="$4" expected_start_time="$5"
  local record="" receipt="" pid="" start_time="" observation=""
  record="$(read_pid_file "$pid_file" 2>/dev/null || true)"
  if [[ -z "$record" ]]; then
    log "refusing teardown: workbench process metadata is missing or invalid at $pid_file"
    return 1
  fi
  if [[ "$record" != "$expected_pid $expected_start_time" ]]; then
    log "refusing teardown: workbench PID belongs to a different process incarnation or does not match its original launch identity"
    return 1
  fi
  read -r pid start_time <<<"$record"
  if [[ -e "$receipt_file" || -L "$receipt_file" ]]; then
    receipt="$(read_process_retirement_receipt "$receipt_file" 2>/dev/null || true)"
    if [[ "$receipt" != "2 prepared $token $pid $start_time" \
      && "$receipt" != "2 retired $token $pid $start_time" ]]; then
      log "refusing teardown: process retirement receipt is invalid or changed at $receipt_file"
      return 1
    fi
    return 0
  fi
  observation="$(process_identity_observation "$pid" "$start_time")"
  case "$observation" in
    running|retired) return 0 ;;
    mismatch)
      log "refusing teardown: workbench PID belongs to a different process incarnation"
      return 1
      ;;
    *)
      log "refusing teardown: workbench process identity could not be observed"
      return 1
      ;;
  esac
}

retire_persisted_process() {
  local pid_file="$1" receipt_file="$2" token="$3" expected_pid="$4" expected_start_time="$5"
  local record="" receipt="" pid="" start_time="" observation=""
  record="$(read_pid_file "$pid_file" 2>/dev/null || true)"
  [[ "$record" = "$expected_pid $expected_start_time" ]] || return 1
  read -r pid start_time <<<"$record"
  receipt="$(read_process_retirement_receipt "$receipt_file" 2>/dev/null || true)"
  if [[ -n "$receipt" ]]; then
    [[ "$receipt" = "2 prepared $token $pid $start_time" \
      || "$receipt" = "2 retired $token $pid $start_time" ]] || return 1
    [[ "$receipt" = "2 retired $token $pid $start_time" ]] && return 0
  else
    observation="$(process_identity_observation "$pid" "$start_time")"
    case "$observation" in
      running|retired) ;;
      mismatch)
        log "workbench process identity changed before retirement began"
        return 1
        ;;
      *)
        log "workbench process retirement could not be observed"
        return 1
        ;;
    esac
    write_process_retirement_receipt \
      "$receipt_file" prepared "$token" "$pid" "$start_time" || true
    if [[ "$(read_process_retirement_receipt "$receipt_file" 2>/dev/null || true)" \
      != "2 prepared $token $pid $start_time" ]]; then
      log "could not persist the prepared workbench retirement receipt"
      return 1
    fi
  fi
  observation="$(process_identity_observation "$pid" "$start_time")"
  case "$observation" in
    running) stop_process_identity "$pid" "$start_time" || return 1 ;;
    retired) ;;
    mismatch)
      [[ -n "$receipt" ]] || {
        log "workbench process identity changed before retirement began"
        return 1
      }
      ;;
    *)
      log "workbench process retirement could not be proven; retaining dependent services"
      return 1
      ;;
  esac
  write_process_retirement_receipt "$receipt_file" retired "$token" "$pid" "$start_time" || true
  if [[ "$(read_process_retirement_receipt "$receipt_file" 2>/dev/null || true)" \
    != "2 retired $token $pid $start_time" ]]; then
    log "could not persist the verified workbench process retirement receipt"
    return 1
  fi
}

stop_attempt_workbench() {
  if [[ -n "$started_workbench_pid" ]]; then
    if [[ -z "$started_workbench_start_time" ]]; then
      log "startup cleanup cannot verify its captured workbench process identity"
      return 1
    fi
    stop_process_identity "$started_workbench_pid" "$started_workbench_start_time" \
      || return 1
    local published_record=""
    published_record="$(read_pid_file "$pid_file" 2>/dev/null || true)"
    if [[ "$published_record" = "$started_workbench_pid $started_workbench_start_time" ]]; then
      rm -f "$pid_file" || return 1
      [[ ! -e "$pid_file" && ! -L "$pid_file" ]] || return 1
    elif [[ -e "$pid_file" || -L "$pid_file" ]]; then
      log "stopped the captured workbench process but retained changed PID metadata at $pid_file"
    fi
    return 0
  fi
  stop_pid_file "$pid_file"
}

stop_started_restate() {
  stop_captured_container "$started_restate_name" "$started_restate_id" \
    "$ownership_token" restate "$restate_marker_file"
}

stop_started_postgres() {
  stop_captured_container "$started_postgres_name" "$started_postgres_id" \
    "$ownership_token" postgres "$postgres_marker_file"
}

retire_started_service_with_receipt() {
  local marker_file="$1" receipt_file="$2" component="$3" name="$4" id="$5"
  local marker_expected="$name $id $ownership_token $component" receipt observation
  if [[ -e "$marker_file" || -L "$marker_file" ]]; then
    [[ "$(read_container_marker "$marker_file" 2>/dev/null || true)" = "$marker_expected" ]] \
      || return 1
  fi
  receipt="$(read_service_retirement_receipt "$receipt_file" 2>/dev/null || true)"
  if [[ -z "$receipt" ]]; then
    container_identity_matches "$name" "$id" "$ownership_token" "$component" || return 1
    write_service_retirement_receipt \
      "$receipt_file" prepared "$component" "$ownership_token" "$id" || true
    receipt="$(read_service_retirement_receipt "$receipt_file" 2>/dev/null || true)"
  fi
  if [[ "$receipt" = "2 retired $component $ownership_token $id" ]]; then
    return 0
  fi
  [[ "$receipt" = "2 prepared $component $ownership_token $id" ]] || return 1
  observation="$(container_identity_observation "$id" "$id" "$ownership_token" "$component")"
  case "$observation" in
    running)
      log "stopping $component container $name"
      docker rm -fv "$id" >/dev/null || return 1
      ;;
    retired) ;;
    *) return 1 ;;
  esac
  write_service_retirement_receipt \
    "$receipt_file" retired "$component" "$ownership_token" "$id" || true
  [[ "$(read_service_retirement_receipt "$receipt_file" 2>/dev/null || true)" \
    = "2 retired $component $ownership_token $id" ]]
}

finalize_attempt_service_records() {
  local marker_file="$1" receipt_file="$2" component="$3" name="$4" id="$5"
  local marker_expected="$name $id $ownership_token $component"
  local receipt_expected="2 retired $component $ownership_token $id"
  remove_exact_private_record "$marker_file" "$marker_expected" \
    read_container_marker "$component ownership marker" || return 1
  remove_exact_private_record "$receipt_file" "$receipt_expected" \
    read_service_retirement_receipt "$component retirement receipt"
}

validate_persisted_service() {
  local marker_file="$1" component="$2" lease_file="$3" expected_token="$4"
  local record name id token marker_component expected_lease
  record="$(read_container_marker "$marker_file" 2>/dev/null || true)"
  if [[ -z "$record" ]]; then
    log "refusing teardown: $component ownership marker is missing, legacy, or invalid at $marker_file"
    return 1
  fi
  read -r name id token marker_component <<<"$record"
  expected_lease="1 $component $token $id"
  if [[ "$marker_component" != "$component" || "$token" != "$expected_token" \
    || "$(read_service_lease "$lease_file" 2>/dev/null || true)" != "$expected_lease" ]] \
    || ! container_identity_matches "$name" "$id" "$token" "$component"; then
    log "refusing teardown: $component identity or service lease does not prove ownership"
    return 1
  fi
}

read_service_retirement_receipt() {
  local file="$1"
  regular_private_file "$file" || return 1
  local schema phase component token id extra
  read -r schema phase component token id extra < "$file" || return 1
  [[ "$schema" = 2 && "$phase" =~ ^(prepared|retired)$ \
    && "$component" =~ ^(restate|postgres)$ \
    && "$token" =~ ^[0-9a-fA-F-]{36}$ \
    && "$id" =~ ^[0-9a-fA-F]{12,64}$ && -z "$extra" ]] || return 1
  printf '%s %s %s %s %s\n' "$schema" "$phase" "$component" "$token" "$id"
}

write_service_retirement_receipt() {
  local file="$1" phase="$2" component="$3" token="$4" id="$5"
  local expected="2 $phase $component $token $id" existing=""
  [[ "$phase" =~ ^(prepared|retired)$ ]] || return 1
  if [[ -e "$file" || -L "$file" ]]; then
    existing="$(read_service_retirement_receipt "$file" 2>/dev/null || true)"
    [[ -n "$existing" ]] || return 1
    if [[ "$existing" = "$expected" ]]; then
      return 0
    fi
    if [[ "$phase" = retired && "$existing" = "2 prepared $component $token $id" ]]; then
      printf '%s\n' "$expected" | publish_private_record replace "$file" \
        || [[ "$(read_service_retirement_receipt "$file" 2>/dev/null || true)" = "$expected" ]]
      return
    fi
    return 1
  fi
  [[ "$phase" = prepared ]] || return 1
  printf '%s\n' "$expected" | publish_private_record create "$file" \
    || [[ "$(read_service_retirement_receipt "$file" 2>/dev/null || true)" = "$expected" ]]
}

validate_persisted_service_state() {
  local marker_file="$1" component="$2" lease_file="$3" expected_token="$4"
  local receipt_file="$5" receipt="" record="" name="" id="" token="" marker_component=""
  if [[ ! -e "$receipt_file" && ! -L "$receipt_file" ]]; then
    validate_persisted_service "$marker_file" "$component" "$lease_file" "$expected_token"
    return
  fi
  receipt="$(read_service_retirement_receipt "$receipt_file" 2>/dev/null || true)"
  local phase=""
  read -r _ phase marker_component token id <<<"$receipt"
  if [[ -z "$receipt" || "$marker_component" != "$component" || "$token" != "$expected_token" ]]; then
    log "refusing teardown: $component retirement receipt is invalid or changed"
    return 1
  fi
  if [[ -e "$marker_file" || -L "$marker_file" ]]; then
    record="$(read_container_marker "$marker_file" 2>/dev/null || true)"
    read -r name _ _ _ <<<"$record"
    if [[ -z "$record" || "$record" != "$name $id $token $component" ]]; then
      log "refusing teardown: $component marker changed after verified retirement"
      return 1
    fi
  fi
  if [[ -e "$lease_file" || -L "$lease_file" ]]; then
    [[ "$(read_service_lease "$lease_file" 2>/dev/null || true)" \
      = "1 $component $token $id" ]] || {
      log "refusing teardown: $component lease changed after verified retirement"
      return 1
    }
  elif [[ "$phase" = prepared ]] \
    && [[ "$(container_identity_observation "$id" "$id" "$token" "$component")" != retired ]]; then
    log "refusing teardown: $component lease disappeared before verified retirement"
    return 1
  fi
}

validate_additional_service_lease_state() {
  local lease_file="$1" expected="$2" receipt_file="$3"
  local component="$4" token="$5" id="$6" receipt phase
  if [[ -e "$lease_file" || -L "$lease_file" ]]; then
    [[ "$(read_service_lease "$lease_file" 2>/dev/null || true)" = "$expected" ]]
    return
  fi
  receipt="$(read_service_retirement_receipt "$receipt_file" 2>/dev/null || true)"
  read -r _ phase _ _ _ <<<"$receipt"
  [[ "$phase" = retired ]] && return 0
  [[ "$phase" = prepared \
    && "$(container_identity_observation "$id" "$id" "$token" "$component")" = retired ]]
}

retire_persisted_service() {
  local marker_file="$1" component="$2" lease_file="$3" expected_token="$4"
  local receipt_file="$5" receipt="" record="" name="" id="" token="" marker_component=""
  local phase="" observation=""
  receipt="$(read_service_retirement_receipt "$receipt_file" 2>/dev/null || true)"
  if [[ -n "$receipt" ]]; then
    validate_persisted_service_state \
      "$marker_file" "$component" "$lease_file" "$expected_token" "$receipt_file" \
      || return 1
    read -r _ phase marker_component token id <<<"$receipt"
    [[ "$phase" = retired ]] && return 0
    name="$(read_container_marker "$marker_file" | awk '{print $1}')" || return 1
  else
    record="$(read_container_marker "$marker_file" 2>/dev/null || true)"
    read -r name id token marker_component <<<"$record"
    [[ -n "$record" && "$token" = "$expected_token" && "$marker_component" = "$component" \
      && "$(read_service_lease "$lease_file" 2>/dev/null || true)" \
        = "1 $component $token $id" ]] || return 1
    container_identity_matches "$name" "$id" "$token" "$component" || return 1
    write_service_retirement_receipt \
      "$receipt_file" prepared "$component" "$token" "$id" || true
    if [[ "$(read_service_retirement_receipt "$receipt_file" 2>/dev/null || true)" \
      != "2 prepared $component $token $id" ]]; then
      log "could not persist the prepared $component retirement receipt"
      return 1
    fi
  fi
  observation="$(container_identity_observation "$id" "$id" "$token" "$component")"
  case "$observation" in
    running)
      log "stopping $component container $name"
      if ! docker rm -fv "$id" >/dev/null; then
        log "could not remove the exact owned $component container $name"
        return 1
      fi
      ;;
    retired) ;;
    *)
      log "could not prove the prepared $component container identity or retirement"
      return 1
      ;;
  esac
  write_service_retirement_receipt "$receipt_file" retired "$component" "$token" "$id" || true
  if [[ "$(read_service_retirement_receipt "$receipt_file" 2>/dev/null || true)" \
    != "2 retired $component $token $id" ]]; then
    log "removed the exact owned $component container but could not persist its retirement receipt"
    return 1
  fi
}

read_teardown_transaction() {
  local file="$1"
  regular_private_file "$file" || return 1
  local schema phase token pid start_time restate_id postgres_id extra
  read -r schema phase token pid start_time restate_id postgres_id extra < "$file" || return 1
  [[ "$schema" = 1 && "$phase" =~ ^(retiring|retired)$ \
    && "$token" =~ ^[0-9a-fA-F-]{36}$ \
    && "$pid" =~ ^[0-9]+$ && "$start_time" =~ ^[0-9]+$ \
    && ( "$restate_id" = - || "$restate_id" =~ ^[0-9a-fA-F]{12,64}$ ) \
    && ( "$postgres_id" = - || "$postgres_id" =~ ^[0-9a-fA-F]{12,64}$ ) \
    && -z "$extra" ]] || return 1
  printf '%s %s %s %s %s %s %s\n' \
    "$schema" "$phase" "$token" "$pid" "$start_time" "$restate_id" "$postgres_id"
}

write_teardown_transaction() {
  local file="$1" phase="$2" token="$3" pid="$4" start_time="$5"
  local restate_id="$6" postgres_id="$7" expected existing
  expected="1 $phase $token $pid $start_time $restate_id $postgres_id"
  existing="$(read_teardown_transaction "$file" 2>/dev/null || true)"
  if [[ -n "$existing" ]]; then
    if [[ "$existing" = "$expected" ]]; then
      return 0
    fi
    if [[ "$phase" = retired \
      && "$existing" = "1 retiring $token $pid $start_time $restate_id $postgres_id" ]]; then
      printf '%s\n' "$expected" | publish_private_record replace "$file" \
        || [[ "$(read_teardown_transaction "$file" 2>/dev/null || true)" = "$expected" ]]
      return
    fi
    return 1
  fi
  [[ ! -e "$file" && ! -L "$file" && "$phase" = retiring ]] || return 1
  printf '%s\n' "$expected" | publish_private_record create "$file" \
    || [[ "$(read_teardown_transaction "$file" 2>/dev/null || true)" = "$expected" ]]
}

remove_exact_private_record() {
  local file="$1" expected="$2" reader="$3" label="$4"
  [[ -e "$file" || -L "$file" ]] || return 0
  if [[ "$("$reader" "$file" 2>/dev/null || true)" != "$expected" ]]; then
    log "refusing teardown finalization: $label changed at $file"
    return 1
  fi
  rm -f -- "$file" || {
    log "could not clear the exact retired $label at $file"
    return 1
  }
  [[ ! -e "$file" && ! -L "$file" ]] || {
    log "could not prove the retired $label was cleared at $file"
    return 1
  }
}

finalize_teardown_transaction() {
  local transaction_file="$1" transaction_expected="$2"
  local process_expected="$3" restate_marker_expected="$4" postgres_marker_expected="$5"
  local restate_ingress_lease_expected="$6" restate_admin_lease_expected="$7"
  local postgres_lease_expected="$8"
  local restate_receipt_expected="$9" postgres_receipt_expected="${10}"
  local retain_transaction="${11:-0}"
  local transaction=""
  transaction="$(read_teardown_transaction "$transaction_file" 2>/dev/null || true)"
  [[ "$transaction" = "$transaction_expected" ]] || return 1

  if [[ -n "$postgres_marker_expected" ]]; then
    remove_exact_private_record "$stack_postgres_marker" "$postgres_marker_expected" \
      read_container_marker "Postgres ownership marker" || return 1
    remove_exact_private_record "$postgres_lease" "$postgres_lease_expected" \
      read_service_lease "Postgres service lease" || return 1
  fi
  if [[ -n "$restate_marker_expected" ]]; then
    remove_exact_private_record "$stack_restate_marker" "$restate_marker_expected" \
      read_container_marker "Restate ownership marker" || return 1
    remove_exact_private_record "$restate_ingress_lease" "$restate_ingress_lease_expected" \
      read_service_lease "Restate ingress service lease" || return 1
    remove_exact_private_record "$restate_admin_lease" "$restate_admin_lease_expected" \
      read_service_lease "Restate admin service lease" || return 1
  fi
  remove_exact_private_record "$stack_pid_file" "$process_expected" \
    read_pid_file "workbench PID receipt" || return 1
  if [[ -n "$postgres_receipt_expected" ]]; then
    remove_exact_private_record "$stack_postgres_receipt" "$postgres_receipt_expected" \
      read_service_retirement_receipt "Postgres retirement receipt" || return 1
  fi
  if [[ -n "$restate_receipt_expected" ]]; then
    remove_exact_private_record "$stack_restate_receipt" "$restate_receipt_expected" \
      read_service_retirement_receipt "Restate retirement receipt" || return 1
  fi
  remove_exact_private_record "$stack_process_receipt" \
    "2 retired $ownership_token $workbench_pid $workbench_start_time" \
    read_process_retirement_receipt "workbench retirement receipt" || return 1
  if (( ! retain_transaction )); then
    remove_exact_private_record "$transaction_file" "$transaction_expected" \
      read_teardown_transaction "teardown transaction"
  fi
}

owned_service_lease_remains() {
  local token="$1" restate_id="$2" postgres_id="$3" file record
  local _schema component lease_token lease_id
  for file in "$launcher_lock_root"/restate-*.lease \
    "$launcher_lock_root"/postgres-*.lease; do
    [[ -e "$file" || -L "$file" ]] || continue
    record="$(read_service_lease "$file" 2>/dev/null || true)"
    [[ -n "$record" ]] || continue
    read -r _schema component lease_token lease_id <<<"$record"
    if [[ "$lease_token" = "$token" \
      && ( ( "$component" = restate && "$lease_id" = "$restate_id" ) \
        || ( "$component" = postgres && "$lease_id" = "$postgres_id" ) ) ]]; then
      return 0
    fi
  done
  return 1
}

finalize_orphaned_teardown_transaction() {
  local transaction_file="$1" required_key="${2:-}" transaction=""
  local schema phase token pid start_time restate_id postgres_id extra key
  transaction="$(read_teardown_transaction "$transaction_file" 2>/dev/null || true)"
  read -r schema phase token pid start_time restate_id postgres_id extra <<<"$transaction"
  [[ -n "$transaction" && -z "$extra" && "$phase" = retired ]] || {
    log "refusing teardown: orphan transaction is not an exact completed transaction"
    return 1
  }
  key="${transaction_file##*/workbench-}"
  key="${key%.teardown}"
  [[ -n "$key" && ( -z "$required_key" || "$key" = "$required_key" ) \
    && "$transaction_file" = "$state_dir/workbench-$key.teardown" ]] || {
    log "refusing teardown: orphan transaction path does not match the requested workbench identity"
    return 1
  }
  [[ "$(process_identity_observation "$pid" "$start_time")" = retired ]] || {
    log "refusing teardown: completed transaction process identity is not observably retired"
    return 1
  }
  if [[ "$restate_id" != - ]] \
    && [[ "$(container_identity_observation "$restate_id" "$restate_id" "$token" restate)" != retired ]]; then
    log "refusing teardown: completed transaction Restate identity is not observably retired"
    return 1
  fi
  if [[ "$postgres_id" != - ]] \
    && [[ "$(container_identity_observation "$postgres_id" "$postgres_id" "$token" postgres)" != retired ]]; then
    log "refusing teardown: completed transaction Postgres identity is not observably retired"
    return 1
  fi
  local sibling
  for sibling in "$state_dir/workbench-$key.meta" "$state_dir/workbench-$key.pid" \
    "$state_dir/workbench-$key.process-retired" "$state_dir/restate-$key.container" \
    "$state_dir/postgres-$key.container" "$state_dir/restate-$key.service-retired" \
    "$state_dir/postgres-$key.service-retired" \
    "$state_dir/.agent-workbench-dev-run-owner-$key"; do
    [[ ! -e "$sibling" && ! -L "$sibling" ]] || {
      log "refusing teardown: completed transaction still has lifecycle context at $sibling"
      return 1
    }
  done
  if owned_service_lease_remains "$token" "$restate_id" "$postgres_id"; then
    log "refusing teardown: completed transaction still has an owned service reservation"
    return 1
  fi
  remove_exact_private_record "$transaction_file" "$transaction" \
    read_teardown_transaction "orphan completed teardown transaction"
}

remove_matching_service_leases() {
  local component="$1" token="$2" id="$3" file record failed=0
  for file in "$launcher_lock_root"/restate-*.lease \
    "$launcher_lock_root"/postgres-*.lease; do
    [[ -e "$file" || -L "$file" ]] || continue
    record="$(read_service_lease "$file" 2>/dev/null || true)"
    [[ "$record" = "1 $component $token $id" ]] || continue
    remove_service_lease "$file" "$record" || failed=1
  done
  (( ! failed ))
}

finalize_orphaned_service_receipt() {
  local component="$1" receipt_file="$2" marker_file="$3"
  local receipt marker="" name="" schema phase receipt_component token id extra observation
  receipt="$(read_service_retirement_receipt "$receipt_file" 2>/dev/null || true)"
  read -r schema phase receipt_component token id extra <<<"$receipt"
  [[ -n "$receipt" && -z "$extra" && "$phase" =~ ^(prepared|retired)$ \
    && "$receipt_component" = "$component" ]] || return 1
  if [[ -e "$marker_file" || -L "$marker_file" ]]; then
    marker="$(read_container_marker "$marker_file" 2>/dev/null || true)"
    read -r name _ _ _ <<<"$marker"
    [[ "$marker" = "$name $id $token $component" ]] || return 1
  fi
  observation="$(container_identity_observation "$id" "$id" "$token" "$component")"
  [[ "$observation" = retired ]] || return 1
  if [[ "$phase" = prepared ]]; then
    write_service_retirement_receipt \
      "$receipt_file" retired "$component" "$token" "$id" || true
    receipt="$(read_service_retirement_receipt "$receipt_file" 2>/dev/null || true)"
    [[ "$receipt" = "2 retired $component $token $id" ]] || return 1
  fi
  remove_matching_service_leases "$component" "$token" "$id" || return 1
  remove_exact_private_record "$marker_file" "$marker" read_container_marker \
    "$component ownership marker" || return 1
  printf '%s\n' "$token"
}

load_start_finalization_context() {
  local file="$1"
  regular_private_file "$file" || return 1
  start_finalization_schema="" start_finalization_phase=""
  start_finalization_state_key="" start_finalization_state_dir=""
  start_finalization_workbench_addr="" start_finalization_data_action=""
  start_finalization_data_dir="" start_finalization_data_hash=""
  start_finalization_data_identity="" start_finalization_token=""
  start_finalization_owner_record="" start_finalization_data_record=""
  start_finalization_restate_record="" start_finalization_postgres_record=""
  # shellcheck disable=SC1090
  source "$file" || return 1
  [[ "$start_finalization_schema" = 2 \
    && "$start_finalization_phase" =~ ^(retired|data-finalized)$ \
    && "$start_finalization_data_action" =~ ^(remove|preserve)$ \
    && -n "$start_finalization_workbench_addr" \
    && "$(printf '%s' "$start_finalization_workbench_addr" \
      | tr -c 'A-Za-z0-9_.-' '_')" = "$start_finalization_state_key" \
    && "$start_finalization_state_dir" = /* \
    && "$start_finalization_data_dir" = /* \
    && "$start_finalization_data_hash" =~ ^[0-9a-f]{64}$ \
    && "$start_finalization_data_identity" =~ ^(-|[0-9]+:[0-9]+)$ \
    && "$start_finalization_token" =~ ^[0-9a-fA-F-]{36}$ \
    && "$file" = "$launcher_lock_root/$launcher_lock_hash-$start_finalization_state_key-start-finalizing" ]]
}

load_start_finalization_receipt() {
  local file="$1"
  load_start_finalization_context "$file" || return 1
  [[ "$start_finalization_workbench_addr" = "$workbench_addr" \
    && "$start_finalization_state_key" = "$state_key" \
    && "$start_finalization_state_dir" = "$state_dir" \
    && "$start_finalization_data_dir" = "$data_dir" \
    && "$start_finalization_data_hash" = "$data_path_hash" ]] || return 1

  local owner_schema owner_token owner_key owner_hash owner_extra
  read -r owner_schema owner_token owner_key owner_hash owner_extra \
    <<<"$start_finalization_owner_record"
  [[ "$owner_schema" = 1 && "$owner_token" = "$start_finalization_token" \
    && "$owner_key" = "$state_key" && "$owner_hash" = "$data_path_hash" \
    && -z "$owner_extra" ]] || return 1

  if [[ "$start_finalization_data_action" = remove ]]; then
    local data_schema data_token data_hash data_identity data_extra
    read -r data_schema data_token data_hash data_identity data_extra \
      <<<"$start_finalization_data_record"
    [[ "$data_schema" = 1 && "$data_token" = "$start_finalization_token" \
      && "$data_hash" = "$data_path_hash" \
      && "$data_identity" = "$start_finalization_data_identity" \
      && -z "$data_extra" ]] || return 1
  else
    [[ -z "$start_finalization_data_record" ]] || return 1
  fi

  local record record_schema phase component token id extra found=0 expected_component
  for expected_component in restate postgres; do
    if [[ "$expected_component" = restate ]]; then
      record="$start_finalization_restate_record"
    else
      record="$start_finalization_postgres_record"
    fi
    [[ -n "$record" ]] || continue
    read -r record_schema phase component token id extra <<<"$record"
    [[ "$record_schema" = 2 && "$phase" = retired \
      && "$component" = "$expected_component" \
      && "$token" = "$start_finalization_token" \
      && "$id" =~ ^[0-9a-fA-F]{12,64}$ && -z "$extra" ]] || return 1
    found=1
  done
  (( found ))
}

write_start_finalization_receipt() {
  local phase="$1" data_action="$2" token="$3" owner_record="$4" data_record="$5"
  local restate_record="$6" postgres_record="$7" publication=create content data_identity=-
  [[ "$phase" =~ ^(retired|data-finalized)$ \
    && "$data_action" =~ ^(remove|preserve)$ ]] || return 1
  if [[ -e "$data_dir" && ! -L "$data_dir" ]]; then
    data_identity="$(stat -c '%d:%i' "$data_dir")" || return 1
  fi
  if [[ "$data_action" = remove ]]; then
    [[ "$data_identity" =~ ^[0-9]+:[0-9]+$ && -n "$data_record" ]] || return 1
  else
    [[ -z "$data_record" ]] || return 1
  fi
  content="$({
    printf 'start_finalization_schema=2\n'
    printf 'start_finalization_phase=%q\n' "$phase"
    printf 'start_finalization_state_key=%q\n' "$state_key"
    printf 'start_finalization_state_dir=%q\n' "$state_dir"
    printf 'start_finalization_workbench_addr=%q\n' "$workbench_addr"
    printf 'start_finalization_data_action=%q\n' "$data_action"
    printf 'start_finalization_data_dir=%q\n' "$data_dir"
    printf 'start_finalization_data_hash=%q\n' "$data_path_hash"
    printf 'start_finalization_data_identity=%q\n' "$data_identity"
    printf 'start_finalization_token=%q\n' "$token"
    printf 'start_finalization_owner_record=%q\n' "$owner_record"
    printf 'start_finalization_data_record=%q\n' "$data_record"
    printf 'start_finalization_restate_record=%q\n' "$restate_record"
    printf 'start_finalization_postgres_record=%q\n' "$postgres_record"
  })" || return 1
  if [[ -e "$start_finalization_file" || -L "$start_finalization_file" ]]; then
    regular_private_file "$start_finalization_file" || return 1
    [[ "$(cat -- "$start_finalization_file")" = "$content" ]] || return 1
    return 0
  fi
  printf '%s\n' "$content" | publish_private_record "$publication" "$start_finalization_file" \
    || {
      regular_private_file "$start_finalization_file" \
        && [[ "$(cat -- "$start_finalization_file")" = "$content" ]]
    }
}

promote_start_finalization_receipt() {
  load_start_finalization_receipt "$start_finalization_file" || return 1
  [[ "$start_finalization_phase" = retired ]] || return 1
  local content
  content="$(sed 's/^start_finalization_phase=.*/start_finalization_phase=data-finalized/' \
    "$start_finalization_file")" || return 1
  printf '%s\n' "$content" | publish_private_record replace "$start_finalization_file" || true
  load_start_finalization_receipt "$start_finalization_file" \
    && [[ "$start_finalization_phase" = data-finalized ]]
}

remove_start_finalization_receipt() {
  local expected_hash="$1"
  regular_private_file "$start_finalization_file" || return 1
  [[ "$(sha256sum "$start_finalization_file" | awk '{print $1}')" = "$expected_hash" ]] \
    || return 1
  rm -f -- "$start_finalization_file" || return 1
  [[ ! -e "$start_finalization_file" && ! -L "$start_finalization_file" ]]
}

finalize_start_finalization_receipt() {
  load_start_finalization_receipt "$start_finalization_file" || return 1
  local restate_id=- postgres_id=- record phase component token id extra
  for record in "$start_finalization_restate_record" \
    "$start_finalization_postgres_record"; do
    [[ -n "$record" ]] || continue
    read -r _ phase component token id extra <<<"$record"
    [[ "$(container_identity_observation "$id" "$id" "$token" "$component")" = retired ]] \
      || return 1
    if [[ "$component" = restate ]]; then
      restate_id="$id"
    else
      postgres_id="$id"
    fi
  done
  [[ ! -e "$restate_marker_file" && ! -L "$restate_marker_file" \
    && ! -e "$postgres_marker_file" && ! -L "$postgres_marker_file" ]] || return 1
  owned_service_lease_remains "$start_finalization_token" "$restate_id" "$postgres_id" \
    && return 1
  if [[ -e "$run_owner_file" || -L "$run_owner_file" ]]; then
    [[ "$(read_run_owner "$run_owner_file" 2>/dev/null || true)" \
      = "$start_finalization_owner_record" ]] || return 1
  fi
  if [[ -e "$data_creation_receipt_file" || -L "$data_creation_receipt_file" ]]; then
    [[ "$start_finalization_data_action" = remove \
      && "$(read_data_creation_receipt "$data_creation_receipt_file" 2>/dev/null || true)" \
        = "$start_finalization_data_record" ]] || return 1
  fi
  if [[ -e "$restate_service_retirement_receipt_file" \
    || -L "$restate_service_retirement_receipt_file" ]]; then
    [[ "$(read_service_retirement_receipt "$restate_service_retirement_receipt_file" \
      2>/dev/null || true)" = "$start_finalization_restate_record" ]] || return 1
  fi
  if [[ -e "$postgres_service_retirement_receipt_file" \
    || -L "$postgres_service_retirement_receipt_file" ]]; then
    [[ "$(read_service_retirement_receipt "$postgres_service_retirement_receipt_file" \
      2>/dev/null || true)" = "$start_finalization_postgres_record" ]] || return 1
  fi

  if [[ "$start_finalization_phase" = retired ]]; then
    if [[ "$start_finalization_data_action" = remove ]]; then
      if [[ -e "$data_dir" || -L "$data_dir" ]]; then
        [[ -d "$data_dir" && ! -L "$data_dir" \
          && "$data_dir" != / && "$data_dir" != "$repo_root" \
          && "$(stat -c '%d:%i' "$data_dir" 2>/dev/null || true)" \
            = "$start_finalization_data_identity" ]] || return 1
        ! path_has_symlink_component "$configured_data_dir" || return 1
        rm -rf -- "$data_dir" || return 1
      fi
      [[ ! -e "$data_dir" && ! -L "$data_dir" ]] || return 1
    fi
    promote_start_finalization_receipt || return 1
  fi
  [[ "$start_finalization_phase" = data-finalized ]] || return 1
  if [[ "$start_finalization_data_action" = remove ]]; then
    [[ ! -e "$data_dir" && ! -L "$data_dir" ]] || return 1
  fi

  remove_exact_private_record "$run_owner_file" "$start_finalization_owner_record" \
    read_run_owner "orphan startup run-footprint record" || return 1
  if [[ -n "$start_finalization_postgres_record" ]]; then
    remove_exact_private_record "$postgres_service_retirement_receipt_file" \
      "$start_finalization_postgres_record" read_service_retirement_receipt \
      "Postgres retirement receipt" || return 1
  fi
  if [[ -n "$start_finalization_restate_record" ]]; then
    remove_exact_private_record "$restate_service_retirement_receipt_file" \
      "$start_finalization_restate_record" read_service_retirement_receipt \
      "Restate retirement receipt" || return 1
  fi
  local finalization_hash
  finalization_hash="$(sha256sum "$start_finalization_file" | awk '{print $1}')" \
    || return 1
  if ! remove_start_finalization_receipt "$finalization_hash"; then
    [[ ! -e "$start_finalization_file" && ! -L "$start_finalization_file" ]] \
      || return 1
  fi
}

finalize_orphaned_start_attempt() {
  local key="$1" allow_data_cleanup="${2:-0}"
  local candidate_start_finalization_file="$launcher_lock_root/$launcher_lock_hash-$key-start-finalizing"
  local restate_receipt="$state_dir/restate-$key.service-retired"
  local postgres_receipt="$state_dir/postgres-$key.service-retired"
  local restate_marker="$state_dir/restate-$key.container"
  local postgres_marker="$state_dir/postgres-$key.container"
  local owner_file="$state_dir/.agent-workbench-dev-run-owner-$key"
  local owner_record="" schema token="" owner_token owner_key owner_hash extra recovered_token
  local restate_receipt_record="" postgres_receipt_record="" has_service_receipt=0
  if [[ -e "$candidate_start_finalization_file" || -L "$candidate_start_finalization_file" ]]; then
    if (( ! allow_data_cleanup )); then
      log "retained exact startup cleanup authority; retry targeted down with the original data/run settings"
      return 1
    fi
    [[ "$candidate_start_finalization_file" = "$start_finalization_file" ]] || return 1
    finalize_start_finalization_receipt
    return
  fi
  [[ -e "$owner_file" || -L "$owner_file" ]] || return 1
  owner_record="$(read_run_owner "$owner_file" 2>/dev/null || true)"
  read -r schema owner_token owner_key owner_hash extra <<<"$owner_record"
  [[ -n "$owner_record" && -z "$extra" && "$owner_key" = "$key" \
    && "$owner_hash" = "$data_path_hash" ]] || return 1
  token="$owner_token"
  local component receipt marker
  for component in restate postgres; do
    if [[ "$component" = restate ]]; then
      receipt="$restate_receipt"; marker="$restate_marker"
    else
      receipt="$postgres_receipt"; marker="$postgres_marker"
    fi
    if [[ -e "$receipt" || -L "$receipt" ]]; then
      local receipt_record receipt_token
      receipt_record="$(read_service_retirement_receipt "$receipt" 2>/dev/null || true)"
      read -r _ _ _ receipt_token _ <<<"$receipt_record"
      [[ -n "$receipt_token" && ( -z "$token" || "$receipt_token" = "$token" ) ]] || return 1
      token="$receipt_token"
      has_service_receipt=1
      if [[ "$component" = restate ]]; then
        restate_receipt_record="$receipt_record"
      else
        postgres_receipt_record="$receipt_record"
      fi
    elif [[ -e "$marker" || -L "$marker" ]]; then
      return 1
    fi
  done
  [[ -n "$token" && "$has_service_receipt" = 1 ]] || return 1
  if (( allow_data_cleanup )) && [[ -n "$restate_receipt_record" ]]; then
    local _schema _phase _component receipt_token receipt_id lease_file
    read -r _schema _phase _component receipt_token receipt_id <<<"$restate_receipt_record"
    for lease_file in "$restate_ingress_service_lease_file" \
      "$restate_admin_service_lease_file"; do
      if [[ -e "$lease_file" || -L "$lease_file" ]]; then
        [[ "$(read_service_lease "$lease_file" 2>/dev/null || true)" \
          = "1 restate $receipt_token $receipt_id" ]] || return 1
      fi
    done
  fi
  if (( allow_data_cleanup )) && [[ -n "$postgres_receipt_record" ]] \
    && [[ -e "$postgres_service_lease_file" || -L "$postgres_service_lease_file" ]]; then
    local _pg_schema _pg_phase _pg_component pg_receipt_token pg_receipt_id
    read -r _pg_schema _pg_phase _pg_component pg_receipt_token pg_receipt_id \
      <<<"$postgres_receipt_record"
    [[ "$(read_service_lease "$postgres_service_lease_file" 2>/dev/null || true)" \
      = "1 postgres $pg_receipt_token $pg_receipt_id" ]] || return 1
  fi
  if [[ -e "$restate_receipt" || -L "$restate_receipt" ]]; then
    recovered_token="$(finalize_orphaned_service_receipt \
      restate "$restate_receipt" "$restate_marker")" || return 1
    [[ "$recovered_token" = "$token" ]] || return 1
    restate_receipt_record="$(read_service_retirement_receipt "$restate_receipt" \
      2>/dev/null || true)"
  fi
  if [[ -e "$postgres_receipt" || -L "$postgres_receipt" ]]; then
    recovered_token="$(finalize_orphaned_service_receipt \
      postgres "$postgres_receipt" "$postgres_marker")" || return 1
    [[ "$recovered_token" = "$token" ]] || return 1
    postgres_receipt_record="$(read_service_retirement_receipt "$postgres_receipt" \
      2>/dev/null || true)"
  fi
  if (( ! allow_data_cleanup )); then
    log "retained exact startup cleanup authority; retry targeted down with the original data/run settings"
    return 1
  fi
  local creation_record="" creation_schema creation_token creation_hash creation_identity creation_extra
  local data_action=preserve
  [[ -n "$owner_record" ]] || return 1
  if [[ -e "$data_creation_receipt_file" || -L "$data_creation_receipt_file" ]]; then
    creation_record="$(read_data_creation_receipt "$data_creation_receipt_file" 2>/dev/null || true)"
    read -r creation_schema creation_token creation_hash creation_identity creation_extra \
      <<<"$creation_record"
    [[ "$creation_schema" = 1 && -n "$creation_record" && -z "$creation_extra" \
      && "$creation_token" = "$token" && "$creation_hash" = "$data_path_hash" \
      && "$creation_identity" = "$(stat -c '%d:%i' "$data_dir" 2>/dev/null || true)" \
      && "$data_dir" != / && "$data_dir" != "$repo_root" ]] || return 1
    ! path_has_symlink_component "$configured_data_dir" || return 1
    data_action=remove
  fi
  write_start_finalization_receipt retired "$data_action" "$token" "$owner_record" "$creation_record" \
    "$restate_receipt_record" "$postgres_receipt_record" || return 1
  finalize_start_finalization_receipt
}

stop_stack_from_meta() (
  local stack_meta_file="$1"
  local retain_transaction="${2:-0}"
  if ! regular_private_file "$stack_meta_file"; then
    log "refusing teardown: missing or unsafe stack metadata at $stack_meta_file"
    return 1
  fi
  unset meta_schema workbench_addr workbench_pid workbench_start_time
  unset restate_ingress_url restate_admin_url deployment_url
  unset store_backend ownership_token restate_managed postgres_managed postgres_host postgres_port
  unset restate_container_name restate_container_id postgres_container_name postgres_container_id
  unset restate_retirement_authorized restate_deployment_id restate_registry_hash
  # shellcheck disable=SC1090
  source "$stack_meta_file"
  if [[ "${meta_schema:-}" != 3 || ! "${ownership_token:-}" =~ ^[0-9a-fA-F-]{36}$ \
    || ! "${workbench_pid:-}" =~ ^[0-9]+$ || ! "${workbench_start_time:-}" =~ ^[0-9]+$ \
    || ! "${restate_managed:-}" =~ ^[01]$ || ! "${postgres_managed:-}" =~ ^[01]$ \
    || ! "${store_backend:-}" =~ ^(sqlite|postgres)$ ]]; then
    log "refusing teardown: stack metadata is legacy or invalid at $stack_meta_file"
    return 1
  fi
  if [[ "$restate_managed" = 1 ]] \
    && [[ -z "${restate_container_name:-}" || ! "${restate_container_id:-}" =~ ^[0-9a-fA-F]{12,64}$ ]]; then
    log "refusing teardown: stack metadata lacks the original Restate identity"
    return 1
  fi
  if [[ "$postgres_managed" = 1 ]] \
    && [[ -z "${postgres_container_name:-}" || ! "${postgres_container_id:-}" =~ ^[0-9a-fA-F]{12,64}$ ]]; then
    log "refusing teardown: stack metadata lacks the original Postgres identity"
    return 1
  fi
  local stack_key expected_meta_file
  stack_key="$(printf '%s' "$workbench_addr" | tr -c 'A-Za-z0-9_.-' '_')"
  expected_meta_file="$state_dir/workbench-$stack_key.meta"
  if [[ "$stack_meta_file" != "$expected_meta_file" ]]; then
    log "refusing teardown: stack metadata path does not match its workbench identity"
    return 1
  fi
  local stack_pid_file="$state_dir/workbench-$stack_key.pid"
  local stack_process_receipt="$state_dir/workbench-$stack_key.process-retired"
  local stack_restate_marker="$state_dir/restate-$stack_key.container"
  local stack_postgres_marker="$state_dir/postgres-$stack_key.container"
  local stack_restate_receipt="$state_dir/restate-$stack_key.service-retired"
  local stack_postgres_receipt="$state_dir/postgres-$stack_key.service-retired"
  local stack_transaction="$state_dir/workbench-$stack_key.teardown"
  local expected_restate_id="-" expected_postgres_id="-"
  local expected_restate_marker="" expected_postgres_marker=""
  local expected_restate_lease="" expected_postgres_lease=""
  local expected_restate_receipt="" expected_postgres_receipt=""
  if [[ "$restate_managed" = 1 ]]; then
    expected_restate_id="$restate_container_id"
    expected_restate_marker="$restate_container_name $restate_container_id $ownership_token restate"
    expected_restate_receipt="2 retired restate $ownership_token $restate_container_id"
  fi
  if [[ "$postgres_managed" = 1 ]]; then
    expected_postgres_id="$postgres_container_id"
    expected_postgres_marker="$postgres_container_name $postgres_container_id $ownership_token postgres"
    expected_postgres_receipt="2 retired postgres $ownership_token $postgres_container_id"
  fi
  local transaction_retiring transaction_retired transaction="" transaction_phase=""
  transaction_retiring="1 retiring $ownership_token $workbench_pid $workbench_start_time $expected_restate_id $expected_postgres_id"
  transaction_retired="1 retired $ownership_token $workbench_pid $workbench_start_time $expected_restate_id $expected_postgres_id"
  if [[ -e "$stack_transaction" || -L "$stack_transaction" ]]; then
    transaction="$(read_teardown_transaction "$stack_transaction" 2>/dev/null || true)"
    if [[ "$transaction" != "$transaction_retiring" && "$transaction" != "$transaction_retired" ]]; then
      log "refusing teardown: transaction receipt is invalid or does not match original ownership"
      return 1
    fi
    read -r _ transaction_phase _ _ _ _ _ <<<"$transaction"
  fi

  local ingress_host ingress_port admin_host admin_port canonical_ingress canonical_admin
  read -r ingress_host ingress_port < <(url_host_port "$restate_ingress_url")
  read -r admin_host admin_port < <(url_host_port "$restate_admin_url")
  validate_port "Restate ingress" "$ingress_port"
  validate_port "Restate admin" "$admin_port"
  canonical_ingress="$(canonical_service_host "$ingress_host")"
  canonical_admin="$(canonical_service_host "$admin_host")"
  local restate_ingress_hash restate_admin_hash restate_ingress_lease restate_admin_lease
  restate_ingress_hash="$(printf '%s' "$canonical_ingress:$ingress_port" | sha256sum | awk '{print $1}')"
  restate_admin_hash="$(printf '%s' "$canonical_admin:$admin_port" | sha256sum | awk '{print $1}')"
  restate_ingress_lease="$launcher_lock_root/restate-ingress-$restate_ingress_hash.lease"
  restate_admin_lease="$launcher_lock_root/restate-admin-$restate_admin_hash.lease"

  local postgres_lease=""
  if [[ "$postgres_managed" = 1 ]]; then
    validate_port "Postgres" "$postgres_port"
    local canonical_postgres postgres_hash
    canonical_postgres="$(canonical_service_host "$postgres_host")"
    postgres_hash="$(printf '%s' "$canonical_postgres:$postgres_port" | sha256sum | awk '{print $1}')"
    postgres_lease="$launcher_lock_root/postgres-$postgres_hash.lease"
  fi
  expected_restate_lease="1 restate $ownership_token $expected_restate_id"
  expected_postgres_lease="1 postgres $ownership_token $expected_postgres_id"

  if [[ "$transaction_phase" = retired ]]; then
    finalize_teardown_transaction "$stack_transaction" "$transaction_retired" \
      "$workbench_pid $workbench_start_time" "$expected_restate_marker" \
      "$expected_postgres_marker" "$expected_restate_lease" "$expected_restate_lease" \
      "$expected_postgres_lease" "$expected_restate_receipt" "$expected_postgres_receipt" \
      "$retain_transaction"
    return
  fi

  validate_persisted_process_state "$stack_pid_file" "$stack_process_receipt" \
    "$ownership_token" "$workbench_pid" "$workbench_start_time" || return 1

  if [[ "$restate_managed" = 1 ]]; then
    if [[ "${restate_retirement_authorized:-}" != 1 \
      || ! "${restate_deployment_id:-}" =~ ^dp_[A-Za-z0-9]+$ \
      || ! "${restate_registry_hash:-}" =~ ^[0-9a-f]{64}$ ]]; then
      log "refusing teardown: stack metadata does not authorize exclusive Restate retirement"
      return 1
    fi
    validate_persisted_service_state "$stack_restate_marker" restate "$restate_ingress_lease" \
      "$ownership_token" "$stack_restate_receipt" || return 1
    validate_additional_service_lease_state "$restate_admin_lease" \
      "$expected_restate_lease" "$stack_restate_receipt" restate \
      "$ownership_token" "$restate_container_id" || return 1
    [[ "$(read_container_marker "$stack_restate_marker" 2>/dev/null || true)" \
      = "$expected_restate_marker" \
      && "$(read_service_lease "$restate_ingress_lease" 2>/dev/null || true)" \
      = "$expected_restate_lease" ]] || {
      log "refusing teardown: Restate records do not match the original launch identity"
      return 1
    }
    local restate_receipt="" restate_receipt_phase="" restate_observation="running"
    restate_receipt="$(read_service_retirement_receipt "$stack_restate_receipt" 2>/dev/null || true)"
    [[ -z "$restate_receipt" ]] || read -r _ restate_receipt_phase _ _ _ <<<"$restate_receipt"
    if [[ "$restate_receipt_phase" = prepared ]]; then
      restate_observation="$(container_identity_observation "$restate_container_id" \
        "$restate_container_id" "$ownership_token" restate)"
    fi
    if [[ "$restate_receipt_phase" != retired && "$restate_observation" != retired ]]; then
      local registry_records registry_hash expected_registry_record
      registry_records="$(deployment_registry_records "$restate_admin_url" 2>/dev/null || true)"
      registry_hash="$(printf '%s' "$registry_records" | sha256sum | awk '{print $1}')"
      expected_registry_record="$restate_deployment_id"$'\t'"${deployment_url%/}"
      if [[ "$registry_records" != "$expected_registry_record" \
        || "$registry_hash" != "$restate_registry_hash" ]]; then
        log "refusing teardown: current Restate deployment registry does not prove exclusive ownership"
        return 1
      fi
    fi
  fi
  if [[ "$postgres_managed" = 1 ]]; then
    validate_persisted_service_state "$stack_postgres_marker" postgres "$postgres_lease" \
      "$ownership_token" "$stack_postgres_receipt" || return 1
    [[ "$(read_container_marker "$stack_postgres_marker" 2>/dev/null || true)" \
      = "$expected_postgres_marker" \
      && "$(read_service_lease "$postgres_lease" 2>/dev/null || true)" \
      = "$expected_postgres_lease" ]] || {
      log "refusing teardown: Postgres records do not match the original launch identity"
      return 1
    }
  fi

  if [[ -z "$transaction" ]]; then
    write_teardown_transaction "$stack_transaction" retiring "$ownership_token" \
      "$workbench_pid" "$workbench_start_time" "$expected_restate_id" "$expected_postgres_id" || {
      log "could not persist the prepared teardown transaction"
      return 1
    }
  fi

  retire_persisted_process \
    "$stack_pid_file" "$stack_process_receipt" "$ownership_token" \
    "$workbench_pid" "$workbench_start_time" || return 1
  if [[ "$restate_managed" = 1 ]]; then
    retire_persisted_service "$stack_restate_marker" restate "$restate_ingress_lease" \
      "$ownership_token" "$stack_restate_receipt" || return 1
  elif [[ "$postgres_managed" = 1 ]]; then
    log "workbench stopped; retaining managed Postgres because the Restate engine is external"
    return 1
  else
    log "workbench stopped; external Restate remains registered"
  fi
  if [[ "$postgres_managed" = 1 ]]; then
    retire_persisted_service "$stack_postgres_marker" postgres "$postgres_lease" \
      "$ownership_token" "$stack_postgres_receipt" || return 1
  fi
  [[ "$(read_process_retirement_receipt "$stack_process_receipt" 2>/dev/null || true)" \
    = "2 retired $ownership_token $workbench_pid $workbench_start_time" ]] || return 1
  if [[ "$restate_managed" = 1 ]]; then
    [[ "$(read_service_retirement_receipt "$stack_restate_receipt" 2>/dev/null || true)" \
      = "$expected_restate_receipt" ]] || return 1
  fi
  if [[ "$postgres_managed" = 1 ]]; then
    [[ "$(read_service_retirement_receipt "$stack_postgres_receipt" 2>/dev/null || true)" \
      = "$expected_postgres_receipt" ]] || return 1
  fi
  write_teardown_transaction "$stack_transaction" retired "$ownership_token" \
    "$workbench_pid" "$workbench_start_time" "$expected_restate_id" "$expected_postgres_id" || {
    log "could not persist completed teardown transaction"
    return 1
  }
  finalize_teardown_transaction "$stack_transaction" "$transaction_retired" \
    "$workbench_pid $workbench_start_time" "$expected_restate_marker" \
    "$expected_postgres_marker" "$expected_restate_lease" "$expected_restate_lease" \
    "$expected_postgres_lease" "$expected_restate_receipt" "$expected_postgres_receipt" \
    "$retain_transaction"
)

stop_target() {
  if [[ ! -e "$meta_file" && ! -L "$meta_file" ]]; then
    if [[ -e "$start_finalization_file" || -L "$start_finalization_file" ]]; then
      finalize_start_finalization_receipt || {
        log "refusing teardown: retained startup finalization authority is incomplete or changed"
        log "retry exact cleanup with the same data/run settings: scripts/agent-workbench-dev.sh down --addr $workbench_addr"
        return 1
      }
      log "completed cleanup of the exact retired startup attempt"
      return 0
    fi
    if [[ -e "$teardown_transaction_file" || -L "$teardown_transaction_file" ]]; then
      finalize_orphaned_teardown_transaction "$teardown_transaction_file" "$state_key"
      return
    fi
    if [[ -e "$pid_file" || -L "$pid_file" ]]; then
      stop_pid_file "$pid_file" || true
    fi
    if [[ -e "$restate_service_retirement_receipt_file" \
      || -L "$restate_service_retirement_receipt_file" \
      || -e "$postgres_service_retirement_receipt_file" \
      || -L "$postgres_service_retirement_receipt_file" \
      || -e "$run_owner_file" || -L "$run_owner_file" ]]; then
      finalize_orphaned_start_attempt "$state_key" 1 || {
        log "refusing teardown: retained startup cleanup authority is incomplete or changed"
        return 1
      }
      log "completed cleanup of the exact retired startup attempt"
      return 0
    fi
    log "refusing service teardown: stack metadata is missing at $meta_file"
    return 1
  fi
  stop_stack_from_meta "$meta_file"
}

attempt_reset_metadata_matches() {
  local file="$1"
  regular_private_file "$file" || return 1
  (
    unset reset_schema owned_token owned_state_key owned_data_dir
    # shellcheck disable=SC1090
    source "$file"
    [[ "$reset_schema" = 6 && "$owned_token" = "$ownership_token" \
      && "$owned_state_key" = "$state_key" && "$owned_data_dir" = "$data_dir" ]]
  )
}

attempt_data_owner_matches() {
  local file="$1"
  regular_private_file "$file" || return 1
  (
    unset data_owner_schema data_owner_token data_owner_state_key data_owner_path
    # shellcheck disable=SC1090
    source "$file"
    [[ "$data_owner_schema" = 5 && "$data_owner_token" = "$ownership_token" \
      && "$data_owner_state_key" = "$state_key" && "$data_owner_path" = "$data_dir" ]]
  )
}

remove_attempt_reset_ownership() {
  [[ "${created_reset_ownership_this_attempt:-0}" = 1 ]] || return 0
  if (( resuming_stopped_stack )); then
    # These records predate the attempt. Restoring them leaves the same stopped
    # stack this `up` found, so the identical command stays retryable.
    printf '%s\n' "$stopped_stack_reset_record" \
      | publish_private_record replace "$reset_file" || return 1
    printf '%s\n' "$stopped_stack_data_owner_record" \
      | publish_private_record replace "$data_owner_file" || return 1
    created_reset_ownership_this_attempt=0
    return 0
  fi
  if [[ -e "$reset_file" || -L "$reset_file" ]]; then
    attempt_reset_metadata_matches "$reset_file" || return 1
  fi
  if [[ -e "$data_owner_file" || -L "$data_owner_file" ]]; then
    attempt_data_owner_matches "$data_owner_file" || return 1
  fi
  rm -f "$reset_file" "$data_owner_file" || return 1
  [[ ! -e "$reset_file" && ! -L "$reset_file" \
    && ! -e "$data_owner_file" && ! -L "$data_owner_file" ]] || return 1
  created_reset_ownership_this_attempt=0
}

cleanup_attempt_restate_service() {
  local has_lease_progress=0
  (( created_restate_ingress_service_lease_this_attempt \
    || created_restate_admin_service_lease_this_attempt )) && has_lease_progress=1
  if (( started_restate_this_attempt )); then
    if (( created_restate_ingress_service_lease_this_attempt )) \
      && [[ "$(read_service_lease "$restate_ingress_service_lease_file" 2>/dev/null || true)" \
        != "$restate_service_lease_record" ]]; then
      log "startup cleanup could not verify its exact Restate ingress service lease"
      return 1
    fi
    if (( created_restate_admin_service_lease_this_attempt )) \
      && [[ "$(read_service_lease "$restate_admin_service_lease_file" 2>/dev/null || true)" \
        != "$restate_service_lease_record" ]]; then
      log "startup cleanup could not verify its exact Restate admin service lease"
      return 1
    fi
    if (( has_lease_progress )); then
      retire_started_service_with_receipt "$restate_marker_file" \
        "$restate_service_retirement_receipt_file" restate \
        "$started_restate_name" "$started_restate_id" || return 1
    else
      stop_started_restate || return 1
    fi
    started_restate_this_attempt=0
  fi
  if (( created_restate_ingress_service_lease_this_attempt \
    || created_restate_admin_service_lease_this_attempt )); then
    [[ "$(read_service_retirement_receipt "$restate_service_retirement_receipt_file" \
      2>/dev/null || true)" \
      = "2 retired restate $ownership_token $started_restate_id" ]] || return 1
    remove_attempt_restate_service_leases "$restate_service_lease_record" || return 1
  fi
  if [[ -e "$restate_service_retirement_receipt_file" \
    || -L "$restate_service_retirement_receipt_file" ]]; then
    finalize_attempt_service_records "$restate_marker_file" \
      "$restate_service_retirement_receipt_file" restate \
      "$started_restate_name" "$started_restate_id"
  elif [[ -e "$restate_marker_file" || -L "$restate_marker_file" ]]; then
    remove_exact_private_record "$restate_marker_file" \
      "$started_restate_name $started_restate_id $ownership_token restate" \
      read_container_marker "Restate ownership marker"
  fi
}

cleanup_attempt_postgres_service() {
  if (( started_postgres_this_attempt )); then
    if (( created_postgres_service_lease_this_attempt )) \
      && [[ "$(read_service_lease "$postgres_service_lease_file" 2>/dev/null || true)" \
        != "$postgres_service_lease_record" ]]; then
      log "startup cleanup could not verify its exact Postgres service lease"
      return 1
    fi
    if (( created_postgres_service_lease_this_attempt )); then
      retire_started_service_with_receipt "$postgres_marker_file" \
        "$postgres_service_retirement_receipt_file" postgres \
        "$started_postgres_name" "$started_postgres_id" || return 1
    else
      stop_started_postgres || return 1
    fi
    started_postgres_this_attempt=0
  fi
  if (( created_postgres_service_lease_this_attempt )); then
    [[ "$(read_service_retirement_receipt "$postgres_service_retirement_receipt_file" \
      2>/dev/null || true)" \
      = "2 retired postgres $ownership_token $started_postgres_id" ]] || return 1
    remove_attempt_service_lease "$postgres_service_lease_file" \
      "$postgres_service_lease_record" created_postgres_service_lease_this_attempt || return 1
  fi
  if [[ -e "$postgres_service_retirement_receipt_file" \
    || -L "$postgres_service_retirement_receipt_file" ]]; then
    finalize_attempt_service_records "$postgres_marker_file" \
      "$postgres_service_retirement_receipt_file" postgres \
      "$started_postgres_name" "$started_postgres_id"
  elif [[ -e "$postgres_marker_file" || -L "$postgres_marker_file" ]]; then
    remove_exact_private_record "$postgres_marker_file" \
      "$started_postgres_name $started_postgres_id $ownership_token postgres" \
      read_container_marker "Postgres ownership marker"
  fi
}

cleanup_start_attempt() {
  if (( replacing_process )); then
    # A process replacement created nothing but the replacement process. The
    # engine, its journals, the managed stores, the registered deployment, the
    # run footprint and the application data all predate this attempt and are
    # never retired here.
    if (( started_workbench_this_attempt )); then
      if ! stop_attempt_workbench; then
        log "replacement cleanup could not stop the process it started; retaining the engine, application state, and ownership metadata"
        return 1
      fi
      started_workbench_this_attempt=0
    fi
    log "replacement cleanup retained the engine, managed services, deployment, and application data"
    return 0
  fi
  if (( reset_finalization_active )); then
    log "reset finalization remains retryable; retaining its authoritative ownership receipt"
    return 1
  fi
  if [[ -e "$start_finalization_file" || -L "$start_finalization_file" ]]; then
    finalize_start_finalization_receipt
    return
  fi
  if (( process_observation_uncertain )); then
    log "startup cleanup retained the host and dependent resources after unknown process observation"
    return 1
  fi
  if (( created_meta_this_attempt && started_restate_this_attempt \
    && ! restate_retirement_authorized )) && [[ -n "$registered_deployment_id" ]]; then
    capture_restate_registry_ownership
    if (( restate_retirement_authorized )); then
      write_meta || {
        log "startup cleanup could not bind its fresh Restate registry ownership"
        return 1
      }
    fi
  fi
  if (( created_meta_this_attempt && restate_retirement_authorized )) \
    && [[ -n "$registered_deployment_id" \
      && "$(read_pid_file "$pid_file" 2>/dev/null || true)" \
        = "$started_workbench_pid $started_workbench_start_time" \
      && "$(read_container_marker "$restate_marker_file" 2>/dev/null || true)" \
        = "$started_restate_name $started_restate_id $ownership_token restate" \
      && ( "$started_postgres_this_attempt" = 0 \
        || "$(read_container_marker "$postgres_marker_file" 2>/dev/null || true)" \
          = "$started_postgres_name $started_postgres_id $ownership_token postgres" ) ]]; then
    if ! stop_stack_from_meta "$meta_file" 1; then
      log "startup cleanup could not complete its persisted teardown transaction"
      return 1
    fi
    started_workbench_this_attempt=0
    started_restate_this_attempt=0
    started_postgres_this_attempt=0
    created_restate_ingress_service_lease_this_attempt=0
    created_restate_admin_service_lease_this_attempt=0
    created_postgres_service_lease_this_attempt=0
  fi
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
  if (( started_restate_this_attempt )) && [[ -n "$registered_deployment_id" ]]; then
    if ! fresh_restate_registry_ownership; then
      log "startup cleanup cannot prove fresh exclusive Restate registry ownership; retaining the engine and dependent state"
      return 1
    fi
  fi
  if (( started_restate_this_attempt \
    || created_restate_ingress_service_lease_this_attempt \
    || created_restate_admin_service_lease_this_attempt )) \
    || [[ -e "$restate_service_retirement_receipt_file" \
      || -L "$restate_service_retirement_receipt_file" ]]; then
    if ! cleanup_attempt_restate_service; then
      log "startup cleanup could not retire its exact Restate service and reservations; retaining application state and ownership metadata"
      return 1
    fi
  fi
  if (( started_postgres_this_attempt || created_postgres_service_lease_this_attempt )) \
    || [[ -e "$postgres_service_retirement_receipt_file" \
      || -L "$postgres_service_retirement_receipt_file" ]]; then
    if ! cleanup_attempt_postgres_service; then
      log "startup cleanup could not retire its exact Postgres service and reservation; retaining application state and ownership metadata"
      return 1
    fi
  fi
  if (( created_run_owner_this_attempt )); then
    if ! [[ "$(read_run_owner "$run_owner_file" 2>/dev/null || true)" = "$run_owner_record" ]]; then
      log "startup cleanup could not verify its exact run-footprint record; retaining application state and ownership metadata"
      return 1
    fi
    if ! rm -f "$run_owner_file"; then
      log "startup cleanup could not clear its exact run-footprint record; retaining application state and ownership metadata"
      return 1
    fi
    [[ ! -e "$run_owner_file" && ! -L "$run_owner_file" ]] || return 1
    created_run_owner_this_attempt=0
  fi
  if ! remove_attempt_reset_ownership; then
    log "startup cleanup could not verify its disposable-stack ownership metadata"
    return 1
  fi
  if ! remove_attempt_meta; then
    log "startup cleanup could not verify its run metadata; retaining it"
    return 1
  fi
  if ! remove_attempt_teardown_transaction; then
    log "startup cleanup could not finalize its completed teardown transaction"
    return 1
  fi
  if (( data_dir_created_this_attempt )) \
    && [[ "$data_dir" != / && "$data_dir" != "$repo_root" ]] \
    && ! path_has_symlink_component "$configured_data_dir"; then
    if ! data_creation_receipt_matches; then
      log "startup cleanup could not verify exclusive application data creation; retaining the directory"
      return 1
    fi
    if ! rm -rf -- "$data_dir"; then
      log "startup cleanup could not remove the owned application data directory; retaining remaining ownership metadata"
      return 1
    fi
    data_dir_created_this_attempt=0
  fi
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
    if (( reset_finalization_active )); then
      if [[ "$reset_finalization_phase" = data-removed ]]; then
        log "disposable data was reset, but old-stack metadata finalization is incomplete"
      else
        log "resource retirement completed, but disposable data cleanup is incomplete"
      fi
      if write_reset_recovery_file; then
        log "stack is stopped; reset retry command saved at $reset_recovery_file"
      else
        log "could not persist the reset retry command; retained finalization authority at $reset_finalization_file"
      fi
    elif (( reset_committed )); then
      log "the disposable dev state was reset, but replacement startup failed"
      if ! write_reset_recovery_file; then
        log "could not persist the recovery command in the private launcher runtime directory"
      elif (( cleanup_complete )); then
        log "stack is stopped; recovery command saved at $reset_recovery_file"
      else
        log "replacement cleanup is incomplete; retained its application state and ownership metadata"
        log "recovery command saved at $reset_recovery_file; run it only after the retained attempt is safely retired"
      fi
    elif (( reset_destructive_started )); then
      log "disposable reset started but did not complete; the stack may be partially stopped"
      log "no unverified or external resource was removed"
      if write_reset_recovery_file; then
        log "reset retry command saved at $reset_recovery_file"
      else
        log "could not persist the reset retry command in the private launcher runtime directory"
      fi
    elif (( ! cleanup_complete )); then
      log "startup cleanup is incomplete; retained its application state and ownership metadata"
      log "retry exact cleanup with the same data/run settings: scripts/agent-workbench-dev.sh down --addr $workbench_addr"
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

build_reset_retry_command() {
  build_reset_recovery_command
  reset_recovery_command="${reset_recovery_command% up --addr *} restart --reset-dev-state --addr $(printf '%q' "$workbench_addr")"
}

write_reset_finalization_receipt() {
  local phase="$1" transaction="$2" publication=replace content reset_content reset_hash
  [[ "$phase" =~ ^(retired|data-removed)$ ]] || return 1
  if [[ -e "$reset_finalization_file" || -L "$reset_finalization_file" ]]; then
    regular_private_file "$reset_finalization_file" || return 1
    content="$(sed '/^reset_finalization_phase=/d' "$reset_finalization_file")" || return 1
  else
    publication=create
    regular_private_file "$reset_file" || return 1
    reset_content="$(cat -- "$reset_file")" || return 1
    reset_hash="$(sha256sum "$reset_file" | awk '{print $1}')" || return 1
    content="$reset_content
reset_finalization_schema=1
reset_finalization_reset_hash=$reset_hash
reset_finalization_transaction=$(printf '%q' "$transaction")"
  fi
  printf '%s\nreset_finalization_phase=%s\n' "$content" "$phase" \
    | publish_private_record "$publication" "$reset_finalization_file"
}

load_reset_finalization_receipt() {
  regular_private_file "$reset_finalization_file" || return 1
  reset_finalization_schema="" reset_finalization_phase=""
  reset_finalization_reset_hash="" reset_finalization_transaction=""
  # shellcheck disable=SC1090
  source "$reset_finalization_file" || return 1
  [[ "$reset_finalization_schema" = 1 \
    && "$reset_finalization_phase" =~ ^(retired|data-removed)$ \
    && "$reset_finalization_reset_hash" =~ ^[0-9a-f]{64}$ \
    && -n "$reset_finalization_transaction" ]]
}

remove_reset_finalization_receipt() {
  local expected_hash="$1"
  regular_private_file "$reset_finalization_file" || return 1
  [[ "$(sha256sum "$reset_finalization_file" | awk '{print $1}')" = "$expected_hash" ]] \
    || return 1
  rm -f -- "$reset_finalization_file" || return 1
  [[ ! -e "$reset_finalization_file" && ! -L "$reset_finalization_file" ]]
}

write_reset_recovery_file() {
  local content
  content="$({
    printf '#!/usr/bin/env bash\n'
    printf 'set -euo pipefail\n'
    printf 'exec %s\n' "$reset_recovery_command"
  })"
  printf '%s\n' "$content" | publish_private_record replace "$reset_recovery_file"
}

stop_all_known() {
  local found=0 failed=0 file key expected_meta
  for file in "$launcher_lock_root/$launcher_lock_hash-"*-start-finalizing; do
    [[ -e "$file" || -L "$file" ]] || continue
    if ! load_start_finalization_context "$file"; then
      found=1
      failed=1
      log "refusing unsafe startup finalization authority at $file"
      continue
    fi
    [[ "$start_finalization_state_dir" = "$state_dir" ]] || continue
    found=1
    failed=1
    local retry_command
    printf -v retry_command \
      'env AGENT_WORKBENCH_RUN_DIR=%q AGENT_WORKBENCH_DATA_DIR=%q %q down --addr %q' \
      "$start_finalization_state_dir" "$start_finalization_data_dir" "$0" \
      "$start_finalization_workbench_addr"
    log "retained exact startup finalization authority; retry exact cleanup: $retry_command"
  done
  for file in "$state_dir"/workbench-*.meta; do
    [[ -e "$file" ]] || continue
    found=1
    stop_stack_from_meta "$file" || failed=1
  done
  for file in "$state_dir"/workbench-*.pid; do
    [[ -e "$file" ]] || continue
    key="${file##*/workbench-}"
    key="${key%.pid}"
    expected_meta="$state_dir/workbench-$key.meta"
    [[ -e "$expected_meta" || -L "$expected_meta" ]] && continue
    found=1
    if [[ -e "$state_dir/workbench-$key.process-retired" \
      || -L "$state_dir/workbench-$key.process-retired" ]]; then
      log "retaining orphan process retirement receipt without stack metadata"
    else
      stop_pid_file "$file" || true
    fi
    log "refusing service teardown: process metadata has no matching stack metadata at $expected_meta"
    failed=1
  done
  for file in "$state_dir"/restate-*.service-retired \
    "$state_dir"/postgres-*.service-retired; do
    [[ -e "$file" || -L "$file" ]] || continue
    key="${file##*/}"
    key="${key#restate-}"
    key="${key#postgres-}"
    key="${key%.service-retired}"
    expected_meta="$state_dir/workbench-$key.meta"
    [[ -e "$expected_meta" || -L "$expected_meta" ]] && continue
    found=1
    finalize_orphaned_start_attempt "$key" 0 || failed=1
  done
  for file in "$state_dir"/restate-*.container "$state_dir"/postgres-*.container; do
    [[ -e "$file" || -L "$file" ]] || continue
    key="${file##*/}"
    key="${key#restate-}"
    key="${key#postgres-}"
    key="${key%.container}"
    expected_meta="$state_dir/workbench-$key.meta"
    [[ -e "$expected_meta" || -L "$expected_meta" ]] && continue
    found=1
    log "refusing service teardown: ownership marker has no matching stack metadata at $expected_meta"
    failed=1
  done
  for file in "$state_dir"/workbench-*.teardown; do
    [[ -e "$file" || -L "$file" ]] || continue
    key="${file##*/workbench-}"
    key="${key%.teardown}"
    expected_meta="$state_dir/workbench-$key.meta"
    [[ -e "$expected_meta" || -L "$expected_meta" ]] && continue
    found=1
    finalize_orphaned_teardown_transaction "$file" "$key" || failed=1
  done
  for file in "$state_dir"/workbench-*.process-retired; do
    [[ -e "$file" || -L "$file" ]] || continue
    key="${file##*/}"
    key="${key#workbench-}"
    key="${key#restate-}"
    key="${key#postgres-}"
    key="${key%.process-retired}"
    key="${key%.service-retired}"
    expected_meta="$state_dir/workbench-$key.meta"
    [[ -e "$expected_meta" || -L "$expected_meta" ]] && continue
    found=1
    log "refusing teardown: retirement receipt has no matching stack metadata at $expected_meta"
    failed=1
  done
  if (( ! found )); then
    log "no managed workbench processes found"
  fi
  (( ! failed ))
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

require_restate_endpoint_admission() {
  [[ ! -e "$legacy_restate_service_lease_file" && ! -L "$legacy_restate_service_lease_file" ]] \
    || die "legacy composite Restate reservation cannot prove individual endpoint ownership"
  require_service_unreserved "$restate_ingress_service_lease_file" "Restate ingress"
  require_service_unreserved "$restate_admin_service_lease_file" "Restate admin"
  local ingress_ready=0 admin_ready=0
  tcp_ready "$ingress_host" "$ingress_port" && ingress_ready=1
  tcp_ready "$admin_host" "$admin_port" && admin_ready=1
  if (( ingress_ready != admin_ready )); then
    die "Restate ingress and admin endpoints must both belong to one ready external service or both be free"
  fi
}

ensure_restate() {
  require_restate_endpoint_admission
  local ingress_ready=0 admin_ready=0
  tcp_ready "$ingress_host" "$ingress_port" && ingress_ready=1
  tcp_ready "$admin_host" "$admin_port" && admin_ready=1
  if (( ingress_ready && admin_ready )); then
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
  started_restate_name="$restate_container"
  started_restate_id="$container_id"
  started_restate_this_attempt=1
  [[ "$container_id" =~ ^[0-9a-fA-F]{12,64}$ ]] \
    || die "Docker returned an invalid Restate container id"
  write_container_marker "$restate_marker_file" "$restate_container" "$container_id" restate

  if ! wait_tcp "Restate ingress" "$ingress_host" "$ingress_port" 60; then
    docker logs "$restate_container" >&2 || true
    die "Restate ingress did not become ready at $restate_ingress_url"
  fi
  if ! wait_tcp "Restate admin" "$admin_host" "$admin_port" 60; then
    docker logs "$restate_container" >&2 || true
    die "Restate admin did not become ready at $restate_admin_url"
  fi
  restate_service_lease_record="1 restate $ownership_token $container_id"
  publish_attempt_service_lease "$restate_ingress_service_lease_file" restate "$container_id" \
    created_restate_ingress_service_lease_this_attempt \
    || die "could not reserve the launcher-created Restate ingress service"
  publish_attempt_service_lease "$restate_admin_service_lease_file" restate "$container_id" \
    created_restate_admin_service_lease_this_attempt \
    || die "could not reserve the launcher-created Restate admin service"
}

ensure_postgres() {
  (( postgres_enabled )) || return 0
  require_service_unreserved "$postgres_service_lease_file" Postgres
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
  started_postgres_name="$postgres_container"
  started_postgres_id="$container_id"
  started_postgres_this_attempt=1
  [[ "$container_id" =~ ^[0-9a-fA-F]{12,64}$ ]] \
    || die "Docker returned an invalid Postgres container id"
  write_container_marker "$postgres_marker_file" "$postgres_container" "$container_id" postgres

  if ! wait_tcp "Postgres" "$postgres_host" "$postgres_port" 60; then
    docker logs "$postgres_container" >&2 || true
    die "Postgres did not become ready at $postgres_host:$postgres_port"
  fi
  postgres_service_lease_record="1 postgres $ownership_token $container_id"
  publish_attempt_service_lease "$postgres_service_lease_file" postgres "$container_id" \
    created_postgres_service_lease_this_attempt \
    || die "could not reserve the launcher-created Postgres service"
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

# Digest of the durable Restate trust domain this stack is bound to. Lash
# persists only the digest of RESTATE_AUTHORITY_ID and puts it in every
# durable-wait address, so a process replacement carrying a different value
# would not continue the same durable state. `-` records that the launcher ran
# without one (the workbench itself refuses to boot in that case).
current_restate_authority_digest() {
  local value="${RESTATE_AUTHORITY_ID:-}"
  if [[ -z "$value" ]]; then
    printf -- '-\n'
    return
  fi
  printf '%s' "$value" | sha256sum | awk '{print $1}'
}

write_meta() {
  local content
  local meta_restate_managed="$started_restate_this_attempt"
  local meta_restate_name="$started_restate_name"
  local meta_restate_id="$started_restate_id"
  local meta_postgres_managed="$started_postgres_this_attempt"
  local meta_postgres_name="$started_postgres_name"
  local meta_postgres_id="$started_postgres_id"
  local meta_retirement_authorized="$restate_retirement_authorized"
  local meta_deployment_id="$registered_deployment_id"
  local meta_registry_hash="$restate_registry_hash"
  if (( inherited_service_facts )); then
    meta_restate_managed="$inherited_restate_managed"
    meta_restate_name="$inherited_restate_name"
    meta_restate_id="$inherited_restate_id"
    meta_postgres_managed="$inherited_postgres_managed"
    meta_postgres_name="$inherited_postgres_name"
    meta_postgres_id="$inherited_postgres_id"
    meta_retirement_authorized="$inherited_retirement_authorized"
    meta_deployment_id="$inherited_deployment_id"
    meta_registry_hash="$inherited_registry_hash"
  fi
  content="$({
    printf 'meta_schema=3\n'
    printf 'workbench_addr=%q\n' "$workbench_addr"
    printf 'workbench_pid=%q\n' "$started_workbench_pid"
    printf 'workbench_start_time=%q\n' "$started_workbench_start_time"
    printf 'workbench_url=%q\n' "$workbench_url"
    printf 'restate_endpoint_addr=%q\n' "$restate_endpoint_addr"
    printf 'restate_ingress_url=%q\n' "$restate_ingress_url"
    printf 'restate_admin_url=%q\n' "$restate_admin_url"
    printf 'deployment_url=%q\n' "$(endpoint_url)"
    printf 'store_backend=%q\n' "$store_backend"
    printf 'data_dir=%q\n' "$data_dir"
    printf 'database_fingerprint=%q\n' "$database_fingerprint"
    printf 'ownership_token=%q\n' "$ownership_token"
    printf 'restate_managed=%q\n' "$meta_restate_managed"
    printf 'postgres_managed=%q\n' "$meta_postgres_managed"
    printf 'restate_container_name=%q\n' "$meta_restate_name"
    printf 'restate_container_id=%q\n' "$meta_restate_id"
    printf 'postgres_container_name=%q\n' "$meta_postgres_name"
    printf 'postgres_container_id=%q\n' "$meta_postgres_id"
    printf 'restate_retirement_authorized=%q\n' "$meta_retirement_authorized"
    printf 'restate_deployment_id=%q\n' "$meta_deployment_id"
    printf 'restate_registry_hash=%q\n' "$meta_registry_hash"
    printf 'postgres_host=%q\n' "$postgres_host"
    printf 'postgres_port=%q\n' "$postgres_port"
    printf 'restate_authority_digest=%q\n' "$(current_restate_authority_digest)"
    printf 'log_file=%q\n' "$log_file"
  })"
  printf '%s\n' "$content" | publish_private_record replace "$meta_file"
}

capture_restate_registry_ownership() {
  restate_retirement_authorized=0
  restate_registry_hash=""
  (( started_restate_this_attempt )) || return 0
  local registry_records expected_record
  registry_records="$(deployment_registry_records "$restate_admin_url" 2>/dev/null || true)"
  expected_record="$registered_deployment_id"$'\t'"$(endpoint_url)"
  expected_record="${expected_record%/}"
  if [[ -n "$registered_deployment_id" && "$registry_records" = "$expected_record" ]]; then
    restate_registry_hash="$(printf '%s' "$registry_records" | sha256sum | awk '{print $1}')"
    restate_retirement_authorized=1
  fi
}

fresh_restate_registry_ownership() {
  (( restate_retirement_authorized )) || return 1
  [[ "$registered_deployment_id" =~ ^dp_[A-Za-z0-9]+$ \
    && "$restate_registry_hash" =~ ^[0-9a-f]{64}$ ]] || return 1
  local registry_records registry_hash expected_record
  registry_records="$(deployment_registry_records "$restate_admin_url" 2>/dev/null || true)"
  registry_hash="$(printf '%s' "$registry_records" | sha256sum | awk '{print $1}')"
  expected_record="$registered_deployment_id"$'\t'"$(endpoint_url)"
  expected_record="${expected_record%/}"
  [[ "$registry_records" = "$expected_record" && "$registry_hash" = "$restate_registry_hash" ]]
}

attempt_meta_matches() {
  local file="$1" expected_addr="$2" expected_token="$3"
  regular_private_file "$file" || return 1
  (
    unset meta_schema workbench_addr workbench_pid workbench_start_time ownership_token
    # shellcheck disable=SC1090
    source "$file"
    [[ "$meta_schema" = 3 && "$workbench_addr" = "$expected_addr" \
      && "$workbench_pid" = "$started_workbench_pid" \
      && "$workbench_start_time" = "$started_workbench_start_time" \
      && "$ownership_token" = "$expected_token" ]]
  )
}

remove_attempt_meta() {
  (( created_meta_this_attempt )) || return 0
  if (( resuming_stopped_stack )); then
    printf '%s\n' "$stopped_stack_meta_record" \
      | publish_private_record replace "$meta_file" || return 1
    created_meta_this_attempt=0
    return 0
  fi
  if [[ ! -e "$meta_file" && ! -L "$meta_file" ]] \
    && (( data_dir_created_this_attempt )) \
    && path_contains_path "$data_dir" "$meta_file"; then
    created_meta_this_attempt=0
    return 0
  fi
  attempt_meta_matches "$meta_file" "$workbench_addr" "$ownership_token" || return 1
  rm -f "$meta_file" || return 1
  [[ ! -e "$meta_file" && ! -L "$meta_file" ]] || return 1
  created_meta_this_attempt=0
}

remove_attempt_teardown_transaction() {
  [[ -e "$teardown_transaction_file" || -L "$teardown_transaction_file" ]] || return 0
  local restate_id="${started_restate_id:--}" postgres_id="${started_postgres_id:--}"
  local expected="1 retired $ownership_token $started_workbench_pid $started_workbench_start_time $restate_id $postgres_id"
  remove_exact_private_record "$teardown_transaction_file" "$expected" \
    read_teardown_transaction "startup teardown transaction"
}

write_reset_metadata() {
  local pid_record restate_record postgres_record="" reset_content data_owner_content
  pid_record="$(pid_file_identity "$pid_file")" || return 1
  restate_record="$(read_container_marker "$restate_marker_file")" || return 1
  if [[ "$store_backend" = postgres ]]; then
    postgres_record="$(read_container_marker "$postgres_marker_file")" || return 1
  fi
  restate_service_leases_match "$restate_service_lease_record" || return 1
  if [[ "$store_backend" = postgres ]]; then
    [[ "$(read_service_lease "$postgres_service_lease_file" 2>/dev/null || true)" \
      = "$postgres_service_lease_record" ]] || return 1
  fi
  [[ "$(read_run_owner "$run_owner_file" 2>/dev/null || true)" = "$run_owner_record" ]] \
    || return 1
  reset_content="$({
    printf 'reset_schema=6\n'
    printf 'owned_token=%q\n' "$ownership_token"
    printf 'owned_state_key=%q\n' "$state_key"
    printf 'owned_state_dir=%q\n' "$state_dir"
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
    printf 'owned_restate_ingress_service_lease=%q\n' "$restate_service_lease_record"
    printf 'owned_restate_admin_service_lease=%q\n' "$restate_service_lease_record"
    printf 'owned_postgres_service_lease=%q\n' "$postgres_service_lease_record"
    printf 'owned_run_owner=%q\n' "$run_owner_record"
    printf 'owned_log_file=%q\n' "$log_file"
  })"
  data_owner_content="$({
    printf 'data_owner_schema=5\n'
    printf 'data_owner_token=%q\n' "$ownership_token"
    printf 'data_owner_state_key=%q\n' "$state_key"
    printf 'data_owner_path=%q\n' "$data_dir"
    printf 'data_owner_state_dir=%q\n' "$state_dir"
  })"
  local publication=create
  # A resumed stack already carries both records under the same ownership token.
  (( ! resuming_stopped_stack )) || publication=replace
  printf '%s\n' "$reset_content" | publish_private_record "$publication" "$reset_file" || return 1
  printf '%s\n' "$data_owner_content" | publish_private_record "$publication" "$data_owner_file"
}

data_owner_matches() {
  regular_private_file "$data_owner_file" || return 1
  (
    data_owner_schema="" data_owner_token="" data_owner_state_key="" data_owner_path=""
    data_owner_state_dir=""
    # shellcheck disable=SC1090
    source "$data_owner_file"
    [[ "$data_owner_schema" = 5 \
      && "$data_owner_token" = "$owned_token" \
      && "$data_owner_state_key" = "$state_key" \
      && "$data_owner_path" = "$owned_data_dir" \
      && "$data_owner_state_dir" = "$state_dir" ]]
  )
}

finalize_reset_ownership() {
  (( created_reset_ownership_this_attempt )) || return 0
  local registry_records registry_hash expected_record
  registry_records="$(deployment_registry_records "$restate_admin_url" 2>/dev/null || true)"
  expected_record="$registered_deployment_id"$'\t'"$(endpoint_url)"
  if [[ -z "$registered_deployment_id" || "$registry_records" != "$expected_record" ]]; then
    log "reset unavailable: Restate deployment registry is not exclusively owned by this launcher stack"
    remove_attempt_reset_ownership \
      || die "could not clear changed disposable-stack ownership metadata"
    return 0
  fi
  registry_hash="$(printf '%s' "$registry_records" | sha256sum | awk '{print $1}')"
  local existing_content additional_content
  regular_private_file "$reset_file" && attempt_reset_metadata_matches "$reset_file" \
    || die "could not verify disposable-stack ownership metadata before finalization"
  existing_content="$(cat -- "$reset_file")"
  additional_content="$({
    printf 'owned_restate_deployment_id=%q\n' "$registered_deployment_id"
    printf 'owned_restate_registry_hash=%q\n' "$registry_hash"
  })"
  printf '%s\n%s\n' "$existing_content" "$additional_content" \
    | publish_private_record replace "$reset_file"
}

prepare_reset_ownership() {
  created_reset_ownership_this_attempt=0
  if (( ! started_restate_this_attempt )); then
    log "reset unavailable: Restate was not created by this launcher run"
    return 0
  fi
  if (( data_dir_existed_before_invocation && ! resuming_stopped_stack )); then
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
    unset meta_schema workbench_addr workbench_pid workbench_start_time
    unset workbench_url restate_endpoint_addr restate_ingress_url
    unset restate_admin_url deployment_url store_backend data_dir database_fingerprint ownership_token
    unset restate_managed postgres_managed restate_retirement_authorized
    unset restate_container_name restate_container_id postgres_container_name postgres_container_id
    unset restate_deployment_id restate_registry_hash
    # shellcheck disable=SC1090
    source "$meta_file"
    expected_postgres_managed=0
    [[ "$owned_store_backend" != postgres ]] || expected_postgres_managed=1
    read -r expected_restate_name expected_restate_id _ _ <<<"$owned_restate_record"
    expected_postgres_name=""
    expected_postgres_id=""
    if [[ "$expected_postgres_managed" = 1 ]]; then
      read -r expected_postgres_name expected_postgres_id _ _ <<<"$owned_postgres_record"
    fi
    [[ "$meta_schema" = 3 \
      && "$workbench_addr" = "$owned_workbench_addr" \
      && "$workbench_pid $workbench_start_time" = "$owned_pid_record" \
      && "$restate_endpoint_addr" = "$owned_restate_endpoint_addr" \
      && "$restate_ingress_url" = "$owned_restate_ingress_url" \
      && "$restate_admin_url" = "$owned_restate_admin_url" \
      && "$deployment_url" = "$owned_deployment_url" \
      && "$store_backend" = "$owned_store_backend" \
      && "$data_dir" = "$owned_data_dir" \
      && "$database_fingerprint" = "$owned_database_fingerprint" \
      && "$ownership_token" = "$owned_token" \
      && "$restate_managed" = 1 \
      && "$postgres_managed" = "$expected_postgres_managed" \
      && "$restate_container_name" = "$expected_restate_name" \
      && "$restate_container_id" = "$expected_restate_id" \
      && "$postgres_container_name" = "$expected_postgres_name" \
      && "$postgres_container_id" = "$expected_postgres_id" \
      && "$restate_retirement_authorized" = 1 \
      && "$restate_deployment_id" = "$owned_restate_deployment_id" \
      && "$restate_registry_hash" = "$owned_restate_registry_hash" ]]
  )
}

validate_reset_ownership() {
  reset_finalization_active=0
  reset_schema="" owned_token="" owned_state_key="" owned_workbench_addr=""
  owned_state_dir="" owned_log_file=""
  owned_restate_endpoint_addr="" owned_restate_ingress_url=""
  owned_restate_admin_url="" owned_deployment_url="" owned_restate_node_port="" owned_data_dir=""
  owned_data_identity=""
  owned_store_backend="" owned_database_fingerprint="" owned_pid_record=""
  owned_restate_record="" owned_postgres_record=""
  owned_restate_ingress_service_lease="" owned_restate_admin_service_lease=""
  owned_postgres_service_lease=""
  owned_run_owner=""
  owned_restate_deployment_id="" owned_restate_registry_hash=""
  if [[ -e "$reset_finalization_file" || -L "$reset_finalization_file" ]]; then
    load_reset_finalization_receipt \
      || die "reset refused: invalid or unsafe finalization receipt $reset_finalization_file"
    reset_finalization_active=1
    if [[ -e "$reset_file" || -L "$reset_file" ]]; then
      regular_private_file "$reset_file" \
        && [[ "$(sha256sum "$reset_file" | awk '{print $1}')" = "$reset_finalization_reset_hash" ]] \
        || die "reset refused: ownership metadata changed during finalization"
    fi
  else
    regular_private_file "$reset_file" \
      || die "reset refused: missing, legacy, or unsafe ownership record $reset_file"
    # shellcheck disable=SC1090
    source "$reset_file"
  fi
  [[ "$reset_schema" = 6 && "$owned_token" =~ ^[0-9a-fA-F-]{36}$ ]] \
    || die "reset refused: invalid ownership record $reset_file"
  [[ "$owned_state_key" = "$state_key" \
    && "$owned_state_dir" = "$state_dir" \
    && "$owned_workbench_addr" = "$workbench_addr" \
    && "$owned_restate_endpoint_addr" = "$restate_endpoint_addr" \
    && "$owned_restate_ingress_url" = "$restate_ingress_url" \
    && "$owned_restate_admin_url" = "$restate_admin_url" \
    && "$owned_deployment_url" = "$(endpoint_url)" \
    && "$owned_restate_node_port" = "$restate_node_port" \
    && "$owned_data_dir" = "$data_dir" \
    && "$owned_log_file" = "$log_file" \
    && "$owned_store_backend" = "$store_backend" \
    && "$owned_database_fingerprint" = "$database_fingerprint" ]] \
    || die "reset refused: current settings do not match the owned disposable stack"
  [[ "$owned_data_dir" != / && "$owned_data_dir" != "$repo_root" ]] \
    || die "reset refused: unsafe application data directory $owned_data_dir"
  path_has_symlink_component "$configured_data_dir" \
    && die "reset refused: application data path contains a symlink"
  [[ "$(realpath -m -- "$configured_data_dir")" = "$owned_data_dir" ]] \
    || die "reset refused: application data path does not resolve to the owned directory"
  if (( reset_finalization_active )); then
    if [[ "$reset_finalization_phase" = data-removed ]]; then
      [[ ! -e "$owned_data_dir" && ! -L "$owned_data_dir" ]] \
        || die "reset refused: application data reappeared after verified deletion"
    elif [[ -e "$owned_data_dir" || -L "$owned_data_dir" ]]; then
      [[ "$owned_data_identity" = "$(stat -c '%d:%i' "$owned_data_dir" 2>/dev/null || true)" ]] \
        || die "reset refused: application data directory identity changed during finalization"
    fi
  else
    [[ "$owned_data_identity" = "$(stat -c '%d:%i' "$owned_data_dir" 2>/dev/null || true)" ]] \
      || die "reset refused: application data directory identity changed"
  fi
  if [[ -e "$data_owner_file" || -L "$data_owner_file" ]]; then
    data_owner_matches \
      || die "reset refused: application data ownership does not match launcher metadata"
  elif (( ! reset_finalization_active )); then
    die "reset refused: application data ownership does not match launcher metadata"
  fi
  if [[ -e "$meta_file" || -L "$meta_file" ]]; then
    validate_run_metadata \
      || die "reset refused: run metadata does not match disposable-stack ownership"
  elif (( ! reset_finalization_active )); then
    die "reset refused: run metadata does not match disposable-stack ownership"
  fi
  [[ "$owned_run_owner" = "1 $owned_token $state_key $data_path_hash" ]] \
    || die "reset refused: run-footprint ownership is invalid"
  if [[ -e "$run_owner_file" || -L "$run_owner_file" ]]; then
    [[ "$(read_run_owner "$run_owner_file" 2>/dev/null || true)" = "$owned_run_owner" ]] \
      || die "reset refused: run-footprint ownership does not match launcher metadata"
  elif (( ! reset_finalization_active )); then
    die "reset refused: run-footprint ownership does not match launcher metadata"
  fi
  local owned_pid owned_start owned_restate_name owned_restate_id
  local owned_postgres_name="" owned_postgres_id="-" transaction=""
  read -r owned_pid owned_start <<<"$owned_pid_record"
  read -r owned_restate_name owned_restate_id _ _ <<<"$owned_restate_record"
  [[ "$owned_restate_name" = "$restate_container" ]] \
    || die "reset refused: configured Restate container does not match disposable-stack ownership"
  if [[ "$owned_store_backend" = postgres ]]; then
    (( ! database_url_explicit )) \
      || die "reset refused: explicit database URL is external or ambiguous"
    read -r owned_postgres_name owned_postgres_id _ _ <<<"$owned_postgres_record"
    [[ "$owned_postgres_name" = "$postgres_container" ]] \
      || die "reset refused: configured Postgres container does not match disposable-stack ownership"
  elif [[ "$owned_store_backend" != sqlite || -n "$agent_workbench_database_url" ]]; then
    die "reset refused: application database ownership is external or ambiguous"
  fi
  local expected_reset_transaction
  expected_reset_transaction="1 retired $owned_token $owned_pid $owned_start $owned_restate_id $owned_postgres_id"
  if (( reset_finalization_active )); then
    [[ "$reset_finalization_transaction" = "$expected_reset_transaction" ]] \
      || die "reset refused: finalization receipt does not match disposable-stack ownership"
    if [[ -e "$teardown_transaction_file" || -L "$teardown_transaction_file" ]]; then
      [[ "$(read_teardown_transaction "$teardown_transaction_file" 2>/dev/null || true)" \
        = "$expected_reset_transaction" ]] \
        || die "reset refused: teardown transaction changed during finalization"
    fi
    return 0
  fi
  if [[ -e "$teardown_transaction_file" || -L "$teardown_transaction_file" ]]; then
    transaction="$(read_teardown_transaction "$teardown_transaction_file" 2>/dev/null || true)"
    [[ "$transaction" = "1 retiring $owned_token $owned_pid $owned_start $owned_restate_id $owned_postgres_id" \
      || "$transaction" = "1 retired $owned_token $owned_pid $owned_start $owned_restate_id $owned_postgres_id" ]] \
      || die "reset refused: teardown transaction does not match disposable-stack ownership"
    return 0
  fi
  if stack_resources_fully_retired "$owned_token"; then
    # `down` already retired this stack's process, engine, managed services,
    # leases and receipts. A stopped stack is the easiest case to reset: what
    # remains is deleting the application data it still owns and these records.
    # The live-ownership proofs below have nothing left to prove — every
    # resource they guard is proven absent here, which is stricter, not weaker.
    reset_stack_already_retired=1
    return 0
  fi
  [[ "$(pid_file_identity "$pid_file" 2>/dev/null || true)" = "$owned_pid_record" ]] \
    || die "reset refused: workbench PID identity is missing or changed; finish the interrupted teardown with scripts/agent-workbench-dev.sh down --addr $workbench_addr, then run the same reset again"
  local name id token component
  read -r name id token component <<<"$owned_restate_record"
  [[ "$name" = "$restate_container" \
    && "$token" = "$owned_token" && "$component" = restate \
    && "$(read_container_marker "$restate_marker_file" 2>/dev/null || true)" = "$owned_restate_record" ]] \
    || die "reset refused: Restate ownership marker does not match"
  container_identity_matches "$name" "$id" "$token" "$component" \
    || die "reset refused: Restate container identity does not match"
  [[ "$owned_restate_ingress_service_lease" = "1 restate $owned_token $id" \
    && "$owned_restate_admin_service_lease" = "$owned_restate_ingress_service_lease" \
    && "$(read_service_lease "$restate_ingress_service_lease_file" 2>/dev/null || true)" = "$owned_restate_ingress_service_lease" \
    && "$(read_service_lease "$restate_admin_service_lease_file" 2>/dev/null || true)" = "$owned_restate_admin_service_lease" ]] \
    || die "reset refused: Restate service lease does not prove exclusive ownership"

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
    [[ "$owned_postgres_service_lease" = "1 postgres $owned_token $id" \
      && "$(read_service_lease "$postgres_service_lease_file" 2>/dev/null || true)" = "$owned_postgres_service_lease" ]] \
      || die "reset refused: Postgres service lease does not prove exclusive ownership"
  elif [[ "$owned_store_backend" != sqlite || -n "$agent_workbench_database_url" ]]; then
    die "reset refused: application database ownership is external or ambiguous"
  fi

  local registry_records registry_hash expected_registry_record
  registry_records="$(deployment_registry_records "$restate_admin_url" 2>/dev/null || true)"
  registry_hash="$(printf '%s' "$registry_records" | sha256sum | awk '{print $1}')"
  expected_registry_record="$owned_restate_deployment_id"$'\t'"${owned_deployment_url%/}"
  [[ -n "$owned_restate_deployment_id" \
    && "$owned_restate_registry_hash" = "$registry_hash" \
    && "$registry_records" = "$expected_registry_record" ]] \
    || die "reset refused: Restate deployment registry does not prove exclusive ownership"
}

# The Bazel label that builds this launcher's host binary, and the config that
# gives it the judged geometry. `--config=judged` is the Bazel spelling of
# Cargo's `[profile.judged]`; the block that defines it in `.bazelrc` says why
# it has to reach the whole graph rather than this binary's own crate.
workbench_bazel_label='//examples/agent-workbench:agent-workbench'

# Build the host binary and leave its path in `workbench_bin`.
#
# Two things about this are load bearing beyond what it builds.
#
# It runs before any launcher lock is taken. A build says nothing about who
# owns a stack's application data, and `/tmp/lash-agent-workbench-$UID/
# data-ownership.lock` is shared by every checkout on the box: holding it
# across a cold compile serialized every workbench on the machine behind one
# build, and six judged runbook rows spent more wall clock waiting on a
# sibling lane's compile than on driving runbooks (FIG-3153). The locks now
# cover the ownership mutation and the launch, not the build.
#
# And it goes through Bazel, so every checkout on the box shares one action
# cache instead of compiling the workspace again into its own Cargo target
# directory.
#
# `AGENT_WORKBENCH_BIN` skips the build entirely: a caller that already has a
# binary — a matrix driver booting row after row, a job with a prebuilt
# artifact — launches it and pays nothing.
prepare_workbench_binary() {
  if [[ -n "${AGENT_WORKBENCH_BIN:-}" ]]; then
    workbench_bin="$(realpath -m -- "$AGENT_WORKBENCH_BIN")"
    [[ -f "$workbench_bin" && -x "$workbench_bin" ]] \
      || die "AGENT_WORKBENCH_BIN=$AGENT_WORKBENCH_BIN is not an executable file"
    log "launching prebuilt agent-workbench binary $workbench_bin"
    return 0
  fi

  # The `provider-wire-fixtures` scenario is the one launch this script cannot
  # hand to Bazel. The feature turns on an optional dependency
  # (`provider-wire-fixtures = ["dep:lash-sim"]`), and the generated BUILD
  # files describe exactly one feature resolution of this workspace — the
  # default one — so no label builds this shape and none can be generated from
  # that resolve. `scripts/feature-coverage.toml` already covers the feature
  # through Cargo for the same reason. Its single judged row
  # (`workbench-valid-empty-completion`) keeps the judged profile and pays for
  # its own build.
  if [[ "${AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO:-}" = "valid-empty-completion" ]]; then
    log "building agent-workbench (cargo, profile: judged, provider-wire-fixtures)"
    cargo build -p agent-workbench --profile judged --features provider-wire-fixtures
    workbench_bin="${CARGO_TARGET_DIR:-$repo_root/target}/judged/agent-workbench"
    [[ -x "$workbench_bin" ]] \
      || die "the judged cargo build produced no binary at $workbench_bin"
    return 0
  fi

  log "building agent-workbench ($workbench_bazel_label, --config=judged)"
  local symlink_prefix="$launcher_lock_root/$launcher_lock_hash-bazel-"
  # `kiln build` is the warm path, and it is only a path at all inside a kiln
  # fork: it identifies the checkout from the kiln configuration and then runs
  # the driver below with `--config=shared`, which needs the `.kiln.bazelrc`
  # kiln writes into every fork. A plain checkout — a git worktree, a fresh
  # clone, a CI runner — has neither, so asking kiln there fails with "cannot
  # identify this repository" even though kiln is on PATH, and the shared
  # config would have no executor to reach anyway. `.kiln.bazelrc` is the fact
  # that separates the two, so it, and not the presence of the kiln binary,
  # decides. The local build is slower on a cold cache and it works.
  local -a build_command
  if command -v kiln >/dev/null 2>&1 && [[ -f "$repo_root/.kiln.bazelrc" ]]; then
    build_command=(kiln build)
  else
    log "no kiln fork here (.kiln.bazelrc is absent); building locally"
    build_command=("$repo_root/scripts/hermetic-build.sh" --local build)
  fi
  "${build_command[@]}" --config=judged "--symlink_prefix=$symlink_prefix" "$workbench_bazel_label" \
    || die "building $workbench_bazel_label --config=judged failed"
  local built="${symlink_prefix}bin/examples/agent-workbench/agent-workbench"
  [[ -x "$built" ]] || die "the judged build produced no binary at $built"

  # Launch a private copy rather than the Bazel output itself. `--config=judged`
  # adds rustc flags and no output-directory suffix, so the judged and the
  # ordinary configuration write the same path: a sibling `kiln build` between
  # this build and the exec below would otherwise replace the file with the
  # geometry the judged profile exists to avoid. Copy, then rename over the
  # previous copy — the rename is atomic, so a concurrent launch never reads a
  # half-written file, and a workbench already running from the old copy keeps
  # its own inode.
  local bin_dir="$launcher_lock_root/$launcher_lock_hash-bin"
  if [[ ! -e "$bin_dir" ]]; then
    mkdir -m 700 -- "$bin_dir"
  fi
  private_owned_directory "$bin_dir" \
    || die "unsafe launcher binary directory $bin_dir"
  workbench_bin="$bin_dir/agent-workbench"
  local staged="$bin_dir/.agent-workbench.$$"
  cp -f -- "$built" "$staged" \
    || die "could not stage the judged binary in $bin_dir"
  chmod 700 -- "$staged"
  mv -f -- "$staged" "$workbench_bin" \
    || die "could not publish the judged binary at $workbench_bin"
}

start_detached() {
  [[ -n "$workbench_bin" ]] \
    || die "this workbench was serving when the launch began and is not serving now; no host binary was built for it — run the same command again"
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
  started_workbench_pid="$pid"
  started_workbench_this_attempt=1
  started_workbench_start_time="$(process_start_time "$pid")" \
    || die "could not retain process identity for $pid"
  write_pid_file "$pid_file" "$pid" "$started_workbench_start_time" \
    || die "could not record process identity for $pid"
  write_meta
  created_meta_this_attempt=1
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
  require_restate_endpoint_admission
  require_exclusive_data_path_for_start
  adopt_stopped_stack
  start_attempt_active=1
  claim_data_directory
  mkdir -p "$state_dir"
  claim_run_footprint
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
  capture_restate_registry_ownership
  write_meta
  finalize_reset_ownership
  require_workbench_alive "before reporting ready"
  release_data_creation_receipt \
    || die "could not retire application data creation metadata"
  start_attempt_active=0
  rm -f "$reset_recovery_file" \
    || die "could not clear a stale reset recovery command"
  [[ ! -e "$reset_recovery_file" && ! -L "$reset_recovery_file" ]] \
    || die "could not prove a stale reset recovery command was cleared"
  log "ready: $workbench_url"
  open_browser "$workbench_url"
}

# Reads the run metadata of the stack this launcher is asked to replace and
# proves, before anything is stopped, that it is this launcher's own stack at
# exactly the current configuration. Runs in a subshell because the metadata
# assigns the launcher's own live globals. Prints the facts the replacement
# inherits, unit-separated: the separator must not be IFS whitespace, or `read`
# would collapse the empty fields an unmanaged store leaves.
replacement_target_record() (
  local expected_addr="$workbench_addr"
  local expected_endpoint_addr="$restate_endpoint_addr"
  local expected_ingress_url="$restate_ingress_url"
  local expected_admin_url="$restate_admin_url"
  local expected_deployment_url
  expected_deployment_url="$(endpoint_url)"
  local expected_store_backend="$store_backend"
  local expected_data_dir="$data_dir"
  local expected_database_fingerprint="$database_fingerprint"
  local expected_log_file="$log_file"
  local expected_restate_container="$restate_container"

  regular_private_file "$meta_file" || return 1
  unset meta_schema workbench_addr workbench_pid workbench_start_time
  unset workbench_url restate_endpoint_addr restate_ingress_url restate_admin_url
  unset deployment_url store_backend data_dir database_fingerprint ownership_token
  unset restate_managed postgres_managed restate_container_name restate_container_id
  unset postgres_container_name postgres_container_id restate_retirement_authorized
  unset restate_deployment_id restate_registry_hash postgres_host postgres_port
  unset restate_authority_digest log_file
  # shellcheck disable=SC1090
  source "$meta_file"

  [[ "${meta_schema:-}" = 3 \
    && "${ownership_token:-}" =~ ^[0-9a-fA-F-]{36}$ \
    && "${workbench_pid:-}" =~ ^[0-9]+$ && "${workbench_start_time:-}" =~ ^[0-9]+$ \
    && "${restate_managed:-}" =~ ^[01]$ && "${postgres_managed:-}" =~ ^[01]$ \
    && "${store_backend:-}" =~ ^(sqlite|postgres)$ ]] || return 1
  # Written only by launchers that record the durable trust domain. An older
  # stack cannot prove its authority did not change, so it is not replaceable.
  [[ -n "${restate_authority_digest:-}" ]] || return 1
  [[ "${restate_managed:-}" != 1 \
    || ( -n "${restate_container_name:-}" \
      && "${restate_container_id:-}" =~ ^[0-9a-fA-F]{12,64}$ ) ]] || return 1
  [[ "${postgres_managed:-}" != 1 \
    || ( -n "${postgres_container_name:-}" \
      && "${postgres_container_id:-}" =~ ^[0-9a-fA-F]{12,64}$ ) ]] || return 1
  [[ "${restate_managed:-}" != 1 || "${restate_container_name:-}" = "$expected_restate_container" ]] \
    || return 1

  [[ "$workbench_addr" = "$expected_addr" \
    && "$restate_endpoint_addr" = "$expected_endpoint_addr" \
    && "$restate_ingress_url" = "$expected_ingress_url" \
    && "$restate_admin_url" = "$expected_admin_url" \
    && "$deployment_url" = "$expected_deployment_url" \
    && "$store_backend" = "$expected_store_backend" \
    && "$data_dir" = "$expected_data_dir" \
    && "$database_fingerprint" = "$expected_database_fingerprint" \
    && "$log_file" = "$expected_log_file" ]] || return 1

  printf '%s\x1f%s\x1f%s\x1f%s\x1f%s\x1f%s\x1f%s\x1f%s\x1f%s\x1f%s\x1f%s\x1f%s\x1f%s\n' \
    "$workbench_pid" "$workbench_start_time" "$ownership_token" \
    "$restate_managed" "${restate_container_name:-}" "${restate_container_id:-}" \
    "$postgres_managed" "${postgres_container_name:-}" "${postgres_container_id:-}" \
    "${restate_retirement_authorized:-0}" "${restate_deployment_id:-}" \
    "${restate_registry_hash:-}" "$restate_authority_digest"
)

# Keeps the disposable-stack ownership record pointing at the live process, so
# a later `restart --reset-dev-state` still recognizes the stack it owns.
reset_ownership_names_process() {
  local expected_record="$1"
  [[ -e "$reset_file" || -L "$reset_file" ]] || return 0
  regular_private_file "$reset_file" || return 1
  (
    unset reset_schema owned_token owned_state_key owned_data_dir owned_pid_record
    # shellcheck disable=SC1090
    source "$reset_file"
    [[ "$reset_schema" = 6 && "$owned_token" = "$ownership_token" \
      && "$owned_state_key" = "$state_key" && "$owned_data_dir" = "$data_dir" \
      && "$owned_pid_record" = "$expected_record" ]]
  )
}

rebind_reset_ownership_process() {
  [[ -e "$reset_file" || -L "$reset_file" ]] || return 0
  regular_private_file "$reset_file" || return 1
  local new_record updated matched line
  new_record="$(pid_file_identity "$pid_file")" || return 1
  matched="$(grep -c '^owned_pid_record=' -- "$reset_file" || true)"
  [[ "$matched" = 1 ]] || return 1
  updated="$(
    while IFS= read -r line; do
      if [[ "$line" = owned_pid_record=* ]]; then
        printf 'owned_pid_record=%q\n' "$new_record"
      else
        printf '%s\n' "$line"
      fi
    done < "$reset_file"
  )" || return 1
  printf '%s\n' "$updated" | publish_private_record replace "$reset_file" || return 1
  reset_ownership_names_process "$new_record"
}

wait_replaced_endpoint_free() {
  local label="$1" host="$2" port="$3"
  local deadline=$((SECONDS + 30))
  while tcp_ready "$host" "$port"; do
    if (( SECONDS >= deadline )); then
      die "restart stopped after retiring the recorded process: $label port $host:$port did not become free; the engine, managed services and application data are retained and the same command is retryable"
    fi
    sleep 1
  done
}

run_replace_process() {
  local record=""
  record="$(replacement_target_record)" \
    || die "restart refused: no run metadata here proves this launcher owns a workbench stack at $workbench_addr with the current settings; use up to start one, or restart --reset-dev-state to replace a wholly launcher-owned disposable stack"

  local prior_pid prior_start prior_token prior_restate_managed prior_restate_name
  local prior_restate_id prior_postgres_managed prior_postgres_name prior_postgres_id
  local prior_retirement_authorized prior_deployment_id prior_registry_hash prior_authority
  IFS=$'\x1f' read -r prior_pid prior_start prior_token prior_restate_managed \
    prior_restate_name prior_restate_id prior_postgres_managed prior_postgres_name \
    prior_postgres_id prior_retirement_authorized prior_deployment_id \
    prior_registry_hash prior_authority <<<"$record"

  [[ "$prior_authority" = "$(current_restate_authority_digest)" ]] \
    || die "restart refused: RESTATE_AUTHORITY_ID does not match the durable trust domain this stack is bound to; export the original value, or use restart --reset-dev-state to start a new durable state"

  # Continue the recorded stack's ownership: its containers, service leases,
  # run footprint and application data all carry this token.
  ownership_token="$prior_token"
  local prior_record="$prior_pid $prior_start"

  [[ "$(read_run_owner "$run_owner_file" 2>/dev/null || true)" \
    = "1 $ownership_token $state_key $data_path_hash" ]] \
    || die "restart refused: run-footprint ownership does not match the recorded stack"
  if [[ -e "$data_owner_file" || -L "$data_owner_file" ]]; then
    local owned_token="$ownership_token" owned_data_dir="$data_dir"
    data_owner_matches \
      || die "restart refused: application data ownership does not match the recorded stack"
  fi
  reset_ownership_names_process "$prior_record" \
    || die "restart refused: disposable-stack ownership metadata does not name the recorded process"

  local published_record observation
  published_record="$(read_pid_file "$pid_file" 2>/dev/null || true)"
  observation="$(process_identity_observation "$prior_pid" "$prior_start")"
  case "$observation" in
    running|retired) ;;
    mismatch)
      die "restart refused: the recorded workbench PID belongs to a different process incarnation"
      ;;
    *)
      die "restart refused: the recorded workbench process identity could not be observed"
      ;;
  esac
  if [[ "$published_record" = "$prior_record" ]]; then
    log "replacing workbench process $prior_pid at $workbench_addr; engine, managed services and application data are retained"
    retire_persisted_process "$pid_file" "$process_retirement_receipt_file" \
      "$ownership_token" "$prior_pid" "$prior_start" \
      || die "restart stopped before replacement: could not retire the recorded workbench process; nothing else was stopped and the same command is retryable"
    rm -f -- "$pid_file" \
      || die "restart stopped before replacement: could not clear the retired process metadata"
  elif [[ -n "$published_record" ]]; then
    die "restart refused: workbench process metadata does not match the recorded stack"
  else
    [[ "$observation" = retired ]] \
      || die "restart refused: the recorded workbench process is still running without its process metadata"
    log "resuming an interrupted replacement of workbench process $prior_pid at $workbench_addr"
  fi
  [[ ! -e "$pid_file" && ! -L "$pid_file" ]] \
    || die "restart stopped before replacement: workbench process metadata reappeared"
  if [[ -e "$process_retirement_receipt_file" || -L "$process_retirement_receipt_file" ]]; then
    [[ "$(read_process_retirement_receipt "$process_retirement_receipt_file" 2>/dev/null || true)" \
      = "2 retired $ownership_token $prior_pid $prior_start" ]] \
      || die "restart stopped before replacement: the process retirement receipt is invalid or names another process"
    rm -f -- "$process_retirement_receipt_file" \
      || die "restart stopped before replacement: could not clear the process retirement receipt"
    [[ ! -e "$process_retirement_receipt_file" && ! -L "$process_retirement_receipt_file" ]] \
      || die "restart stopped before replacement: the process retirement receipt remains"
  fi

  wait_replaced_endpoint_free "workbench UI" "$workbench_wait_host" "$workbench_port"
  wait_replaced_endpoint_free "workbench Restate endpoint" "$endpoint_wait_host" "$endpoint_port"

  replacing_process=1
  inherited_service_facts=1
  inherited_restate_managed="$prior_restate_managed"
  inherited_restate_name="$prior_restate_name"
  inherited_restate_id="$prior_restate_id"
  inherited_postgres_managed="$prior_postgres_managed"
  inherited_postgres_name="$prior_postgres_name"
  inherited_postgres_id="$prior_postgres_id"
  inherited_retirement_authorized="$prior_retirement_authorized"
  inherited_deployment_id="$prior_deployment_id"
  inherited_registry_hash="$prior_registry_hash"
  start_attempt_active=1
  start_detached
  wait_workbench_ready 90
  wait_workbench_endpoint_ready 90
  rebind_reset_ownership_process \
    || die "the replacement process is running but its disposable-stack ownership record could not be rebound; rerun the same restart command"
  require_workbench_alive "before reporting ready"
  start_attempt_active=0
  log "replaced process; the Restate deployment, its journals and the application data at $data_dir were retained"
  log "ready: $workbench_url"
  open_browser "$workbench_url"
}

run_reset_dev_state() {
  validate_reset_ownership

  build_reset_retry_command
  reset_destructive_started=1
  start_attempt_active=1
  log "resetting wholly launcher-owned disposable stack at $workbench_addr"

  local reset_pid reset_start reset_restate_id reset_postgres_id="-"
  read -r reset_pid reset_start <<<"$owned_pid_record"
  read -r _ reset_restate_id _ _ <<<"$owned_restate_record"
  if [[ "$owned_store_backend" = postgres ]]; then
    read -r _ reset_postgres_id _ _ <<<"$owned_postgres_record"
  fi
  local reset_transaction
  reset_transaction="1 retired $owned_token $reset_pid $reset_start $reset_restate_id $reset_postgres_id"
  if (( ! reset_finalization_active )); then
    if (( reset_stack_already_retired )); then
      log "the recorded process, engine and managed services are already retired; clearing the application data and ownership records they left behind"
    else
      stop_stack_from_meta "$meta_file" 1 \
        || die "reset stopped before data deletion: resource retirement is incomplete and retryable"
    fi
    [[ "$(read_run_owner "$run_owner_file" 2>/dev/null || true)" = "$owned_run_owner" ]] \
      || die "reset stopped before data deletion: run-footprint ownership changed"
    [[ "$owned_data_identity" = "$(stat -c '%d:%i' "$owned_data_dir" 2>/dev/null || true)" ]] \
      || die "reset stopped before data deletion: application data directory identity changed"
    data_owner_matches \
      || die "reset stopped before data deletion: application ownership marker changed"
    write_reset_finalization_receipt retired "$reset_transaction" || true
    load_reset_finalization_receipt \
      || die "reset stopped before data deletion: could not persist finalization authority"
    [[ "$reset_finalization_phase" = retired \
      && "$reset_finalization_transaction" = "$reset_transaction" \
      && "$reset_finalization_reset_hash" \
        = "$(sha256sum "$reset_file" | awk '{print $1}')" ]] \
      || die "reset stopped before data deletion: finalization authority changed"
    reset_finalization_active=1
  else
    log "resuming verified reset finalization"
  fi

  if [[ "$reset_finalization_phase" = retired ]]; then
    if [[ -e "$owned_data_dir" || -L "$owned_data_dir" ]]; then
      [[ "$owned_data_identity" = "$(stat -c '%d:%i' "$owned_data_dir" 2>/dev/null || true)" ]] \
        || die "reset stopped before data deletion: application data directory identity changed"
      path_has_symlink_component "$configured_data_dir" \
        && die "reset stopped before data deletion: application data path became a symlink"
      [[ "$(realpath -m -- "$configured_data_dir")" = "$owned_data_dir" ]] \
        || die "reset stopped before data deletion: application data path changed"
      if ! rm -rf -- "$owned_data_dir"; then
        die "reset stopped during application data deletion; the same reset command is retryable"
      fi
    fi
    [[ ! -e "$owned_data_dir" && ! -L "$owned_data_dir" ]] \
      || die "reset stopped during application data deletion; owned data remains"
    write_reset_finalization_receipt data-removed "$reset_transaction" || true
    load_reset_finalization_receipt \
      || die "reset deleted application data but could not record completion"
    [[ "$reset_finalization_phase" = data-removed \
      && "$reset_finalization_transaction" = "$reset_transaction" ]] \
      || die "reset deleted application data but completion evidence changed"
  fi
  reset_committed=1

  remove_exact_private_record "$teardown_transaction_file" "$reset_transaction" \
    read_teardown_transaction "reset teardown transaction" \
    || die "reset completed data deletion but could not clear its teardown transaction"
  remove_exact_private_record "$run_owner_file" "$owned_run_owner" \
    read_run_owner "reset run-footprint ownership" \
    || die "reset completed data deletion but could not clear run-footprint ownership"
  if [[ -e "$meta_file" || -L "$meta_file" ]]; then
    validate_run_metadata \
      || die "reset completed data deletion but run metadata changed"
    rm -f -- "$meta_file" \
      || die "reset completed data deletion but could not clear run metadata"
    [[ ! -e "$meta_file" && ! -L "$meta_file" ]] \
      || die "reset completed data deletion but run metadata remains"
  fi
  if [[ -e "$reset_file" || -L "$reset_file" ]]; then
    regular_private_file "$reset_file" \
      && [[ "$(sha256sum "$reset_file" | awk '{print $1}')" = "$reset_finalization_reset_hash" ]] \
      || die "reset completed data deletion but ownership metadata changed"
    rm -f -- "$reset_file" \
      || die "reset completed data deletion but could not clear ownership metadata"
    [[ ! -e "$reset_file" && ! -L "$reset_file" ]] \
      || die "reset completed data deletion but ownership metadata remains"
  fi
  local retired_record
  for retired_record in "$pid_file" "$restate_marker_file" "$postgres_marker_file" \
    "$process_retirement_receipt_file" "$restate_service_retirement_receipt_file" \
    "$postgres_service_retirement_receipt_file"; do
    [[ ! -e "$retired_record" && ! -L "$retired_record" ]] \
      || die "reset completed data deletion but retired resource metadata reappeared at $retired_record"
  done
  rm -f -- "$owned_log_file" \
    || die "reset completed data deletion but could not clear its log"
  [[ ! -e "$owned_log_file" && ! -L "$owned_log_file" ]] \
    || die "reset completed data deletion but its log remains"
  local reset_finalization_hash
  reset_finalization_hash="$(sha256sum "$reset_finalization_file" | awk '{print $1}')" \
    || die "reset completed data deletion but could not verify finalization authority"
  if ! remove_reset_finalization_receipt "$reset_finalization_hash"; then
    if [[ ! -e "$reset_finalization_file" && ! -L "$reset_finalization_file" ]]; then
      reset_finalization_active=0
      build_reset_recovery_command
      die "reset finalization removal reported failure after clearing its authority; replacement is recoverable"
    fi
    die "reset completed data deletion but could not clear finalization authority"
  fi
  reset_finalization_active=0
  build_reset_recovery_command

  ownership_token="$(new_ownership_token)"
  data_dir_existed_before_invocation=0
  data_dir_created_this_attempt=0
  data_creation_identity=""
  data_creation_receipt_record=""
  started_workbench_this_attempt=0
  started_workbench_pid=""
  started_workbench_start_time=""
  started_restate_this_attempt=0
  started_restate_name=""
  started_restate_id=""
  external_restate_used_this_attempt=0
  started_postgres_this_attempt=0
  started_postgres_name=""
  started_postgres_id=""
  created_reset_ownership_this_attempt=0
  created_restate_ingress_service_lease_this_attempt=0
  created_restate_admin_service_lease_this_attempt=0
  created_postgres_service_lease_this_attempt=0
  created_run_owner_this_attempt=0
  created_meta_this_attempt=0
  restate_service_lease_record=""
  postgres_service_lease_record=""
  run_owner_record=""
  registered_deployment_id=""
  restate_registry_hash=""
  restate_retirement_authorized=0
  log "disposable dev state cleared; starting a fresh stack"
  run_up
}

run_foreground() {
  if ! ensure_ports_available; then
    return
  fi
  require_restate_endpoint_admission
  require_exclusive_data_path_for_start
  adopt_stopped_stack
  start_attempt_active=1
  claim_data_directory
  mkdir -p "$state_dir"
  claim_run_footprint
  ensure_restate

  local deployment_url
  deployment_url="$(endpoint_url)"
  if ! require_unused_deployment_uri "$restate_admin_url" "$deployment_url"; then
    cleanup_start_attempt || true
    die "refusing to replace an existing Restate deployment at $deployment_url"
  fi
  ensure_postgres

  cleanup_foreground() {
    if (( foreground_cleanup_done )); then
      return "$foreground_cleanup_status"
    fi
    foreground_cleanup_done=1
    foreground_cleanup_status=1
    if (( process_observation_uncertain )); then
      log "foreground cleanup retained the host and dependent resources after unknown process observation"
      return 1
    fi
    local persisted_stack_retired=0
    if [[ -n "$started_workbench_pid" ]]; then
      if [[ -z "$started_workbench_start_time" ]]; then
        log "foreground cleanup could not stop the owned workbench; retaining its engine and application state"
        foreground_cleanup_status=1
        return 1
      fi
      if [[ "$(read_pid_file "$pid_file" 2>/dev/null || true)" \
        = "$started_workbench_pid $started_workbench_start_time" ]] \
        && (( created_meta_this_attempt )); then
        if ! retire_persisted_process "$pid_file" "$process_retirement_receipt_file" \
          "$ownership_token" "$started_workbench_pid" "$started_workbench_start_time"; then
          log "foreground cleanup could not stop the owned workbench; retaining its engine and application state"
          return 1
        fi
      elif ! stop_process_identity "$started_workbench_pid" "$started_workbench_start_time"; then
        log "foreground cleanup could not stop its captured unpublished workbench process"
        return 1
      fi
      wait "$started_workbench_pid" >/dev/null 2>&1 || true
      started_workbench_this_attempt=0
    fi
    if (( external_restate_used_this_attempt )); then
      log "foreground cleanup cannot retire the external Restate engine; retaining application state, managed stores, and ownership metadata"
      foreground_cleanup_status=1
      return 1
    fi
    if (( started_restate_this_attempt )) && [[ -n "$registered_deployment_id" ]]; then
      if ! fresh_restate_registry_ownership; then
        log "foreground cleanup cannot prove fresh exclusive Restate registry ownership; retaining the engine and dependent state"
        foreground_cleanup_status=1
        return 1
      fi
      if ! stop_stack_from_meta "$meta_file" 1; then
        log "foreground cleanup could not complete its persisted teardown transaction"
        return 1
      fi
      persisted_stack_retired=1
      started_restate_this_attempt=0
      started_postgres_this_attempt=0
      created_restate_ingress_service_lease_this_attempt=0
      created_restate_admin_service_lease_this_attempt=0
      created_postgres_service_lease_this_attempt=0
    fi
    if (( ! persisted_stack_retired )) && {
      (( started_restate_this_attempt \
        || created_restate_ingress_service_lease_this_attempt \
        || created_restate_admin_service_lease_this_attempt )) \
        || [[ -e "$restate_service_retirement_receipt_file" \
          || -L "$restate_service_retirement_receipt_file" ]]
    }; then
      if ! cleanup_attempt_restate_service; then
        log "foreground cleanup could not retire its exact Restate service and reservations; retaining application state"
        foreground_cleanup_status=1
        return 1
      fi
    fi
    if (( ! persisted_stack_retired )) && {
      (( started_postgres_this_attempt || created_postgres_service_lease_this_attempt )) \
        || [[ -e "$postgres_service_retirement_receipt_file" \
          || -L "$postgres_service_retirement_receipt_file" ]]
    }; then
      if ! cleanup_attempt_postgres_service; then
        log "foreground cleanup could not retire its exact Postgres service and reservation; retaining application state"
        foreground_cleanup_status=1
        return 1
      fi
    fi
    if (( created_run_owner_this_attempt )); then
      if [[ "$(read_run_owner "$run_owner_file" 2>/dev/null || true)" != "$run_owner_record" ]] \
        || ! rm -f "$run_owner_file"; then
        log "foreground cleanup could not clear its exact run-footprint record; retaining application state"
        foreground_cleanup_status=1
        return 1
      fi
      [[ ! -e "$run_owner_file" && ! -L "$run_owner_file" ]] || return 1
    fi
    if (( data_dir_created_this_attempt )); then
      if ! release_data_creation_receipt; then
        log "foreground cleanup could not retire application data creation metadata"
        foreground_cleanup_status=1
        return 1
      fi
    fi
    if (( ! persisted_stack_retired )) \
      && [[ -e "$process_retirement_receipt_file" || -L "$process_retirement_receipt_file" ]]; then
      remove_exact_private_record "$pid_file" \
        "$started_workbench_pid $started_workbench_start_time" read_pid_file \
        "foreground workbench PID receipt" || return 1
      remove_exact_private_record "$process_retirement_receipt_file" \
        "2 retired $ownership_token $started_workbench_pid $started_workbench_start_time" \
        read_process_retirement_receipt "foreground workbench retirement receipt" || return 1
    fi
    if ! remove_attempt_meta; then
      log "foreground cleanup could not verify its run metadata; retaining it"
      foreground_cleanup_status=1
      return 1
    fi
    if (( persisted_stack_retired )) && ! remove_attempt_teardown_transaction; then
      log "foreground cleanup could not finalize its completed teardown transaction"
      foreground_cleanup_status=1
      return 1
    fi
    foreground_cleanup_status=0
  }
  foreground_exit() {
    local original_status=$? cleanup_status=0
    trap - EXIT INT TERM
    cleanup_foreground || cleanup_status=$?
    if (( original_status == 0 && cleanup_status != 0 )); then
      original_status="$cleanup_status"
    fi
    exit "$original_status"
  }
  foreground_signal() {
    local signal_status="$1"
    trap - INT TERM
    exit "$signal_status"
  }
  foreground_cleanup_done=0
  foreground_cleanup_status=0
  trap foreground_exit EXIT
  trap 'foreground_signal 130' INT
  trap 'foreground_signal 143' TERM

  log "starting workbench at $workbench_url"
  [[ -n "$workbench_bin" ]] \
    || die "no host binary was built for this launch — run the same command again"
  local -a workbench_env=(
    "AGENT_WORKBENCH_ADDR=$workbench_addr"
    "AGENT_WORKBENCH_RESTATE_ADDR=$restate_endpoint_addr"
    "AGENT_WORKBENCH_DATABASE_URL=$agent_workbench_database_url"
    "RESTATE_INGRESS_URL=$restate_ingress_url"
    "RESTATE_ADMIN_URL=$restate_admin_url"
    "AGENT_WORKBENCH_DATA_DIR=$data_dir"
  )
  (
    exec {launcher_lock_fd}>&-
    exec {launcher_data_lock_fd}>&-
    exec env "${workbench_env[@]}" "$workbench_bin"
  ) &
  started_workbench_pid="$!"
  started_workbench_this_attempt=1
  started_workbench_start_time="$(process_start_time "$started_workbench_pid")" \
    || die "could not retain process identity for $started_workbench_pid"
  write_pid_file "$pid_file" "$started_workbench_pid" "$started_workbench_start_time" \
    || die "could not record process identity for $started_workbench_pid"
  write_meta
  created_meta_this_attempt=1

  wait_workbench_ready 90
  wait_workbench_endpoint_ready 90
  log "registering Restate deployment $deployment_url"
  register_deployment "$restate_admin_url" "$deployment_url" \
    || die "failed to register Restate deployment $deployment_url through $restate_admin_url"
  capture_restate_registry_ownership
  write_meta

  require_workbench_alive "before reporting ready"
  log "ready: $workbench_url"
  open_browser "$workbench_url"
  wait "$started_workbench_pid"
  start_attempt_active=0
}

run_status_one() {
  local record="" pid="" start_time="" observation="retired"
  record="$(read_pid_file "$pid_file" 2>/dev/null || true)"
  if [[ -n "$record" ]]; then
    read -r pid start_time <<<"$record"
    observation="$(process_identity_observation "$pid" "$start_time")"
  elif [[ -e "$pid_file" || -L "$pid_file" ]]; then
    observation="unknown"
  fi
  if workbench_ready; then
    if [[ "$observation" = running ]]; then
      log "running: $workbench_url (pid $pid, log $log_file)"
    else
      log "running: $workbench_url (process identity unverified)"
    fi
    return 0
  fi
  if [[ "$observation" = running ]]; then
    log "process $pid exists but health check failed: $workbench_url/healthz"
    return 1
  fi
  if [[ "$observation" = unknown ]]; then
    log "unknown: process identity could not be observed for $workbench_url"
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
canonical_ingress_host="$(canonical_service_host "$ingress_host")"
canonical_admin_host="$(canonical_service_host "$admin_host")"
validate_port "Restate endpoint" "$endpoint_port"
validate_port "Restate ingress" "$ingress_port"
validate_port "Restate admin" "$admin_port"
validate_port "Restate node" "$restate_node_port"
if (( postgres_enabled )); then
  validate_port "Postgres" "$postgres_port"
  canonical_postgres_host="$(canonical_service_host "$postgres_host")"
else
  canonical_postgres_host=""
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
process_retirement_receipt_file="$state_dir/workbench-$state_key.process-retired"
teardown_transaction_file="$state_dir/workbench-$state_key.teardown"
meta_file="$state_dir/workbench-$state_key.meta"
log_file="$state_dir/workbench-$state_key.log"
restate_marker_file="$state_dir/restate-$state_key.container"
postgres_marker_file="$state_dir/postgres-$state_key.container"
restate_service_retirement_receipt_file="$state_dir/restate-$state_key.service-retired"
postgres_service_retirement_receipt_file="$state_dir/postgres-$state_key.service-retired"
reset_file="$state_dir/reset-$state_key.meta"
data_owner_file="$data_dir/.agent-workbench-dev-reset-owner"
data_path_hash="$(printf '%s' "$data_dir" | sha256sum | awk '{print $1}')"
data_creation_receipt_file="$data_dir/.agent-workbench-dev-attempt-owner"
run_owner_file="$state_dir/.agent-workbench-dev-run-owner-$state_key"
created_reset_ownership_this_attempt=0

case "$action" in
  up|start|foreground|run|restart|down|stop)
    command -v flock >/dev/null 2>&1 || die "flock is required for launcher lifecycle operations"
    launcher_lock_hash="$(printf '%s' "$repo_root" | sha256sum | awk '{print $1}')"
    launcher_lock_root="$(stable_launcher_runtime_root)"
    if path_contains_path "$data_dir" "$launcher_lock_root"; then
      die "application data path encloses launcher private runtime state"
    fi
    if path_overlaps_reset_owner "$launcher_lock_root"; then
      die "launcher private runtime path overlaps another launcher-owned disposable stack"
    fi
    if [[ ! -e "$launcher_lock_root" ]]; then
      mkdir -m 700 -- "$launcher_lock_root"
    fi
    private_owned_directory "$launcher_lock_root" \
      || die "unsafe launcher lock directory $launcher_lock_root"
    launcher_lock_file="$launcher_lock_root/$launcher_lock_hash.lock"
    launcher_data_lock_file="$launcher_lock_root/data-ownership.lock"
    restate_ingress_service_hash="$(printf '%s' "$canonical_ingress_host:$ingress_port" | sha256sum | awk '{print $1}')"
    restate_admin_service_hash="$(printf '%s' "$canonical_admin_host:$admin_port" | sha256sum | awk '{print $1}')"
    legacy_restate_service_hash="$(printf '%s' "$canonical_ingress_host:$ingress_port|$canonical_admin_host:$admin_port" | sha256sum | awk '{print $1}')"
    postgres_service_hash="$(printf '%s' "$canonical_postgres_host:$postgres_port" | sha256sum | awk '{print $1}')"
    restate_ingress_service_lease_file="$launcher_lock_root/restate-ingress-$restate_ingress_service_hash.lease"
    restate_admin_service_lease_file="$launcher_lock_root/restate-admin-$restate_admin_service_hash.lease"
    legacy_restate_service_lease_file="$launcher_lock_root/restate-$legacy_restate_service_hash.lease"
    postgres_service_lease_file="$launcher_lock_root/postgres-$postgres_service_hash.lease"
    reset_recovery_file="$launcher_lock_root/$launcher_lock_hash-$state_key-recover.sh"
    reset_finalization_file="$launcher_lock_root/$launcher_lock_hash-$state_key-reset-finalizing"
    start_finalization_file="$launcher_lock_root/$launcher_lock_hash-$state_key-start-finalizing"
    # Deliberately before the first `flock` below. A build says nothing about
    # who owns a stack, and the locks it would otherwise sit inside — this
    # checkout's lifecycle lock, and the box-wide data-ownership lock every
    # checkout on the machine shares — then hold for the length of a compile.
    # See `prepare_workbench_binary`.
    case "$action" in
      up|start)
        # `up` against a stack that is already serving is a no-op, and a no-op
        # must not compile. The readiness probe is a plain HTTP read of the
        # address this command names — it mutates nothing, so it needs no lock.
        # If the stack dies between this probe and the launch below, the
        # attempt refuses and says to run the same `up` again; that is a
        # better answer than compiling inside the locks to cover a race.
        if ! workbench_ready; then
          prepare_workbench_binary
        fi
        ;;
      foreground|run|restart)
        prepare_workbench_binary
        ;;
    esac
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
    if (( reset_dev_state )); then
      run_reset_dev_state
    else
      run_replace_process
    fi
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
