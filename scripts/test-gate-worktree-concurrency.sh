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
compose_leftover_container=""
worktree_a_job=""
worktree_b_job=""
first_job=""

read_gate_identity() {
  local root="$1"
  env -u LASH_GATE_SLOT_OVERRIDE -u LASH_GATE_LOCK_HELD -u LASH_GATE_LOCK_SLUG \
    bash -c 'source "$1/scripts/worktree-gate-env.sh"; printf "%s %s %s\n" \
      "$LASH_GATE_WORKTREE_SLUG" "$LASH_GATE_PORT_SLOT" "$LASH_E2E_PORT_BASE"' \
      _ "$root"
}

read -r primary_slug primary_slot primary_base < <(read_gate_identity "$repo")
read -r peer_slug peer_slot peer_base < <(read_gate_identity "$peer")
[ "$primary_slug" != "$peer_slug" ] || {
  echo "Proof worktrees derived the same path-qualified identity '$primary_slug'." >&2
  exit 1
}
if [ "$primary_slot" -eq "$peer_slot" ]; then
  peer_slot=$(((peer_slot + 1) % 90))
  peer_base=$((61000 + peer_slot * 50))
fi

cleanup() {
  local job
  for job in "$worktree_a_job" "$worktree_b_job" "$first_job"; do
    if [ -n "$job" ] && kill -0 "$job" >/dev/null 2>&1; then
      kill "$job" >/dev/null 2>&1 || true
      wait "$job" 2>/dev/null || true
    fi
  done
  if [ -n "$leftover_container" ]; then
    docker rm -f "$leftover_container" >/dev/null 2>&1 || true
  fi
  if [ -n "$compose_leftover_container" ]; then
    docker rm -f "$compose_leftover_container" >/dev/null 2>&1 || true
  fi
  lash_gate_cleanup
}
trap cleanup EXIT

run_smoke() {
  local root="$1"
  local log="$2"
  local expected_slug="$3"
  local pinned_slot="$4"
  local status=0
  (
    cd "$root"
    unset LASH_GATE_LOCK_HELD LASH_GATE_LOCK_SLUG
    LASH_GATE_SLOT_OVERRIDE="$pinned_slot" just gate-container-smoke
  ) >"$log" 2>&1 || status=$?
  if [ "$status" -eq 0 ]; then
    grep -Fq "gate container smoke ready: slug=${expected_slug} " "$log"
  fi
  return "$status"
}

parallel_started_at="$SECONDS"
run_smoke "$repo" "$proof_root/worktree-a.log" "$primary_slug" "$primary_slot" &
worktree_a_job=$!
run_smoke "$peer" "$proof_root/worktree-b.log" "$peer_slug" "$peer_slot" &
worktree_b_job=$!
parallel_status=0
wait "$worktree_a_job" || parallel_status=1
worktree_a_job=""
wait "$worktree_b_job" || parallel_status=1
worktree_b_job=""
if [ "$parallel_status" -ne 0 ]; then
  echo "Concurrent worktree proof failed; logs: $proof_root" >&2
  exit 1
fi
parallel_elapsed=$((SECONDS - parallel_started_at))

LASH_GATE_SMOKE_HOLD_SECONDS=8 run_smoke \
  "$repo" "$proof_root/same-worktree-first.log" "$primary_slug" "$primary_slot" &
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
run_smoke "$repo" "$proof_root/same-worktree-second.log" "$primary_slug" "$primary_slot"
second_status=$?
set -e
if [ "$second_status" -ne 73 ]; then
  echo "Second same-worktree run exited $second_status; expected clean refusal 73." >&2
  exit 1
fi
grep -q "already holds the worktree gate for '${LASH_GATE_WORKTREE_SLUG}'" \
  "$proof_root/same-worktree-second.log"
