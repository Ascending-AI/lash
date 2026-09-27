#!/usr/bin/env bash
# Dev driver for the examples/slack-clone processes: the platform, the local
# restate-server the bot's turns run on, the bot, and the HTTP-served MCP server
# the bot can attach at runtime. Mirrors
# scripts/agent-workbench-dev.sh (detached `up`, `status`, `logs`, `down`, state
# under a run directory), with the one structural difference that matters here:
# this example is several processes, and the bot registers itself with the
# platform at boot, so `up` starts them in order and waits for the registration
# to land. The MCP server is deliberately *not* wired into the bot's boot: it is
# an integration an operator attaches over the bot's admin API while it serves.
#
# The restate-server is lash's zero-infra effect engine (ADR 0104 section 4):
# the pinned release binary from `scripts/ci/restate_suite.py server-path`, with
# its data under the state directory so it outlives a bot restart. The bot
# serves its Restate endpoint on a stable port and registers it at boot; the
# server re-drives a killed boot's in-flight turn on the restarted bot.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

state_root="${SLACK_CLONE_STATE_DIR:-.slack-clone}"
state_dir="$state_root/run"
mkdir -p "$state_dir"

log() {
  printf '[slack-clone] %s\n' "$*" >&2
}

die() {
  printf '[slack-clone] error: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'USAGE'
Usage:
  scripts/slack-clone-dev.sh [up]        [--port PORT | --addr HOST:PORT]
  scripts/slack-clone-dev.sh restart     [--port PORT | --addr HOST:PORT]
  scripts/slack-clone-dev.sh status      [--port PORT | --addr HOST:PORT]
  scripts/slack-clone-dev.sh logs        [--port PORT | --addr HOST:PORT] [-f]
  scripts/slack-clone-dev.sh down        [--port PORT | --addr HOST:PORT]
  scripts/slack-clone-dev.sh platform-foreground [--port PORT | --addr HOST:PORT]

Defaults:
  up is detached and idempotent; it starts the platform and the local
  restate-server, waits for them, then starts the bot and waits for the bot to
  register its Events API request URL, and finally starts the HTTP MCP server
  the bot can attach at runtime.
  The bot's port is the platform port + 1, the MCP server's is the platform
  port + 2. The restate-server's ingress, admin and node ports are the
  platform port + 3, + 4 and + 5, and the bot serves its Restate endpoint on
  the platform port + 6.
  Without --port/--addr, SLACK_CLONE_ADDR is used, then 127.0.0.1:3040.
  State (SQLite stores, Restate data, traces, pids, logs) lives under
  .slack-clone/.
  OPENROUTER_API_KEY is required for the bot; the platform needs no key.
  SLACK_CLONE_MCP_HTTP_TOKEN overrides the HTTP MCP server's bearer token.
  SLACK_CLONE_OPEN=0 suppresses opening a browser.
USAGE
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
  timeout 1 bash -c "cat < /dev/null > /dev/tcp/$1/$2" >/dev/null 2>&1
}

wait_tcp() {
  local label="$1" host="$2" port="$3" timeout_seconds="${4:-60}"
  local deadline=$((SECONDS + timeout_seconds))
  until tcp_ready "$host" "$port"; do
    if (( SECONDS >= deadline )); then
      log "$label did not become ready at $host:$port"
      return 1
    fi
    sleep 0.5
  done
}

open_browser() {
  case "${SLACK_CLONE_OPEN:-1}" in
    0|false|no) return 0 ;;
  esac
  local url="$1"
  if command -v xdg-open >/dev/null 2>&1; then
    xdg-open "$url" >/dev/null 2>&1 || true
  elif command -v open >/dev/null 2>&1; then
    open "$url" >/dev/null 2>&1 || true
  fi
}

# ---------------------------------------------------------------- arguments ---

action="up"
case "${1:-}" in
  up|restart|status|logs|down|platform-foreground)
    action="$1"
    shift
    ;;
  -h|--help)
    usage
    exit 0
    ;;
  "") ;;
  --*) ;;
  *) die "unknown action '$1'" ;;
