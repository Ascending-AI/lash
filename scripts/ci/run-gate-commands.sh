#!/usr/bin/env bash
# Run one gate command per line of stdin, concurrently, and report each one's
# wall time.
# Launcher self-tests own isolated namespaces. Every command runs to completion,
# even after another fails, and its output is printed as one block.
# Usage: run-gate-commands.sh [--jobs N] [--discover] < extra commands
set -uo pipefail

# Self-tests must not write bytecode into the checkout: a sibling command
# copying the scripts tree would race the writer's temporary .pyc.
export PYTHONDONTWRITEBYTECODE=1

jobs_limit=""
discover=0
while (($#)); do
  case "$1" in
    --discover)
      discover=1
      shift
      ;;
    --jobs)
      if (($# < 2)); then
        printf 'run-gate-commands: --jobs requires a value\n' >&2
        exit 2
      fi
      jobs_limit="${2:-}"
      shift 2
      ;;
    *)
      printf 'usage: %s [--jobs N] [--discover] < extra commands\n' "$0" >&2
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
while IFS= read -r line || [[ -n "$line" ]]; do
  [[ -n "${line//[[:space:]]/}" ]] || continue
  [[ "${line#"${line%%[![:space:]]*}"}" != '#'* ]] || continue
  commands+=("$line")

done

if ((discover)); then
  repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
  discovered="$(python3 "$repo/scripts/ci/repository_gate_commands.py")" || exit $?
  declare -A queued=()
  for command in "${commands[@]}"; do
    queued["$command"]=1
  done
  while IFS= read -r line; do
    if [[ -n "$line" && -z "${queued["$line"]+present}" ]]; then
      commands+=("$line")
      queued["$line"]=1
    fi
  done <<< "$discovered"
fi

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

running=0
for index in "${!commands[@]}"; do
  if ((running >= jobs_limit)); then
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