grep -Eq 'PID [0-9]+' "$proof_root/same-worktree-second.log"
grep -Fq "$LASH_GATE_WORKTREE_LOCK_PATH" "$proof_root/same-worktree-second.log"
wait "$first_job"
first_job=""

# A completed gate must hand its locks to the next same-worktree gate without
# exposing the lock holder's crash-backstop polling interval to the caller.
set +e
run_smoke "$repo" "$proof_root/same-worktree-back-to-back.log" "$primary_slug" "$primary_slot"
back_to_back_status=$?
set -e
if [ "$back_to_back_status" -ne 0 ]; then
  echo "Back-to-back same-worktree run exited $back_to_back_status; expected success." >&2
  exit 1
fi

leftover_container="lash-gate-proof-leftover-${LASH_GATE_WORKTREE_SLUG}"
docker create --name "$leftover_container" --label "$LASH_GATE_LABEL" \
  postgres:16-alpine true >/dev/null
set +e
run_smoke "$repo" "$proof_root/leftover-refusal.log" "$primary_slug" "$primary_slot"
leftover_status=$?
set -e
if [ "$leftover_status" -ne 73 ]; then
  echo "Leftover-container run exited $leftover_status; expected clean refusal 73." >&2
  exit 1
fi
grep -q "owns leftover gate containers" "$proof_root/leftover-refusal.log"
grep -Fxq "  docker rm -fv $leftover_container" "$proof_root/leftover-refusal.log"
docker rm -f "$leftover_container" >/dev/null
leftover_container=""

compose_project="lash-gate-proof-compose-${LASH_GATE_WORKTREE_SLUG}"
compose_file="$repo/runbooks/restate-postgres-workers/docker-compose.yml"
compose_leftover_container="lash-gate-proof-compose-leftover-${LASH_GATE_WORKTREE_SLUG}"
docker create --name "$compose_leftover_container" --label "$LASH_GATE_LABEL" \
  --label "com.docker.compose.project=$compose_project" \
  --label "com.docker.compose.project.config_files=$compose_file" \
  postgres:16-alpine true >/dev/null
set +e
run_smoke "$repo" "$proof_root/compose-leftover-refusal.log" "$primary_slug" "$primary_slot"
compose_leftover_status=$?
set -e
if [ "$compose_leftover_status" -ne 73 ]; then
  echo "Compose-leftover run exited $compose_leftover_status; expected clean refusal 73." >&2
  exit 1
fi
grep -Fq "docker compose -p $compose_project -f $compose_file down -v --remove-orphans" \
  "$proof_root/compose-leftover-refusal.log"
docker rm -f "$compose_leftover_container" >/dev/null
compose_leftover_container=""

{
  printf 'parallel_elapsed_seconds=%s\n' "$parallel_elapsed"
  printf 'primary_identity=%s slot=%s base=%s\n' "$primary_slug" "$primary_slot" "$primary_base"
  printf 'peer_identity=%s slot=%s base=%s\n' "$peer_slug" "$peer_slot" "$peer_base"
  sed -n '/gate container smoke ready:/p' "$proof_root/worktree-a.log"
  sed -n '/gate container smoke ready:/p' "$proof_root/worktree-b.log"
  printf 'same_worktree_second_exit=%s\n' "$second_status"
  sed -n '/Refusing to start/p' "$proof_root/same-worktree-second.log"
  printf 'same_worktree_back_to_back_exit=%s\n' "$back_to_back_status"
  sed -n '/gate container smoke ready:/p' "$proof_root/same-worktree-back-to-back.log"
  printf 'leftover_refusal_exit=%s\n' "$leftover_status"
  sed -n '/Refusing to start:/p;/docker rm -fv/p' "$proof_root/leftover-refusal.log"
  printf 'compose_leftover_refusal_exit=%s\n' "$compose_leftover_status"
  sed -n '/Refusing to start:/p;/docker compose -p/p' "$proof_root/compose-leftover-refusal.log"
} | tee "$proof_root/summary.txt"
