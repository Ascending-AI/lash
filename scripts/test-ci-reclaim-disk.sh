#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
script="$repo_root/scripts/ci-reclaim-disk.sh"
test_tmp="$(mktemp -d "${TMPDIR:-/tmp}/lash-ci-reclaim-test.XXXXXX")"
declare -a owned_process_groups=()

cleanup() {
  local pgid
  for pgid in "${owned_process_groups[@]}"; do
    [[ "$pgid" =~ ^[0-9]+$ ]] || continue
    kill -KILL -- "-$pgid" >/dev/null 2>&1 || true
  done
  rm -rf -- "$test_tmp"
}
trap cleanup EXIT

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
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
  printf '%s\n' started >"$CI_RECLAIM_MOCK_STATE/prune-started"
  while [[ ! -e "$CI_RECLAIM_MOCK_STATE/release-prune" ]]; do
    :
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
  local name="$1" expected_status="$2" docker_status="$3"
  local case_dir="$test_tmp/$name" bin_dir="$test_tmp/$name/bin"
  local state_dir="$test_tmp/$name/state" output="$test_tmp/$name/output.log"
  local log_file="$test_tmp/$name/commands.log" pid status deadline

  mkdir -p "$bin_dir" "$state_dir"
  write_mock_commands "$bin_dir"

  PATH="$bin_dir:$PATH" \
    CI_RECLAIM_MOCK_STATE="$state_dir" \
    CI_RECLAIM_MOCK_LOG="$log_file" \
    CI_RECLAIM_MOCK_DOCKER_STATUS="$docker_status" \
  setsid bash "$script" >"$output" 2>&1 &
  pid=$!
  owned_process_groups+=("$pid")

  deadline=$((SECONDS + 5))
  while [[ ! -f "$state_dir/prune-started" && SECONDS -lt deadline ]]; do
    :
  done
  [[ -f "$state_dir/prune-started" ]] || {
    wait "$pid" 2>/dev/null || true
    fail "$name: mock prune did not start\n$(sed -n '1,120p' "$output")"
  }

  kill -0 "$pid" 2>/dev/null \
    || fail "$name: reclaim script returned before prune completed"
  [[ ! -f "$state_dir/prune-finished" ]] \
    || fail "$name: mock prune completed before the test released it"

  : >"$state_dir/release-prune"
  set +e
  wait "$pid"
  status=$?
  set -e
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

  printf '%s case passed (status %s)\n' "$name" "$status"
}

run_case successful-prune 0 0
run_case failed-prune 0 17
printf '%s\n' 'ci reclaim disk checks passed'
