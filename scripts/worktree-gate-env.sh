#!/usr/bin/env bash

# Shared identity and ownership contract for container-backed local gates.
# This file is sourced; callers keep the lock file descriptors for their
# lifetime and own cleanup of the containers they start.

lash_gate_configure() {
  local repo_root raw_slug checksum slot

  repo_root="$(git rev-parse --show-toplevel)"
  raw_slug="$(basename "$repo_root")"
  LASH_GATE_WORKTREE_SLUG="$({
    printf '%s' "$raw_slug" \
      | tr '[:upper:]' '[:lower:]' \
      | sed -E 's/[^a-z0-9]+/-/g; s/^-+//; s/-+$//'
  })"
  if [ -z "$LASH_GATE_WORKTREE_SLUG" ]; then
    echo "Cannot derive a gate slug from worktree basename '$raw_slug'." >&2
    return 2
  fi

  checksum="$(printf '%s' "$LASH_GATE_WORKTREE_SLUG" | cksum)"
  checksum="${checksum%% *}"
  slot=$((checksum % 64))

  # 64 disjoint 64-port blocks occupy 61000-65095, above Linux's default
  # ephemeral range. Callers use offsets
  # 0-63, and explicit lane-specific port overrides remain escape hatches.
  LASH_GATE_PORT_SLOT="$slot"
  LASH_E2E_PORT_BASE="$((61000 + slot * 64))"
  LASH_E2E_NETWORK="lash-e2e-${LASH_GATE_WORKTREE_SLUG}"
  LASH_GATE_LABEL="com.lash.e2e.worktree=${LASH_GATE_WORKTREE_SLUG}"
  LASH_GATE_STATE_DIR="/tmp/lash-gate-${LASH_GATE_WORKTREE_SLUG}"

  export LASH_GATE_WORKTREE_SLUG LASH_GATE_PORT_SLOT LASH_E2E_PORT_BASE
  export LASH_E2E_NETWORK LASH_GATE_LABEL LASH_GATE_STATE_DIR
}

lash_gate_owned_container_names() {
  docker ps -aq --filter "label=${LASH_GATE_LABEL}" \
    | xargs -r docker inspect --format '{{.Name}}' \
    | sed 's#^/##' \
    | sort
}

lash_gate_refuse_leftovers() {
  local containers
  containers="$(lash_gate_owned_container_names)"
  if [ -z "$containers" ]; then
    return
  fi

  echo "Refusing to start: worktree '${LASH_GATE_WORKTREE_SLUG}' owns leftover gate containers:" >&2
  while IFS= read -r container; do
    printf '  %s\n' "$container" >&2
  done <<<"$containers"
  echo "Remove only those containers, then retry:" >&2
  printf '  docker rm -f %s\n' "$(printf '%s\n' "$containers" | paste -sd' ' -)" >&2
  return 73
}

lash_gate_acquire() {
  local battery="${1:-gate}" lock_owner

  if [ "${LASH_GATE_LOCK_HELD:-0}" = "1" ]; then
    if [ "${LASH_GATE_LOCK_SLUG:-}" != "$LASH_GATE_WORKTREE_SLUG" ]; then
      echo "Inherited gate lock belongs to '${LASH_GATE_LOCK_SLUG:-unknown}', not '${LASH_GATE_WORKTREE_SLUG}'." >&2
      return 73
    fi
    return
  fi

  if ! command -v flock >/dev/null 2>&1; then
    echo "flock is required for concurrency-safe local gates." >&2
    return 127
  fi

  mkdir -p "$LASH_GATE_STATE_DIR"
  exec {LASH_GATE_WORKTREE_LOCK_FD}>"${LASH_GATE_STATE_DIR}/worktree.lock"
  if ! flock -n "$LASH_GATE_WORKTREE_LOCK_FD"; then
    lock_owner="$(cat "${LASH_GATE_STATE_DIR}/owner" 2>/dev/null || echo another-gate)"
    echo "Refusing to start ${battery}: ${lock_owner} is already running for worktree '${LASH_GATE_WORKTREE_SLUG}'." >&2
    return 73
  fi

  # A stable hash has a bounded collision domain. Serialize the selected port
  # slot too, turning an unlikely collision into a named refusal, never a port
  # bind race between different worktrees.
  exec {LASH_GATE_PORT_LOCK_FD}>"/tmp/lash-gate-port-slot-${LASH_GATE_PORT_SLOT}.lock"
  if ! flock -n "$LASH_GATE_PORT_LOCK_FD"; then
    echo "Refusing to start ${battery}: deterministic port slot ${LASH_GATE_PORT_SLOT} is in use by another worktree." >&2
    return 73
  fi

  printf '%s\n' "$battery" >"${LASH_GATE_STATE_DIR}/owner"
  export LASH_GATE_LOCK_HELD=1
  export LASH_GATE_LOCK_SLUG="$LASH_GATE_WORKTREE_SLUG"
  export LASH_GATE_WORKTREE_LOCK_FD LASH_GATE_PORT_LOCK_FD

  if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
    lash_gate_refuse_leftovers || return
    docker network inspect "$LASH_E2E_NETWORK" >/dev/null 2>&1 \
      || docker network create \
        --label "$LASH_GATE_LABEL" \
        "$LASH_E2E_NETWORK" >/dev/null
  fi
}

lash_gate_configure
