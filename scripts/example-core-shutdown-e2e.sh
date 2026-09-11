#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
. "$repo/env.sh"

artifact_root="${LASH_HOST_SHUTDOWN_ARTIFACT_DIR:-/tmp/lash-example-core-shutdown-$$}"
mkdir -p "$artifact_root"
scorecard="$artifact_root/scorecard.tsv"
printf 'case\tmarker_count\ttrace\tprocess_reaped\tdetail\n' >"$scorecard"

owned_pids=()
wait_status=0
forget_pid() {
  local forgotten="$1" pid
  local remaining=()
  for pid in "${owned_pids[@]}"; do
    if [[ "$pid" != "$forgotten" ]]; then
      remaining+=("$pid")
    fi
  done
  owned_pids=("${remaining[@]}")
}

cleanup() {
  local pid
  for pid in "${owned_pids[@]}"; do
    if kill -0 "$pid" 2>/dev/null; then
      kill -TERM "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
}
trap cleanup EXIT

free_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

wait_http() {
  local url="$1" runner="$2" log="$3"
  local attempt
  for attempt in $(seq 1 6000); do
    if curl --silent --fail --max-time 1 "$url" >/dev/null 2>&1; then
      return
    fi
    if ! kill -0 "$runner" 2>/dev/null; then
      cat "$log" >&2
      return 1
    fi
    sleep 0.1
  done
  cat "$log" >&2
  return 1
}

wait_reaped() {
  local pid="$1" label="$2"
  if ! timeout 30s tail --pid="$pid" -f /dev/null >/dev/null 2>&1; then
    echo "$label did not exit within 30 seconds" >&2
    return 1
  fi
  set +e
  wait "$pid"
  wait_status=$?
  set -e
  forget_pid "$pid"
}

