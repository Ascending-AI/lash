#!/usr/bin/env bash
set -euo pipefail

mode="$1"
worktree_lock="$2"
slot_lock="$3"
battery="$4"
owner_pid="$5"
owner_start="$6"
worktree_slug="$7"
worktree_root="$8"
port_slot="$9"

write_metadata() {
  local lock_path="$1" scope="$2"
  umask 077
  printf 'battery=%s\npid=%s\nstarted_at=%s\nworktree_slug=%s\nworktree_root=%s\nscope=%s\nlock_path=%s\n' \
    "$battery" "$owner_pid" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    "$worktree_slug" "$worktree_root" "$scope" "$lock_path" \
    >"$lock_path"
}

case "$mode" in
  worktree)
    write_metadata "$worktree_lock" worktree
    printf 'worktree-acquired\n'
    exec flock -n -o "$slot_lock" "$0" slot \
      "$worktree_lock" "$slot_lock" "$battery" "$owner_pid" "$owner_start" \
      "$worktree_slug" "$worktree_root" "$port_slot"
    ;;
  slot)
    write_metadata "$slot_lock" port-slot
    printf 'slot-acquired\n'
    while [ -r "/proc/${owner_pid}/stat" ] \
      && [ "$(awk '{print $22}' "/proc/${owner_pid}/stat" 2>/dev/null || true)" = "$owner_start" ]; do
      sleep 0.1
    done
    ;;
  *)
    echo "Unknown lock-holder mode: $mode" >&2
    exit 2
    ;;
esac
