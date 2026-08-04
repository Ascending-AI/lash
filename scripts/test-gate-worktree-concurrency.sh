#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
peer="${1:-}"
if [ -z "$peer" ] || ! git -C "$peer" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "Usage: $0 <peer-worktree>" >&2
  exit 2
fi
peer="$(cd "$peer" && git rev-parse --show-toplevel)"

# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"
proof_root="${LASH_GATE_PROOF_OUT_DIR:-$repo/target/gate-concurrency-proof/$LASH_GATE_WORKTREE_SLUG}"
mkdir -p "$proof_root"
leftover_container=""

cleanup() {
  if [ -n "$leftover_container" ]; then
    docker rm -f "$leftover_container" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

run_smoke() {
  local root="$1"
  local log="$2"
  (
    cd "$root"
    just gate-container-smoke
  ) >"$log" 2>&1
}

parallel_started_at="$SECONDS"
run_smoke "$repo" "$proof_root/worktree-a.log" &
worktree_a_job=$!
run_smoke "$peer" "$proof_root/worktree-b.log" &
worktree_b_job=$!
parallel_status=0
wait "$worktree_a_job" || parallel_status=1
wait "$worktree_b_job" || parallel_status=1
if [ "$parallel_status" -ne 0 ]; then
  echo "Concurrent worktree proof failed; logs: $proof_root" >&2
  exit 1
fi
parallel_elapsed=$((SECONDS - parallel_started_at))

LASH_GATE_SMOKE_HOLD_SECONDS=8 run_smoke "$repo" "$proof_root/same-worktree-first.log" &
first_job=$!
deadline=$((SECONDS + 60))
until grep -q 'gate container smoke ready:' "$proof_root/same-worktree-first.log" 2>/dev/null; do
  if ! kill -0 "$first_job" >/dev/null 2>&1; then
    wait "$first_job"
    echo "First same-worktree smoke exited before reaching ready state." >&2
    exit 1
  fi
  ((SECONDS < deadline)) || { echo "Timed out waiting for same-worktree smoke." >&2; exit 1; }
  sleep 1
done

set +e
run_smoke "$repo" "$proof_root/same-worktree-second.log"
second_status=$?
set -e
if [ "$second_status" -ne 73 ]; then
  echo "Second same-worktree run exited $second_status; expected clean refusal 73." >&2
  exit 1
fi
grep -q "already running for worktree '${LASH_GATE_WORKTREE_SLUG}'" \
  "$proof_root/same-worktree-second.log"
wait "$first_job"

leftover_container="lash-gate-proof-leftover-${LASH_GATE_WORKTREE_SLUG}"
docker create --name "$leftover_container" --label "$LASH_GATE_LABEL" \
  postgres:16-alpine true >/dev/null
set +e
run_smoke "$repo" "$proof_root/leftover-refusal.log"
leftover_status=$?
set -e
if [ "$leftover_status" -ne 73 ]; then
  echo "Leftover-container run exited $leftover_status; expected clean refusal 73." >&2
  exit 1
fi
grep -q "owns leftover gate containers" "$proof_root/leftover-refusal.log"
grep -q "docker rm -f $leftover_container" "$proof_root/leftover-refusal.log"
docker rm -f "$leftover_container" >/dev/null
leftover_container=""

{
  printf 'parallel_elapsed_seconds=%s\n' "$parallel_elapsed"
  sed -n '/gate container smoke ready:/p' "$proof_root/worktree-a.log"
  sed -n '/gate container smoke ready:/p' "$proof_root/worktree-b.log"
  printf 'same_worktree_second_exit=%s\n' "$second_status"
  sed -n '/Refusing to start/p' "$proof_root/same-worktree-second.log"
  printf 'leftover_refusal_exit=%s\n' "$leftover_status"
  sed -n '/Refusing to start:/p;/docker rm -f/p' "$proof_root/leftover-refusal.log"
} | tee "$proof_root/summary.txt"