app_descendant() {
  local runner="$1" expected="$2"
  local attempt parent child child_file cmdline
  for attempt in $(seq 1 100); do
    cmdline="$(tr '\0' ' ' <"/proc/$runner/cmdline" 2>/dev/null || true)"
    if [[ "$cmdline" == *"$expected"* ]]; then
      printf '%s\n' "$runner"
      return
    fi
    local frontier=("$runner")
    local next=()
    while ((${#frontier[@]})); do
      next=()
      for parent in "${frontier[@]}"; do
        for child_file in /proc/"$parent"/task/*/children; do
          if [[ -r "$child_file" ]]; then
            for child in $(cat "$child_file"); do
            cmdline="$(tr '\0' ' ' <"/proc/$child/cmdline" 2>/dev/null || true)"
            if [[ "$cmdline" == *"$expected"* ]]; then
              printf '%s\n' "$child"
              return
            fi
            next+=("$child")
            done
          fi
        done
      done
      frontier=("${next[@]}")
    done
    sleep 0.1
  done
  echo "could not find $expected descendant of cargo runner $runner" >&2
  return 1
}

marker_count() {
  local marker="$1" host="$2"
  grep -c "host=$host plugin_factory=host_shutdown_marker phase=shutdown_completed" "$marker"
}

wait_listener_ready() {
  local pid="$1" log="$2" attempt
  for attempt in $(seq 1 200); do
    if grep -q ready "$log"; then
      return
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
      cat "$log" >&2
      return 1
    fi
    sleep 0.05
  done
  echo "owned conflict listener did not become ready" >&2
  return 1
}

assert_count() {
  local actual="$1" expected="$2" label="$3"
  if [[ "$actual" != "$expected" ]]; then
    echo "$label: expected $expected, got $actual" >&2
    return 1
  fi
}

run_agent_service_signal() {
  local dir="$artifact_root/agent-service-signal"
  mkdir -p "$dir/data"
  local port marker log trace runner app count
  port="$(free_port)"
  marker="$dir/shutdown.marker"
  log="$dir/host.log"
  trace="$dir/trace.jsonl"
  env OPENROUTER_API_KEY=deterministic-no-network \
    AGENT_SERVICE_ADDR="127.0.0.1:$port" \
    AGENT_SERVICE_DATA_DIR="$dir/data" \
    AGENT_SERVICE_TRACE="$trace" \
    LASH_HOST_SHUTDOWN_MARKER="$marker" \
    cargo run -p agent-service --profile judged --locked >"$log" 2>&1 &
  runner=$!
  owned_pids+=("$runner")
  wait_http "http://127.0.0.1:$port/" "$runner" "$log"
  app="$(app_descendant "$runner" target/judged/agent-service)"
  kill -TERM "$app"
  wait_reaped "$runner" agent-service-signal
  assert_count "$wait_status" 0 agent-service-signal-exit
  if kill -0 "$app" 2>/dev/null; then
    echo "agent-service app child remained live after cargo runner exit" >&2
    return 1
  fi
  count="$(marker_count "$marker" agent-service)"
  assert_count "$count" 1 agent-service-signal-marker
  grep -q 'agent-service shutdown complete' "$log"
  test ! -e "$trace"
  printf 'agent-service-signal\t%s\tempty-flush-returned\tyes\tSIGTERM graceful shutdown\n' "$count" >>"$scorecard"
}

run_agent_service_bind_error() {
  local dir="$artifact_root/agent-service-bind-error"
  mkdir -p "$dir/data"
  local port marker log trace holder runner count
  port="$(free_port)"
  marker="$dir/shutdown.marker"
  log="$dir/host.log"
  trace="$dir/trace.jsonl"
  python3 - "$port" >"$dir/listener.log" 2>&1 <<'PY' &
import socket, sys, time
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", int(sys.argv[1])))
s.listen()
print("ready", flush=True)
time.sleep(120)
PY
  holder=$!
  owned_pids+=("$holder")
  wait_listener_ready "$holder" "$dir/listener.log"
  env OPENROUTER_API_KEY=deterministic-no-network \
    AGENT_SERVICE_ADDR="127.0.0.1:$port" \
    AGENT_SERVICE_DATA_DIR="$dir/data" \
    AGENT_SERVICE_TRACE="$trace" \
    LASH_HOST_SHUTDOWN_MARKER="$marker" \
    cargo run -p agent-service --profile judged --locked >"$log" 2>&1 &
  runner=$!
  owned_pids+=("$runner")
  wait_reaped "$runner" agent-service-bind-error
  if [[ "$wait_status" == 0 ]]; then
    echo "agent-service bind-error command unexpectedly succeeded" >&2
    return 1
  fi
  kill -TERM "$holder"
  wait_reaped "$holder" agent-service-bind-holder
  count="$(marker_count "$marker" agent-service)"
  assert_count "$count" 1 agent-service-bind-error-marker
  grep -q 'Address already in use' "$log"
  test ! -e "$trace"
  printf 'agent-service-bind-error\t%s\tempty-flush-returned\tyes\tprimary bind error retained\n' "$count" >>"$scorecard"
}

run_workbench_signal_with_streams_and_fixture() {
  local dir="$artifact_root/workbench-signal-streams"
  mkdir -p "$dir/data"
  local port restate_port marker log trace runner app events observations count nested_count
  port="$(free_port)"
  restate_port="$(free_port)"
  marker="$dir/shutdown.marker"
  log="$dir/host.log"
  trace="$dir/trace.jsonl"
  env AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=valid-empty-completion \
    AGENT_WORKBENCH_ADDR="127.0.0.1:$port" \
    AGENT_WORKBENCH_RESTATE_ADDR="127.0.0.1:$restate_port" \
    AGENT_WORKBENCH_DATA_DIR="$dir/data" \
    AGENT_WORKBENCH_TRACE="$trace" \
    AGENT_WORKBENCH_OPEN=0 \
    LASH_HOST_SHUTDOWN_MARKER="$marker" \
    cargo run -p agent-workbench --profile judged --locked --features provider-wire-fixtures \
    >"$log" 2>&1 &
  runner=$!
  owned_pids+=("$runner")
  wait_http "http://127.0.0.1:$port/healthz" "$runner" "$log"
  curl --silent --show-error --fail --max-time 30 -X POST \
    "http://127.0.0.1:$port/dev/valid-empty-completion" >"$dir/valid-empty-response.json"
  count="$(marker_count "$marker" agent-workbench-valid-empty)"
  assert_count "$count" 1 valid-empty-nested-marker
  curl --silent --show-error --no-buffer \
    "http://127.0.0.1:$port/api/events" >"$dir/events.ndjson" 2>"$dir/events.stderr" &
  events=$!
  owned_pids+=("$events")
  curl --silent --show-error --no-buffer \
    "http://127.0.0.1:$port/api/observations" >"$dir/observations.ndjson" 2>"$dir/observations.stderr" &
  observations=$!
  owned_pids+=("$observations")
  sleep 0.2
  kill -0 "$events"
  kill -0 "$observations"
  app="$(app_descendant "$runner" target/judged/agent-workbench)"
  kill -TERM "$app"
  wait_reaped "$runner" workbench-signal
  assert_count "$wait_status" 0 workbench-signal-exit
  if kill -0 "$app" 2>/dev/null; then
    echo "agent-workbench app child remained live after cargo runner exit" >&2
    return 1
  fi
  wait_reaped "$events" workbench-events-stream
  wait_reaped "$observations" workbench-observations-stream
  count="$(marker_count "$marker" agent-workbench)"
  nested_count="$(marker_count "$marker" agent-workbench-valid-empty)"
  assert_count "$count" 1 workbench-signal-marker
  assert_count "$nested_count" 1 valid-empty-nested-marker-final
  grep -q 'agent-workbench shutdown complete' "$log"
  grep -q 'development provider scenario enabled: valid-empty-completion' "$log"
  test -f "$trace"
  printf 'workbench-signal-active-streams\t%s\tpresent\tyes\tevents and observations streams reaped\n' "$count" >>"$scorecard"
  printf 'workbench-valid-empty-nested\t%s\tpresent\tyes\tnested core shutdown before response\n' "$nested_count" >>"$scorecard"
}

run_workbench_bind_error() {
  local dir="$artifact_root/workbench-bind-error"
  mkdir -p "$dir/data"
  local port restate_port marker log trace holder runner count
  port="$(free_port)"
  restate_port="$(free_port)"
  marker="$dir/shutdown.marker"
  log="$dir/host.log"
  trace="$dir/trace.jsonl"
  python3 - "$port" >"$dir/listener.log" 2>&1 <<'PY' &
import socket, sys, time
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", int(sys.argv[1])))
s.listen()
print("ready", flush=True)
time.sleep(120)
PY
  holder=$!
  owned_pids+=("$holder")
  wait_listener_ready "$holder" "$dir/listener.log"
  env AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=auth-failure-once \
    AGENT_WORKBENCH_ADDR="127.0.0.1:$port" \
    AGENT_WORKBENCH_RESTATE_ADDR="127.0.0.1:$restate_port" \
    AGENT_WORKBENCH_DATA_DIR="$dir/data" \
    AGENT_WORKBENCH_TRACE="$trace" \
    AGENT_WORKBENCH_OPEN=0 \
    LASH_HOST_SHUTDOWN_MARKER="$marker" \
    cargo run -p agent-workbench --profile judged --locked >"$log" 2>&1 &
  runner=$!
  owned_pids+=("$runner")
  wait_reaped "$runner" workbench-bind-error
  if [[ "$wait_status" == 0 ]]; then
    echo "agent-workbench bind-error command unexpectedly succeeded" >&2
    return 1
  fi
  kill -TERM "$holder"
  wait_reaped "$holder" workbench-bind-holder
  count="$(marker_count "$marker" agent-workbench)"
  assert_count "$count" 1 workbench-bind-error-marker
  grep -q 'Address already in use' "$log"
  test -f "$trace"
  printf 'workbench-bind-error\t%s\tpresent\tyes\tprimary bind error retained\n' "$count" >>"$scorecard"
}

run_agent_service_signal
run_agent_service_bind_error
run_workbench_signal_with_streams_and_fixture
run_workbench_bind_error

printf 'artifact_root=%s\n' "$artifact_root"
cat "$scorecard"
