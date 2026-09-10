#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
script="${CI_RECLAIM_SCRIPT:-$repo_root/scripts/ci-reclaim-disk.sh}"
test_tmp="$(mktemp -d "${TMPDIR:-/tmp}/lash-ci-reclaim-test.XXXXXX")"
wait_seconds="${CI_RECLAIM_TEST_WAIT_SECONDS:-5}"
active_pid=""

cleanup() {
  if [[ "$active_pid" =~ ^[0-9]+$ ]]; then
    kill -KILL -- "-$active_pid" >/dev/null 2>&1 || true
    wait "$active_pid" 2>/dev/null || true
  fi
  rm -rf -- "$test_tmp"
}
trap cleanup EXIT

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

wait_for_file() {
  local path="$1" deadline=$((SECONDS + wait_seconds))
  while [[ ! -f "$path" ]]; do
    ((SECONDS < deadline)) || return 1
    sleep 0.05
  done
}

wait_for_exit() {
  local pid="$1" deadline=$((SECONDS + wait_seconds))
  while kill -0 "$pid" 2>/dev/null; do
    ((SECONDS < deadline)) || return 1
    sleep 0.05
  done
}

write_mock_commands() {
  local bin_dir="$1"

  cat >"$bin_dir/sudo" <<'SUDO'
#!/usr/bin/env bash
set -euo pipefail
exec "$@"
SUDO

  cat >"$bin_dir/rm" <<'RM'
#!/usr/bin/env bash
set -euo pipefail
printf 'rm %s\n' "$*" >>"$CI_RECLAIM_MOCK_LOG"
exit "${CI_RECLAIM_MOCK_RM_STATUS:-0}"
RM

  cat >"$bin_dir/df" <<'DF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' 'Filesystem      Size  Used Avail Use% Mounted on' 'mock            1G   1M   1G   1% /'
DF

  cat >"$bin_dir/docker" <<'DOCKER'
#!/usr/bin/env bash
set -euo pipefail

printf 'docker %s\n' "$*" >>"$CI_RECLAIM_MOCK_LOG"
if [[ "${1:-}" == image && "${2:-}" == prune ]]; then
  sleep "${CI_RECLAIM_MOCK_START_DELAY_SECONDS:-0}"
  printf '%s\n' started >"$CI_RECLAIM_MOCK_STATE/prune-started"
  while [[ ! -e "$CI_RECLAIM_MOCK_STATE/release-prune" ]]; do
    sleep 0.05
  done
  while [[ "${CI_RECLAIM_MOCK_HANG_AFTER_RELEASE:-0}" == 1 ]]; do
    sleep 0.05
  done
  printf '%s\n' 'deleted: mock-image'
  printf '%s\n' finished >"$CI_RECLAIM_MOCK_STATE/prune-finished"
  exit "${CI_RECLAIM_MOCK_DOCKER_STATUS:-0}"
fi
exit 0
DOCKER

  chmod +x "$bin_dir"/*
}

run_case() {
  local name="$1" expected_status="$2" docker_status="$3" rm_status="$4"
  local bin_dir="$test_tmp/$name/bin"
  local state_dir="$test_tmp/$name/state" output="$test_tmp/$name/output.log"
  local log_file="$test_tmp/$name/commands.log" status

  mkdir -p "$bin_dir" "$state_dir"
  write_mock_commands "$bin_dir"

  PATH="$bin_dir:$PATH" \
    CI_RECLAIM_MOCK_STATE="$state_dir" \
    CI_RECLAIM_MOCK_LOG="$log_file" \
    CI_RECLAIM_MOCK_DOCKER_STATUS="$docker_status" \
    CI_RECLAIM_MOCK_RM_STATUS="$rm_status" \
  setsid bash "$script" >"$output" 2>&1 &
  active_pid=$!

  wait_for_file "$state_dir/prune-started" \
    || fail "$name: mock prune did not start within ${wait_seconds}s\n$(sed -n '1,120p' "$output")"

  sleep 0.1
  kill -0 "$active_pid" 2>/dev/null \
    || fail "$name: reclaim script returned before prune completed"
  [[ ! -f "$state_dir/prune-finished" ]] \
    || fail "$name: mock prune completed before the test released it"

  : >"$state_dir/release-prune"
  wait_for_exit "$active_pid" \
    || fail "$name: reclaim script did not finish within ${wait_seconds}s after prune release"
  set +e
  wait "$active_pid"
  status=$?
  set -e
  active_pid=""
  [[ "$status" -eq "$expected_status" ]] \
    || fail "$name: reclaim script exited $status, expected $expected_status\n$(sed -n '1,120p' "$output")"
  [[ -f "$state_dir/prune-finished" ]] \
    || fail "$name: reclaim script returned without observing prune completion"
  grep -Fq 'deleted: mock-image' "$output" \
    || fail "$name: prune deletion output was not retained\n$(sed -n '1,120p' "$output")"
  grep -Fq 'docker image prune started at ' "$output" \
    || fail "$name: prune start timestamp was not retained\n$(sed -n '1,120p' "$output")"
  grep -Fq 'docker image prune finished at ' "$output" \
    || fail "$name: prune finish timestamp was not retained\n$(sed -n '1,120p' "$output")"
  grep -Fq 'docker image prune exit status ' "$output" \
    || fail "$name: prune exit status was not retained\n$(sed -n '1,120p' "$output")"
  if [[ "$docker_status" -ne 0 ]]; then
    grep -Fq 'docker image prune failed; continuing with best-effort runner cleanup' "$output" \
      || fail "$name: prune failure was not handled as best effort\n$(sed -n '1,120p' "$output")"
  fi
  grep -Fq 'docker image prune --all --force' "$log_file" \
    || fail "$name: mock prune invocation was not recorded"
  grep -Fq 'rm -rf /usr/share/dotnet /usr/local/lib/android /opt/ghc /opt/hostedtoolcache/CodeQL /usr/local/share/boost' "$log_file" \
    || fail "$name: mock runner cleanup invocation was not recorded"

  printf '%s case passed (status %s)\n' "$name" "$status"
}

run_case successful-prune 0 0 0
run_case failed-cleanup 0 17 23
printf '%s\n' 'ci reclaim disk checks passed'
