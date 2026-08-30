#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
test_tmp="$(mktemp -d)"
declare -a owned_processes=()

process_start_time() {
  local pid="$1"
  [[ -r "/proc/$pid/stat" ]] || return 1
  awk '{print $22}' "/proc/$pid/stat"
}

cleanup() {
  local record pid expected_start current_start
  for record in "${owned_processes[@]}"; do
    read -r pid expected_start <<<"$record"
    current_start="$(process_start_time "$pid" 2>/dev/null || true)"
    if [[ -n "$current_start" && "$current_start" = "$expected_start" ]]; then
      kill "$pid" >/dev/null 2>&1 || true
    fi
  done
  rm -rf -- "$test_tmp"
}
trap cleanup EXIT

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

spawn_owned_sleep() {
  local launch_file="$test_tmp/launch-$RANDOM"
  bash -c 'setsid sleep 120 >/dev/null 2>&1 & printf "%s\n" "$!"' > "$launch_file"
  SPAWNED_PID="$(<"$launch_file")"
  SPAWNED_START=""
  for _ in {1..100}; do
    SPAWNED_START="$(process_start_time "$SPAWNED_PID" 2>/dev/null || true)"
    [[ -n "$SPAWNED_START" ]] && break
    sleep 0.01
  done
  [[ "$SPAWNED_START" =~ ^[0-9]+$ ]] || fail "throwaway sleep $SPAWNED_PID never appeared in /proc"
  owned_processes+=("$SPAWNED_PID $SPAWNED_START")
}

assert_identity_alive() {
  local pid="$1" expected_start="$2" current_start
  current_start="$(process_start_time "$pid" 2>/dev/null || true)"
  [[ "$current_start" = "$expected_start" ]] \
    || fail "expected throwaway process $pid with start time $expected_start to remain alive"
}

assert_identity_gone() {
  local pid="$1" expected_start="$2" current_start
  current_start="$(process_start_time "$pid" 2>/dev/null || true)"
  [[ "$current_start" != "$expected_start" ]] \
    || fail "expected managed throwaway process $pid to stop"
}

run_agent_workbench_cases() {
  local run_dir="$test_tmp/agent-workbench/run"
  local address="127.0.0.1:3032"
  local pid_file="$run_dir/workbench-127.0.0.1_3032.pid"
  local output="$test_tmp/agent-workbench.log"
  mkdir -p "$run_dir"

  spawn_owned_sleep
  printf '%s %s\n' "$SPAWNED_PID" "$SPAWNED_START" > "$pid_file"
  AGENT_WORKBENCH_RUN_DIR="$run_dir" \
    bash "$repo_root/scripts/agent-workbench-dev.sh" down --addr "$address" \
    > "$output" 2>&1
  [[ ! -e "$pid_file" ]] || fail "agent-workbench happy-path PID file survived down"
  assert_identity_gone "$SPAWNED_PID" "$SPAWNED_START"
  grep -Fq "stopping process $SPAWNED_PID" "$output" \
    || fail "agent-workbench happy path did not report the stopped PID"

  spawn_owned_sleep
  printf '%s %s\n' "$SPAWNED_PID" "$((SPAWNED_START + 1))" > "$pid_file"
  AGENT_WORKBENCH_RUN_DIR="$run_dir" \
    bash "$repo_root/scripts/agent-workbench-dev.sh" down --addr "$address" \
    >> "$output" 2>&1
  [[ ! -e "$pid_file" ]] || fail "agent-workbench mismatched PID file was not removed"
  assert_identity_alive "$SPAWNED_PID" "$SPAWNED_START"
  grep -Fq "removing stale or mismatched PID file $pid_file" "$output" \
    || fail "agent-workbench mismatched PID file removal was not reported"

  printf '%s\n' 'agent-workbench identity cases:'
  sed -n '/stopping process/p;/removing stale or mismatched PID file/p' "$output"
}

assert_agent_workbench_default_derivation() {
  local workbench_port="$1"
  local expected_offset="$2"
  local output="$test_tmp/agent-workbench-trace-$workbench_port.log"
  local status
  env -u AGENT_WORKBENCH_RESTATE_ADDR \
    -u RESTATE_INGRESS_URL \
    -u RESTATE_ADMIN_URL \
    -u AGENT_WORKBENCH_RESTATE_ADMIN_PORT \
    -u AGENT_WORKBENCH_RESTATE_NODE_PORT \
    -u AGENT_WORKBENCH_POSTGRES \
    -u AGENT_WORKBENCH_POSTGRES_PORT \
    -u AGENT_WORKBENCH_DATABASE_URL \
    AGENT_WORKBENCH_RUN_DIR="$test_tmp/agent-workbench/trace-$workbench_port/run" \
    bash -x "$repo_root/scripts/agent-workbench-dev.sh" status --port "$workbench_port" \
    > "$output" 2>&1 || status=$?
  [[ "${status:-0}" = 1 ]] || fail "agent-workbench status trace for port $workbench_port exited ${status:-0}"

  grep -Fqx "+ port_offset=$expected_offset" "$output" \
    || fail "agent-workbench port $workbench_port used an unexpected offset"
  grep -Fqx "+ default_restate_endpoint_port=$((9081 + expected_offset))" "$output" \
    || fail "agent-workbench port $workbench_port used an unexpected endpoint port"
  grep -Fqx "+ default_restate_ingress_port=$((8080 + expected_offset))" "$output" \
    || fail "agent-workbench port $workbench_port used an unexpected ingress port"
  grep -Fqx "+ default_restate_admin_port=$((19070 + expected_offset))" "$output" \
    || fail "agent-workbench port $workbench_port used an unexpected admin port"
  grep -Fqx "+ default_restate_node_port=$((19071 + expected_offset))" "$output" \
    || fail "agent-workbench port $workbench_port used an unexpected node port"
  grep -Fqx "+ default_postgres_port=$((15432 + expected_offset))" "$output" \
    || fail "agent-workbench port $workbench_port used an unexpected Postgres port"
}

