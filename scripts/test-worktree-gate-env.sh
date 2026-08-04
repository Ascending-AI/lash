#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
helper="$repo/scripts/worktree-gate-env.sh"
test_tmp="$(mktemp -d "${TMPDIR:-/tmp}/lash-gate-env-test.XXXXXX")"
holder_pid=""
leaked_child_pid=""

cleanup() {
  [ -z "$holder_pid" ] || kill "$holder_pid" >/dev/null 2>&1 || true
  [ -z "$leaked_child_pid" ] || kill "$leaked_child_pid" >/dev/null 2>&1 || true
  [ ! -d "$test_tmp" ] || rm -rf "$test_tmp"
}
trap cleanup EXIT

fail() {
  echo "worktree gate regression failed: $*" >&2
  exit 1
}

wait_for_unlocked() {
  local lock_path="$1" deadline=$((SECONDS + 5))
  until flock -n "$lock_path" true; do
    ((SECONDS < deadline)) || fail "lock did not release: $lock_path"
    sleep 0.05
  done
}

# shellcheck source=scripts/worktree-gate-env.sh
source "$helper"

mkdir -p "$test_tmp/a/checkout" "$test_tmp/b/checkout"
slug_a="$(lash_gate_slug_for_root "$test_tmp/a/checkout")"
slug_b="$(lash_gate_slug_for_root "$test_tmp/b/checkout")"
[ "$slug_a" != "$slug_b" ] || fail "same-basename paths derived the same slug"
[[ "$slug_a" == checkout-* && "$slug_b" == checkout-* ]] \
  || fail "path-qualified slugs lost the sanitized basename"

override_slot=$(((LASH_GATE_PORT_SLOT + 17) % 90))
override_base="$(
  LASH_GATE_SLOT_OVERRIDE="$override_slot" bash -c \
    'source "$1"; printf "%s" "$LASH_E2E_PORT_BASE"' _ "$helper"
)"
[ "$override_base" -eq $((61000 + override_slot * 50)) ] \
  || fail "LASH_GATE_SLOT_OVERRIDE did not control the derived port base"

ready="$test_tmp/holder-ready"
holder_log="$test_tmp/holder.log"
LASH_GATE_SLOT_OVERRIDE="$override_slot" bash -c '
  set -euo pipefail
  source "$1"
  lash_gate_acquire_locks regression-holder
  printf "ready\n" >"$2"
  sleep 30
' _ "$helper" "$ready" >"$holder_log" 2>&1 &
holder_pid=$!
deadline=$((SECONDS + 10))
until [ -f "$ready" ]; do
  kill -0 "$holder_pid" 2>/dev/null || fail "lock holder exited before readiness"
  ((SECONDS < deadline)) || fail "timed out waiting for lock holder"
  sleep 0.1
done

refusal_log="$test_tmp/refusal.log"
set +e
LASH_GATE_SLOT_OVERRIDE="$override_slot" bash -c '
  set -euo pipefail
  source "$1"
  lash_gate_acquire_locks regression-contender
' _ "$helper" >"$refusal_log" 2>&1
refusal_status=$?
set -e
[ "$refusal_status" -eq 73 ] || fail "contender exited $refusal_status, expected 73"
grep -Fq "PID $holder_pid" "$refusal_log" || fail "refusal omitted owner PID"
grep -Fq "$LASH_GATE_WORKTREE_LOCK_PATH" "$refusal_log" || fail "refusal omitted lock path"
grep -Fq "kill $holder_pid" "$refusal_log" || fail "refusal omitted exact orphan remedy"

kill "$holder_pid"
wait "$holder_pid" 2>/dev/null || true
holder_pid=""
wait_for_unlocked "$LASH_GATE_WORKTREE_LOCK_PATH"
wait_for_unlocked "$LASH_GATE_STATE_ROOT/port-slot-${override_slot}.lock"

leaked_pid_file="$test_tmp/leaked-child.pid"
LASH_GATE_SLOT_OVERRIDE="$override_slot" bash -c '
  set -euo pipefail
  source "$1"
  lash_gate_acquire_locks regression-leaky-parent
  setsid sleep 30 >/dev/null 2>&1 &
  printf "%s\n" "$!" >"$2"
' _ "$helper" "$leaked_pid_file"
leaked_child_pid="$(sed -n '1p' "$leaked_pid_file")"
kill -0 "$leaked_child_pid" 2>/dev/null || fail "leaked child did not survive parent"

reacquired=0
for _ in 1 2 3 4 5 6 7 8 9 10; do
  if LASH_GATE_SLOT_OVERRIDE="$override_slot" bash -c '
    set -euo pipefail
    source "$1"
    lash_gate_acquire_locks regression-after-leak
  ' _ "$helper" >"$test_tmp/reacquire.log" 2>&1; then
    reacquired=1
    break
  fi
  sleep 0.1
done
[ "$reacquired" -eq 1 ] \
  || fail "orphaned child retained a gate lock: $(tr '\n' ' ' <"$test_tmp/reacquire.log")"
kill "$leaked_child_pid"
wait "$leaked_child_pid" 2>/dev/null || true
leaked_child_pid=""

printf 'worktree gate env regressions passed: distinct_slugs=%s,%s override_slot=%s leaked_child_lock=released\n' \
  "$slug_a" "$slug_b" "$override_slot"
