#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
. "$repo/env.sh"
cd "$repo"

workbench_target="$(python3 scripts/resolve_buck2_target.py \
  //examples/agent-workbench agent-workbench --feature provider-wire-fixtures)"

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

wait_log_pattern() {
  local runner="$1" log="$2" pattern="$3" attempt
  for attempt in $(seq 1 6000); do
    if grep -q "$pattern" "$log"; then
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

# The third argument is the reap budget in seconds, default 30. A workbench
# host needs more: its shutdown deletes the session it opened against the
# Parallel Search MCP server at boot, which is a live network round trip. The
# example-core rows keep 30s: they have no such cost.
wait_reaped() {
  local pid="$1" label="$2" budget="${3:-30}"
  if ! timeout "${budget}s" tail --pid="$pid" -f /dev/null >/dev/null 2>&1; then
    echo "$label did not exit within $budget seconds" >&2
    return 1
  fi
  set +e
  wait "$pid"
  wait_status=$?
  set -e
  forget_pid "$pid"
}

# Match the executable, not the runner's arguments (which also name the target).
app_descendant() {
  local runner="$1" expected="$2"
  local attempt parent child child_file executable
  for attempt in $(seq 1 100); do
    executable="$(readlink "/proc/$runner/exe" 2>/dev/null || true)"
    if [[ "$executable" == *"$expected" ]]; then
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
            executable="$(readlink "/proc/$child/exe" 2>/dev/null || true)"
            if [[ "$executable" == *"$expected" ]]; then
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
  echo "could not find $expected descendant of build runner $runner" >&2
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

run_workbench_signal_with_streams_and_fixture() {
  local dir="$artifact_root/workbench-signal-streams"
  mkdir -p "$dir/data"
  local port marker log trace runner app events observations count
  port="$(free_port)"
  marker="$dir/shutdown.marker"
  log="$dir/host.log"
  trace="$dir/trace.jsonl"
  env AGENT_WORKBENCH_PROTOCOL=standard \
    AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=valid-empty-completion \
    AGENT_WORKBENCH_ADDR="127.0.0.1:$port" \
    AGENT_WORKBENCH_DATA_DIR="$dir/data" \
    AGENT_WORKBENCH_TRACE="$trace" \
    AGENT_WORKBENCH_OPEN=0 \
    LASH_HOST_SHUTDOWN_MARKER="$marker" \
    kiln run --config=judged "$workbench_target" \
    >"$log" 2>&1 &
  runner=$!
  owned_pids+=("$runner")
  wait_http "http://127.0.0.1:$port/healthz" "$runner" "$log"
  curl --silent --show-error --no-buffer --dump-header "$dir/events.headers" \
    "http://127.0.0.1:$port/api/events" >"$dir/events.ndjson" 2>"$dir/events.stderr" &
  events=$!
  owned_pids+=("$events")
  curl --silent --show-error --no-buffer --dump-header "$dir/observations.headers" \
    "http://127.0.0.1:$port/api/observations" >"$dir/observations.ndjson" 2>"$dir/observations.stderr" &
  observations=$!
  owned_pids+=("$observations")
  wait_log_pattern "$events" "$dir/events.headers" '200 OK'
  wait_log_pattern "$observations" "$dir/observations.headers" '200 OK'
  kill -0 "$events"
  kill -0 "$observations"
  curl --silent --show-error --fail --max-time 30 -X POST \
    -H 'content-type: application/json' \
    --data '{"text":"Run valid empty completion.","model_variant":""}' \
    "http://127.0.0.1:$port/api/turn" >"$dir/valid-empty-response.json"
  python3 - "$port" "$dir" <<'PY'
import json, pathlib, sys, time, urllib.request

port, directory = sys.argv[1], pathlib.Path(sys.argv[2])
accepted = json.loads((directory / "valid-empty-response.json").read_text())
assert accepted["accepted"] and not accepted["queued"], accepted
turn_id = accepted["turn_id"]
deadline = time.monotonic() + 30
while time.monotonic() < deadline:
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/api/state", timeout=2) as response:
        state = json.load(response)
    (directory / "valid-empty-state.json").write_text(json.dumps(state, indent=2) + "\n")
    events = state["product_events"]["events"]
    lines = (directory / "events.ndjson").read_text().splitlines(keepends=True)
    streamed = [json.loads(line)["event"] for line in lines if line.endswith("\n")
                and json.loads(line).get("type") == "event"]
    done = [event for event in streamed if event["type"] == "done" and event.get("turn_id") == turn_id]
    if done and not state["active_turns"]:
        break
    time.sleep(0.1)
else:
    raise AssertionError(f"valid empty turn {turn_id} did not finish within 30 seconds")
assert len(done) == 1 and done[0].get("outcome", "completed") == "completed", done
replies = [row for row in state["transcript"] if row["kind"] == "assistant_reply"
           and row["provenance"]["turn_id"] == turn_id]
assert len(replies) == 1 and replies[0]["content"]["text"] == "", replies
calls = [event for event in events if event["type"] == "model_call_recorded"]
assert len(calls) == 1, calls
assert calls[0]["event_id"].startswith(f"turn:{turn_id}:model-call:"), calls
attempts = calls[0]["record"]["attempts"]
assert len(attempts) == 1, attempts
attempt = attempts[0]
assert attempt["outcome"] == "completed", attempt
assert attempt["protocol_position"] == "terminal_observed", attempt
assert "error" not in attempt and "retry_decision" not in attempt, attempt
assert attempt["evidence"]["provider_finish_reason"] == "stop", attempt
assert attempt["usage"]["input_tokens"] == 7 and attempt["usage"]["output_tokens"] == 0, attempt
print(f"valid empty completion: turn={turn_id} calls=1 attempts=1 assistant_bytes=0 usage=7/0")
PY
  app="$(app_descendant "$runner" /agent_workbench)"
  kill -TERM "$app"
  wait_reaped "$runner" workbench-signal 90
  assert_count "$wait_status" 0 workbench-signal-exit
  if kill -0 "$app" 2>/dev/null; then
    echo "agent-workbench app child remained live after build runner exit" >&2
    return 1
  fi
  wait_reaped "$events" workbench-events-stream 90
  wait_reaped "$observations" workbench-observations-stream 90
  count="$(marker_count "$marker" agent-workbench)"
  assert_count "$count" 1 workbench-signal-marker
  grep -q 'agent-workbench shutdown complete' "$log"
  grep -q 'development provider scenario enabled: valid-empty-completion' "$log"
  test -f "$trace"
  printf 'workbench-signal-active-streams\t%s\tpresent\tyes\tevents and observations streams reaped\n' "$count" >>"$scorecard"
  printf 'workbench-valid-empty-composer\t%s\tpresent\tyes\tStandard empty reply completed before signal\n' "$count" >>"$scorecard"
}

run_workbench_bind_error() {
  local dir="$artifact_root/workbench-bind-error"
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
  env AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=auth-failure-once \
    AGENT_WORKBENCH_ADDR="127.0.0.1:$port" \
    AGENT_WORKBENCH_DATA_DIR="$dir/data" \
    AGENT_WORKBENCH_TRACE="$trace" \
    AGENT_WORKBENCH_OPEN=0 \
    LASH_HOST_SHUTDOWN_MARKER="$marker" \
    kiln run --config=judged "$workbench_target" >"$log" 2>&1 &
  runner=$!
  owned_pids+=("$runner")
  wait_log_pattern "$runner" "$log" 'agent-workbench listening on'
  wait_reaped "$runner" workbench-bind-error 90
  if [[ "$wait_status" == 0 ]]; then
    echo "agent-workbench bind-error command unexpectedly succeeded" >&2
    return 1
  fi
  kill -TERM "$holder"
  wait_reaped "$holder" workbench-bind-holder 90
  count="$(marker_count "$marker" agent-workbench)"
  assert_count "$count" 1 workbench-bind-error-marker
  grep -q 'Address already in use' "$log"
  test -f "$trace"
  printf 'workbench-bind-error\t%s\tpresent\tyes\tprimary bind error retained\n' "$count" >>"$scorecard"
}

run_workbench_signal_with_streams_and_fixture
run_workbench_bind_error

printf 'artifact_root=%s\n' "$artifact_root"
cat "$scorecard"