run_agent_workbench_port_cases() {
  local output="$test_tmp/agent-workbench-port.log"
  local status

  env -u AGENT_WORKBENCH_RESTATE_ADDR \
    -u RESTATE_INGRESS_URL \
    -u RESTATE_ADMIN_URL \
    -u AGENT_WORKBENCH_RESTATE_ADMIN_PORT \
    -u AGENT_WORKBENCH_RESTATE_NODE_PORT \
    -u AGENT_WORKBENCH_POSTGRES \
    -u AGENT_WORKBENCH_POSTGRES_PORT \
    -u AGENT_WORKBENCH_DATABASE_URL \
    AGENT_WORKBENCH_RUN_DIR="$test_tmp/agent-workbench/high/run" \
    bash "$repo_root/scripts/agent-workbench-dev.sh" status --port 21440 \
    > "$output" 2>&1 || status=$?
  [[ "${status:-0}" = 1 ]] || fail "high-port derivation unexpectedly exited ${status:-0}"
  grep -Fq "cannot derive managed service ports for workbench port 21440" "$output" \
    || fail "high-port derivation did not explain the explicit override requirement"

  status=0
  AGENT_WORKBENCH_RUN_DIR="$test_tmp/agent-workbench/explicit-high/run" \
    AGENT_WORKBENCH_RESTATE_ADDR=127.0.0.1:49081 \
    RESTATE_INGRESS_URL=http://127.0.0.1:48080 \
    RESTATE_ADMIN_URL=http://127.0.0.1:49070 \
    AGENT_WORKBENCH_RESTATE_NODE_PORT=49071 \
    AGENT_WORKBENCH_POSTGRES=1 \
    AGENT_WORKBENCH_POSTGRES_PORT=45432 \
    bash "$repo_root/scripts/agent-workbench-dev.sh" status --port 21440 \
    > "$output" 2>&1 || status=$?
  [[ "$status" = 1 ]] || fail "explicit high-port status exited $status instead of reporting stopped"
  ! grep -Fq "cannot derive managed service ports" "$output" \
    || fail "explicit high-port overrides were rejected"

  assert_agent_workbench_default_derivation 3030 0
  assert_agent_workbench_default_derivation 3032 20
  printf '%s\n' 'agent-workbench port derivation cases passed'
}

run_slack_clone_cases() {
  local state_root="$test_tmp/slack-clone"
  local run_dir="$state_root/run"
  local address="127.0.0.1:3040"
  local pid_file="$run_dir/platform-127.0.0.1_3040.pid"
  local output="$test_tmp/slack-clone.log"
  mkdir -p "$run_dir"

  spawn_owned_sleep
  printf '%s %s\n' "$SPAWNED_PID" "$SPAWNED_START" > "$pid_file"
  SLACK_CLONE_STATE_DIR="$state_root" \
    bash "$repo_root/scripts/slack-clone-dev.sh" down --addr "$address" \
    > "$output" 2>&1
  [[ ! -e "$pid_file" ]] || fail "slack-clone happy-path PID file survived down"
  assert_identity_gone "$SPAWNED_PID" "$SPAWNED_START"
  grep -Fq "stopping platform (process $SPAWNED_PID)" "$output" \
    || fail "slack-clone happy path did not report the stopped PID"

  spawn_owned_sleep
  printf '%s %s\n' "$SPAWNED_PID" "$((SPAWNED_START + 1))" > "$pid_file"
  SLACK_CLONE_STATE_DIR="$state_root" \
    bash "$repo_root/scripts/slack-clone-dev.sh" down --addr "$address" \
    >> "$output" 2>&1
  [[ ! -e "$pid_file" ]] || fail "slack-clone mismatched PID file was not removed"
  assert_identity_alive "$SPAWNED_PID" "$SPAWNED_START"
  grep -Fq "removing stale or mismatched platform PID file $pid_file" "$output" \
    || fail "slack-clone mismatched PID file removal was not reported"

  printf '%s\n' 'slack-clone identity cases:'
  sed -n '/stopping platform/p;/removing stale or mismatched platform PID file/p' "$output"
}

run_agent_workbench_cases
run_agent_workbench_port_cases
run_slack_clone_cases
printf '%s\n' 'dev script process identity checks passed'
