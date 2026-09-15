#!/usr/bin/env bash
# Run one gate command per line of stdin, concurrently, and report each one's
# wall time.
#
# `Test repository scripts` ran its self-tests as a serial list of shell lines,
# which cost 269 s of the 306 s `Repository gates` job while three of the
# runner's four cores sat idle: the commands are independent processes over a
# read-only checkout, and two of them (the launcher self-tests) account for
# nearly half the total on their own.
#
# Two rules make the concurrency sound:
#
#   * `--serial` names the commands that may not run beside each other. The
#     agent-workbench and slack-clone launcher self-tests drive
#     `scripts/agent-workbench-dev.sh`, which takes a box-wide `flock -n` on
#     `/tmp/lash-agent-workbench-$UID/data-ownership.lock` and *refuses* rather
#     than waits when it is held, so two of them at once would fail on
#     contention instead of on a defect. They run as one serial stream beside
#     the pool.
#   * Every command runs to completion even after one fails, and each one's
#     output is held and printed whole under its own heading. Interleaved
#     output from concurrent gates is unreadable, and a first failure that
#     cancels the rest turns one red gate into several rounds.
#
# Usage: run-gate-commands.sh [--jobs N] [--serial REGEX] < commands
set -uo pipefail

jobs_limit=""
serial_pattern=""
while (($#)); do
  case "$1" in
    --jobs)
      jobs_limit="${2:-}"
      shift 2
      ;;
    --serial)
      serial_pattern="${2:-}"
      shift 2
      ;;
    *)
      printf 'usage: %s [--jobs N] [--serial REGEX] < commands\n' "$0" >&2
      exit 2
      ;;
  esac
done

if [[ -z "$jobs_limit" ]]; then
  jobs_limit="$(nproc 2>/dev/null || echo 4)"
fi
if ! [[ "$jobs_limit" =~ ^[1-9][0-9]*$ ]]; then
  printf 'run-gate-commands: --jobs expects a positive integer, got %s\n' \
    "$jobs_limit" >&2
  exit 2
fi

work_dir="$(mktemp -d)"
trap 'rm -rf -- "$work_dir"' EXIT

commands=()
serial_indexes=()
parallel_indexes=()
while IFS= read -r line || [[ -n "$line" ]]; do
  [[ -n "${line//[[:space:]]/}" ]] || continue
  [[ "${line#"${line%%[![:space:]]*}"}" != '#'* ]] || continue
  commands+=("$line")
  index=$((${#commands[@]} - 1))
  if [[ -n "$serial_pattern" && "$line" =~ $serial_pattern ]]; then
    serial_indexes+=("$index")
  else
    parallel_indexes+=("$index")
  fi
done

if ((${#commands[@]} == 0)); then
  printf 'run-gate-commands: no commands on stdin\n' >&2
  exit 2
fi

run_one() {
  local index="$1"
  local started ended status
  started="$(date +%s.%N)"
  bash -c "${commands[index]}" > "$work_dir/$index.log" 2>&1
  status=$?
  ended="$(date +%s.%N)"
  printf '%s\n' "$status" > "$work_dir/$index.status"
  printf '%s\n' "$(awk "BEGIN { printf \"%.1f\", $ended - $started }")" \
    > "$work_dir/$index.seconds"
}

run_serial_stream() {
  local index
  for index in "$@"; do
    run_one "$index"
  done
}

pool_limit="$jobs_limit"
if ((${#serial_indexes[@]} > 0)); then
  run_serial_stream "${serial_indexes[@]}" &
  if ((pool_limit > 1)); then
    pool_limit=$((pool_limit - 1))
  fi
fi

running=0
for index in "${parallel_indexes[@]}"; do
  if ((running >= pool_limit)); then
    wait -n
    running=$((running - 1))
  fi
  run_one "$index" &
  running=$((running + 1))
done
wait

failures=()
for index in "${!commands[@]}"; do
  status="$(cat "$work_dir/$index.status" 2>/dev/null || echo 'no status')"
  seconds="$(cat "$work_dir/$index.seconds" 2>/dev/null || echo '?')"
  printf '::group::[%ss, exit %s] %s\n' "$seconds" "$status" "${commands[index]}"
  cat "$work_dir/$index.log" 2>/dev/null
  printf '::endgroup::\n'
  if [[ "$status" != 0 ]]; then
    failures+=("${commands[index]}")
  fi
done

printf '\nwall time per command (slowest first):\n'
for index in "${!commands[@]}"; do
  printf '%s\t%s\n' \
    "$(cat "$work_dir/$index.seconds" 2>/dev/null || echo '?')" \
    "${commands[index]}"
done | sort -rn

if ((${#failures[@]} > 0)); then
  printf '\n%s of %s gate commands failed:\n' \
    "${#failures[@]}" "${#commands[@]}" >&2
  printf -- '- %s\n' "${failures[@]}" >&2
  exit 1
fi

printf '\n%s gate commands passed\n' "${#commands[@]}"
