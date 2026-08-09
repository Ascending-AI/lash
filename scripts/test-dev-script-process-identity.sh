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
run_slack_clone_cases
printf '%s\n' 'dev script process identity checks passed'
