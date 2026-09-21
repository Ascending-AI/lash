#!/usr/bin/env bash
# Run one `bash -c` command per stdin line, all of them concurrently, wait for
# every one, and print a single table: command, PASS/FAIL, and one evidence
# line (the command's last non-empty output). Exits nonzero when any command
# failed; a failure never cancels the commands still running.
#
# Shared by the justfile's `floor` and `bump-check` recipes, which are the same
# gate over different command lists.
set -uo pipefail

work_dir="$(mktemp -d)"
trap 'rm -rf -- "$work_dir"' EXIT

commands=()
while IFS= read -r line || [[ -n "$line" ]]; do
  [[ -n "${line//[[:space:]]/}" ]] || continue
  commands+=("$line")
done
if ((${#commands[@]} == 0)); then
  printf 'gate-table: no commands on stdin\n' >&2
  exit 2
fi

pids=()
for index in "${!commands[@]}"; do
  # `( … ) &`, not `bash -c … &`: a non-interactive shell masks SIGINT in a
  # simple background command and the ignore disposition propagates through
  # exec, so spawned subprocess trees inside a leg would silently swallow
  # interrupts (test_with_service.py's Ctrl-C teardown check fails on it).
  ( bash -c "${commands[$index]}" >"$work_dir/$index.log" 2>&1 ) &
  pids+=("$!")
done

failures=0
for index in "${!commands[@]}"; do
  if wait "${pids[$index]}"; then
    printf 'PASS\n' >"$work_dir/$index.status"
  else
    printf 'FAIL\n' >"$work_dir/$index.status"
    failures=$((failures + 1))
  fi
done

width=7
for command in "${commands[@]}"; do
  ((${#command} > width)) && width=${#command}
done

printf '%-*s  %-4s  %s\n' "$width" command status evidence
for index in "${!commands[@]}"; do
  evidence="$(awk '
    /PASSED|passed|FAIL|failed|error|Executed|ok\.|NO STATUS/ { strong = $0 }
    NF { last = $0 }
    END { print strong != "" ? strong : last }
  ' "$work_dir/$index.log")"
  [[ -n "$evidence" ]] || evidence='(no output)'
  printf '%-*s  %-4s  %s\n' "$width" "${commands[$index]}" \
    "$(cat "$work_dir/$index.status" 2>/dev/null || echo '?')" "$evidence"
done

if ((failures > 0)); then
  printf '%s of %s commands failed\n' "$failures" "${#commands[@]}" >&2
  for index in "${!commands[@]}"; do
    [[ "$(cat "$work_dir/$index.status" 2>/dev/null)" == FAIL ]] || continue
    printf -- '--- output of: %s ---\n' "${commands[$index]}" >&2
    cat "$work_dir/$index.log" >&2
    printf -- '--- end: %s ---\n' "${commands[$index]}" >&2
  done
  exit 1
fi