esac

platform_addr="${SLACK_CLONE_ADDR:-127.0.0.1:3040}"
follow_logs=0
while (($#)); do
  case "$1" in
    --port)
      [[ $# -ge 2 ]] || die "--port needs a value"
      validate_port platform "$2"
      platform_addr="127.0.0.1:$2"
      shift 2
      ;;
    --addr)
      [[ $# -ge 2 ]] || die "--addr needs a value"
      platform_addr="$2"
      shift 2
      ;;
    -f|--follow)
      follow_logs=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *) die "unknown argument '$1'" ;;
  esac
done

read -r platform_host platform_port <<<"$(addr_host_port "$platform_addr")"
validate_port platform "$platform_port"
# The bot sits one port above the platform, so two workspaces on one machine
# never collide as long as their platform ports differ by more than one.
bot_port=$((10#$platform_port + 1))
bot_addr="$platform_host:$bot_port"
# The runtime-attachable MCP server sits one port above the bot.
mcp_http_port=$((10#$platform_port + 2))
mcp_http_addr="$platform_host:$mcp_http_port"
platform_url="http://$platform_addr"
bot_url="http://$bot_addr"
mcp_http_url="http://$mcp_http_addr/mcp"
# The local restate-server and the bot's Restate endpoint sit above the MCP
# server.
restate_ingress_port=$((10#$platform_port + 3))
restate_admin_port=$((10#$platform_port + 4))
restate_node_port=$((10#$platform_port + 5))
restate_endpoint_port=$((10#$platform_port + 6))
validate_port "bot Restate endpoint" "$restate_endpoint_port"
restate_ingress_url="http://$platform_host:$restate_ingress_port"
restate_admin_url="http://$platform_host:$restate_admin_port"

state_key="$(printf '%s' "$platform_addr" | tr -c 'A-Za-z0-9_.-' '_')"
platform_pid_file="$state_dir/platform-$state_key.pid"
bot_pid_file="$state_dir/bot-$state_key.pid"
mcp_http_pid_file="$state_dir/mcp-http-$state_key.pid"
restate_pid_file="$state_dir/restate-$state_key.pid"
platform_log="$state_dir/platform-$state_key.log"
bot_log="$state_dir/bot-$state_key.log"
mcp_http_log="$state_dir/mcp-http-$state_key.log"
restate_log="$state_dir/restate-$state_key.log"
data_root="$state_root/$state_key"

# ------------------------------------------------------------- process glue ---

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
  local label="$1" file="$2"
  if [[ -e "$file" ]]; then
    log "removing stale or mismatched $label PID file $file"
  fi
  rm -f "$file"
}

pid_alive() {
  local label="$1" file="$2"
  [[ -e "$file" ]] || return 1
  if pid_file_identity "$file" >/dev/null; then
    return 0
  fi
  remove_stale_pid_file "$label" "$file"
  return 1
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

require_alive() {
  local label="$1" pid_file="$2" log_file="$3"
  if ! pid_alive "$label" "$pid_file"; then
    log "$label exited; last log lines:"
    tail -n 40 "$log_file" >&2 || true
    die "$label is not running"
  fi
}

# This launcher serves both runbook layers, and they want different builds. A
# judged browser row scores what the host ships, so it boots the `judged`
# profile (`runbooks/RULES.md`); the scripted full-host gate is deterministic
# evidence rather than a product judgement, so it keeps `dev` and its debug
# assertions. `scripts/slack-clone-full-host-e2e.sh` is the only caller that
# overrides this.
cargo_profile="${SLACK_CLONE_CARGO_PROFILE:-judged}"

# Cargo's artifact directory is not the profile name: the built-in `dev` profile
# writes to `target/debug`, and only `dev` is renamed this way. Every other
# profile — `release` and custom ones like `judged` — uses its own name. Getting
# this wrong points the launcher at a path that never exists, and the failure
# reads as "the binary is missing" rather than "the mapping is wrong".
profile_artifact_dir() {
  case "$cargo_profile" in
    dev) printf 'debug' ;;
    *) printf '%s' "$cargo_profile" ;;
  esac
}

# Launch the binary cargo just built: honor CARGO_TARGET_DIR, or a stale binary
# in the repo-local target/ boots instead of the fresh build.
binary_path() {
  printf '%s/%s/%s' "${CARGO_TARGET_DIR:-$repo_root/target}" "$(profile_artifact_dir)" "$1"
}

start_detached() {
  local label="$1" pid_file="$2" log_file="$3"
  shift 3
  printf '\n[%s] starting %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$label" >> "$log_file"
  if command -v setsid >/dev/null 2>&1; then
    setsid env "$@" >> "$log_file" 2>&1 < /dev/null &
  else
    nohup env "$@" >> "$log_file" 2>&1 < /dev/null &
  fi
  local pid="$!"
  write_pid_file "$pid_file" "$pid" || die "could not record $label process identity for $pid"
  log "started $label as process $pid; log: $log_file"
}

platform_env() {
  printf '%s\n' \
    "SLACK_CLONE_ADDR=$platform_addr" \
    "SLACK_CLONE_DATA_DIR=$data_root/platform"
}

# The server's data lives under the workspace's data root, so a restarted
# server keeps every invocation a killed bot left in flight. The authority id
# is stable for the same reason: it names this workspace's durable state.
restate_env() {
  printf '%s\n' \
    "RESTATE_BASE_DIR=$(cd "$data_root" && pwd)/restate" \
    "RESTATE_NODE_NAME=n1" \
    "RESTATE_CLUSTER_NAME=slack-clone-$state_key" \
    "RESTATE_LISTEN_MODE=tcp" \
    "RESTATE_BIND_IP=$platform_host" \
    "RESTATE_BIND_PORT=$restate_node_port" \
    "RESTATE_INGRESS__BIND_ADDRESS=$platform_host:$restate_ingress_port" \
    "RESTATE_ADMIN__BIND_ADDRESS=$platform_host:$restate_admin_port" \
    "RESTATE_DEFAULT_NUM_PARTITIONS=1" \
    "RESTATE_ROCKSDB_TOTAL_MEMORY_SIZE=256MB" \
    "RESTATE_LOG_FILTER=warn,restate=info" \
    "RESTATE_LOG_FORMAT=compact" \
    "RESTATE_LOG_DISABLE_ANSI_CODES=true"
}

bot_env() {
  printf '%s\n' \
    "SLACK_CLONE_BOT_ADDR=$bot_addr" \
    "SLACK_CLONE_API_BASE_URL=$platform_url" \
    "SLACK_CLONE_BOT_DATA_DIR=$data_root/bot" \
    "SLACK_CLONE_BOT_RESTATE_ENDPOINT_ADDR=$platform_host:$restate_endpoint_port" \
    "RESTATE_INGRESS_URL=$restate_ingress_url" \
    "RESTATE_ADMIN_URL=$restate_admin_url" \
    "RESTATE_AUTHORITY_ID=slack-clone:$state_key"
}

restate_ready() {
  curl -fsS "$restate_admin_url/health" >/dev/null 2>&1 \
    && curl -fsS "$restate_ingress_url/restate/health" >/dev/null 2>&1
}

wait_restate() {
  local deadline=$((SECONDS + 60))
  until restate_ready; do
    require_alive restate "$restate_pid_file" "$restate_log"
    if (( SECONDS >= deadline )); then
      log "restate-server never became healthy; last log lines:"
      tail -n 40 "$restate_log" >&2 || true
      return 1
    fi
    sleep 0.2
  done
}

mcp_http_env() {
  printf '%s\n' \
    "SLACK_CLONE_MCP_HTTP_ADDR=$mcp_http_addr" \
    "SLACK_CLONE_MCP_HTTP_TOKEN=${SLACK_CLONE_MCP_HTTP_TOKEN:-slack-clone-mcp-http-dev-token}"
}

events_registered() {
  curl -fsS "$platform_url/healthz" 2>/dev/null | grep -q '"events_verified":true'
}

wait_registered() {
  local deadline=$((SECONDS + 60))
  until events_registered; do
    require_alive platform "$platform_pid_file" "$platform_log"
    require_alive bot "$bot_pid_file" "$bot_log"
    if (( SECONDS >= deadline )); then
      log "the bot never registered its Events API request url; last bot log:"
      tail -n 40 "$bot_log" >&2 || true
      return 1
    fi
    sleep 0.5
  done
}

build_binaries() {
  log "building slack-clone (profile: $cargo_profile)"
  local -a feature_args=()
  if [[ "${SLACK_CLONE_E2E_PROVIDER:-}" == "scripted-v1" ]]; then
    feature_args=(--features e2e)
  fi
  cargo build -p slack-clone --locked --profile "$cargo_profile" "${feature_args[@]}"
}

stop_one() {
  local label="$1" pid_file="$2"
  [[ -e "$pid_file" ]] || return 0
  local record="" pid="" start_time=""
  record="$(pid_file_identity "$pid_file" 2>/dev/null || true)"
  if [[ -z "$record" ]]; then
    remove_stale_pid_file "$label" "$pid_file"
    return 0
  fi
  read -r pid start_time <<<"$record"
  log "stopping $label (process $pid)"
  if ! signal_verified_process TERM "$pid" "$start_time"; then
    remove_stale_pid_file "$label" "$pid_file"
    return 0
  fi
  local deadline=$((SECONDS + 15))
  while pid_identity_matches "$pid" "$start_time"; do
    if (( SECONDS >= deadline )); then
      log "$label did not exit; sending SIGKILL"
      if ! signal_verified_process KILL "$pid" "$start_time"; then
        log "$label identity changed before SIGKILL; refusing to signal PID $pid"
      fi
      break
    fi
    sleep 0.5
  done
  rm -f "$pid_file"
}

# ----------------------------------------------------------------- actions ---

run_up() {
  build_binaries
  if pid_alive platform "$platform_pid_file"; then
    log "platform already running on $platform_addr"
  else
    mapfile -t env_pairs < <(platform_env)
    start_detached platform "$platform_pid_file" "$platform_log" \
      "${env_pairs[@]}" "$(binary_path slack-clone-platform)"
    wait_tcp platform "$platform_host" "$platform_port" \
      || require_alive platform "$platform_pid_file" "$platform_log"
  fi

  if pid_alive restate "$restate_pid_file"; then
    log "restate-server already running on $restate_ingress_url"
  else
    local restate_server
    restate_server="$(python3 "$repo_root/scripts/ci/restate_suite.py" server-path)" \
      || die "could not locate the pinned restate-server binary"
    mkdir -p "$data_root/restate"
    mapfile -t env_pairs < <(restate_env)
    # The invoker reaches the bot's endpoint on loopback directly; a proxy in
    # the caller's environment would route its calls through the proxy.
    start_detached restate "$restate_pid_file" "$restate_log" \
      -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY -u NO_PROXY \
      -u http_proxy -u https_proxy -u all_proxy -u no_proxy \
      "${env_pairs[@]}" "$restate_server" --no-logo
  fi
  wait_restate || die "the restate-server is not ready"

  # The bot also loads a repo-root .env itself, so only warn when neither source
  # can supply a key — a false alarm here reads as a real failure.
  if [[ "${SLACK_CLONE_E2E_PROVIDER:-}" != "scripted-v1" ]] \
    && [[ -z "${OPENROUTER_API_KEY:-}" ]] \
    && ! grep -qs '^[[:space:]]*OPENROUTER_API_KEY=' "$repo_root/.env"; then
    log "OPENROUTER_API_KEY is set neither in the environment nor in .env;"
    log "the bot will exit on boot. The platform alone is still usable at $platform_url."
  fi

  if pid_alive bot "$bot_pid_file"; then
    log "bot already running on $bot_addr"
  else
    mapfile -t env_pairs < <(bot_env)
    start_detached bot "$bot_pid_file" "$bot_log" \
      "${env_pairs[@]}" "$(binary_path slack-clone-bot)"
    wait_tcp bot "$platform_host" "$bot_port" \
      || require_alive bot "$bot_pid_file" "$bot_log"
  fi

  wait_registered || die "the bot is up but not receiving events"

  if pid_alive mcp-http "$mcp_http_pid_file"; then
    log "HTTP MCP server already running on $mcp_http_addr"
  else
    mapfile -t env_pairs < <(mcp_http_env)
    start_detached mcp-http "$mcp_http_pid_file" "$mcp_http_log" \
      "${env_pairs[@]}" "$(binary_path slack-clone-mcp-http-server)"
    wait_tcp mcp-http "$platform_host" "$mcp_http_port" \
      || require_alive mcp-http "$mcp_http_pid_file" "$mcp_http_log"
  fi

  log "platform: $platform_url"
  log "bot:      $bot_url (events at $bot_url/slack/events)"
  log "mcp:      $mcp_http_url (attach it via POST $bot_url/admin/mcp/servers)"
  log "restate:  $restate_admin_url (admin), $restate_ingress_url (ingress)"
  open_browser "$platform_url"
}

run_status() {
  local platform_state="stopped" bot_state="stopped" mcp_http_state="stopped"
  local restate_state="stopped"
  local record="" pid=""
  if pid_alive platform "$platform_pid_file"; then
    record="$(read_pid_file "$platform_pid_file")"
    read -r pid _ <<<"$record"
    platform_state="running ($pid)"
  fi
  if pid_alive bot "$bot_pid_file"; then
    record="$(read_pid_file "$bot_pid_file")"
    read -r pid _ <<<"$record"
    bot_state="running ($pid)"
  fi
  if pid_alive mcp-http "$mcp_http_pid_file"; then
    record="$(read_pid_file "$mcp_http_pid_file")"
    read -r pid _ <<<"$record"
    mcp_http_state="running ($pid)"
  fi
  if pid_alive restate "$restate_pid_file"; then
    record="$(read_pid_file "$restate_pid_file")"
    read -r pid _ <<<"$record"
    restate_state="running ($pid)"
  fi
  printf 'platform  %-24s %s\n' "$platform_addr" "$platform_state"
  printf 'bot       %-24s %s\n' "$bot_addr" "$bot_state"
  printf 'mcp-http  %-24s %s\n' "$mcp_http_addr" "$mcp_http_state"
  printf 'restate   %-24s %s\n' "$platform_host:$restate_ingress_port" "$restate_state"
  if health="$(curl -fsS "$platform_url/healthz" 2>/dev/null)"; then
    printf 'health    %s\n' "$health"
  fi
}

run_logs() {
  local files=()
  [[ -f "$platform_log" ]] && files+=("$platform_log")
  [[ -f "$bot_log" ]] && files+=("$bot_log")
  [[ -f "$mcp_http_log" ]] && files+=("$mcp_http_log")
  [[ -f "$restate_log" ]] && files+=("$restate_log")
  ((${#files[@]})) || die "no logs yet for $platform_addr"
  if (( follow_logs )); then
    tail -n 40 -F "${files[@]}"
  else
    tail -n 200 "${files[@]}"
  fi
}

run_down() {
  # Bot first: it is the guest, and stopping the platform under it would only
  # make its shutdown noisier. The MCP server outlives the bot on the way down
  # so an in-flight tool call fails against a live server rather than a
  # half-torn-down socket. The restate-server goes last: nothing is left to
  # drive, and its data stays under the state directory for the next `up`.
  stop_one bot "$bot_pid_file"
  stop_one mcp-http "$mcp_http_pid_file"
  stop_one platform "$platform_pid_file"
  stop_one restate "$restate_pid_file"
}

case "$action" in
  up) run_up ;;
  restart)
    run_down
    run_up
    ;;
  status) run_status ;;
  logs) run_logs ;;
  down) run_down ;;
  platform-foreground)
    build_binaries
    mapfile -t env_pairs < <(platform_env)
    exec env "${env_pairs[@]}" "$(binary_path slack-clone-platform)"
    ;;
  *) die "unknown action '$action'" ;;
esac
