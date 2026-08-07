#!/usr/bin/env bash
# Dev driver for the examples/slack-clone pair: one platform process and one bot
# process. Mirrors scripts/agent-workbench-dev.sh (detached `up`, `status`,
# `logs`, `down`, state under a run directory), with the one structural
# difference that matters here: this example is two processes, and the bot
# registers itself with the platform at boot, so `up` starts them in order and
# waits for the registration to land.
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
  up is detached and idempotent; it starts the platform, waits for it, then
  starts the bot and waits for the bot to register its Events API request URL.
  The bot's port is the platform port + 1.
  Without --port/--addr, SLACK_CLONE_ADDR is used, then 127.0.0.1:3040.
  State (SQLite stores, traces, pids, logs) lives under .slack-clone/.
  OPENROUTER_API_KEY is required for the bot; the platform needs no key.
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
platform_url="http://$platform_addr"
bot_url="http://$bot_addr"

state_key="$(printf '%s' "$platform_addr" | tr -c 'A-Za-z0-9_.-' '_')"
platform_pid_file="$state_dir/platform-$state_key.pid"
bot_pid_file="$state_dir/bot-$state_key.pid"
platform_log="$state_dir/platform-$state_key.log"
bot_log="$state_dir/bot-$state_key.log"
data_root="$state_root/$state_key"

# ------------------------------------------------------------- process glue ---

pid_alive() {
  local pid_file="$1"
  [[ -f "$pid_file" ]] || return 1
  local pid
  pid="$(cat "$pid_file" 2>/dev/null || true)"
  [[ -n "$pid" ]] || return 1
  kill -0 "$pid" 2>/dev/null
}

require_alive() {
  local label="$1" pid_file="$2" log_file="$3"
  if ! pid_alive "$pid_file"; then
    log "$label exited; last log lines:"
    tail -n 40 "$log_file" >&2 || true
    rm -f "$pid_file"
    die "$label is not running"
  fi
}

# Launch the binary cargo just built: honor CARGO_TARGET_DIR, or a stale binary
# in the repo-local target/ boots instead of the fresh build.
binary_path() {
  printf '%s/debug/%s' "${CARGO_TARGET_DIR:-$repo_root/target}" "$1"
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
  printf '%s\n' "$pid" > "$pid_file"
  log "started $label as process $pid; log: $log_file"
}

platform_env() {
  printf '%s\n' \
    "SLACK_CLONE_ADDR=$platform_addr" \
    "SLACK_CLONE_DATA_DIR=$data_root/platform"
}

bot_env() {
  printf '%s\n' \
    "SLACK_CLONE_BOT_ADDR=$bot_addr" \
    "SLACK_CLONE_API_BASE_URL=$platform_url" \
    "SLACK_CLONE_BOT_DATA_DIR=$data_root/bot"
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
  log "building slack-clone"
  cargo build -p slack-clone
}

stop_one() {
  local label="$1" pid_file="$2"
  if ! pid_alive "$pid_file"; then
    rm -f "$pid_file"
    return 0
  fi
  local pid
  pid="$(cat "$pid_file")"
  log "stopping $label (process $pid)"
  kill "$pid" 2>/dev/null || true
  local deadline=$((SECONDS + 15))
  while kill -0 "$pid" 2>/dev/null; do
    if (( SECONDS >= deadline )); then
      log "$label did not exit; sending SIGKILL"
      kill -9 "$pid" 2>/dev/null || true
      break
    fi
    sleep 0.5
  done
  rm -f "$pid_file"
}

# ----------------------------------------------------------------- actions ---

run_up() {
  build_binaries
  if pid_alive "$platform_pid_file"; then
    log "platform already running on $platform_addr"
  else
    mapfile -t env_pairs < <(platform_env)
    start_detached platform "$platform_pid_file" "$platform_log" \
      "${env_pairs[@]}" "$(binary_path slack-clone-platform)"
    wait_tcp platform "$platform_host" "$platform_port" \
      || require_alive platform "$platform_pid_file" "$platform_log"
  fi

  # The bot also loads a repo-root .env itself, so only warn when neither source
  # can supply a key — a false alarm here reads as a real failure.
  if [[ -z "${OPENROUTER_API_KEY:-}" ]] \
    && ! grep -qs '^[[:space:]]*OPENROUTER_API_KEY=' "$repo_root/.env"; then
    log "OPENROUTER_API_KEY is set neither in the environment nor in .env;"
    log "the bot will exit on boot. The platform alone is still usable at $platform_url."
  fi

  if pid_alive "$bot_pid_file"; then
    log "bot already running on $bot_addr"
  else
    mapfile -t env_pairs < <(bot_env)
    start_detached bot "$bot_pid_file" "$bot_log" \
      "${env_pairs[@]}" "$(binary_path slack-clone-bot)"
    wait_tcp bot "$platform_host" "$bot_port" \
      || require_alive bot "$bot_pid_file" "$bot_log"
  fi

  wait_registered || die "the bot is up but not receiving events"
  log "platform: $platform_url"
  log "bot:      $bot_url (events at $bot_url/slack/events)"
  open_browser "$platform_url"
}

run_status() {
  local platform_state="stopped" bot_state="stopped"
  pid_alive "$platform_pid_file" && platform_state="running ($(cat "$platform_pid_file"))"
  pid_alive "$bot_pid_file" && bot_state="running ($(cat "$bot_pid_file"))"
  printf 'platform  %-24s %s\n' "$platform_addr" "$platform_state"
  printf 'bot       %-24s %s\n' "$bot_addr" "$bot_state"
  if health="$(curl -fsS "$platform_url/healthz" 2>/dev/null)"; then
    printf 'health    %s\n' "$health"
  fi
}

run_logs() {
  local files=()
  [[ -f "$platform_log" ]] && files+=("$platform_log")
  [[ -f "$bot_log" ]] && files+=("$bot_log")
  ((${#files[@]})) || die "no logs yet for $platform_addr"
  if (( follow_logs )); then
    tail -n 40 -F "${files[@]}"
  else
    tail -n 200 "${files[@]}"
  fi
}

run_down() {
  # Bot first: it is the guest, and stopping the platform under it would only
  # make its shutdown noisier.
  stop_one bot "$bot_pid_file"
  stop_one platform "$platform_pid_file"
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
