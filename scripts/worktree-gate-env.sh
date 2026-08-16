#!/usr/bin/env bash

# Shared identity and ownership contract for container-backed local gates.
# This file is sourced. Lock ownership lives in a dedicated flock -o holder so
# commands launched by the gate cannot inherit either lock descriptor.

lash_gate_slug_for_root() {
  local repo_root="$1" raw_slug base_slug path_checksum path_hash

  repo_root="$(cd "$repo_root" && pwd -P)"
  raw_slug="$(basename "$repo_root")"
  base_slug="$({
    printf '%s' "$raw_slug" \
      | tr '[:upper:]' '[:lower:]' \
      | sed -E 's/[^a-z0-9]+/-/g; s/^-+//; s/-+$//'
  })"
  if [ -z "$base_slug" ]; then
    echo "Cannot derive a gate slug from worktree basename '$raw_slug'." >&2
    return 2
  fi

  path_checksum="$(printf '%s' "$repo_root" | cksum)"
  path_checksum="${path_checksum%% *}"
  printf -v path_hash '%08x' "$path_checksum"
  printf '%s-%s\n' "$base_slug" "$path_hash"
}

lash_gate_configure() {
  local helper_dir repo_root checksum derived_slot slot

  helper_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
  repo_root="$(cd "$helper_dir/.." && pwd -P)"
  LASH_GATE_WORKTREE_ROOT="$repo_root"
  LASH_GATE_WORKTREE_SLUG="$(lash_gate_slug_for_root "$repo_root")"

  checksum="$(printf '%s' "$repo_root" | cksum)"
  checksum="${checksum%% *}"
  derived_slot=$((checksum % 90))
  slot="${LASH_GATE_SLOT_OVERRIDE:-$derived_slot}"
  if ! [[ "$slot" =~ ^[0-9]+$ ]] || ((10#$slot >= 90)); then
    echo "LASH_GATE_SLOT_OVERRIDE must be an integer in 0..89, got '${slot}'." >&2
    return 2
  fi
  slot=$((10#$slot))

  # Ninety disjoint 50-port blocks occupy 61000-65499, above Linux's default
  # ephemeral range. Callers use offsets 0-49; a documented slot override is
  # available when another live worktree happens to select the same block.
  LASH_GATE_PORT_SLOT="$slot"
  LASH_E2E_PORT_BASE="$((61000 + slot * 50))"
  LASH_E2E_NETWORK="lash-e2e-${LASH_GATE_WORKTREE_SLUG}"
  LASH_GATE_LABEL="com.lash.e2e.worktree=${LASH_GATE_WORKTREE_SLUG}"
  # Lock identity must not change between interactive, cron, and systemd-run
  # environments. /tmp is host-wide and the UID suffix keeps users disjoint.
  LASH_GATE_STATE_ROOT="/tmp/lash-gate-$(id -u)"
  LASH_GATE_STATE_DIR="${LASH_GATE_STATE_ROOT}/${LASH_GATE_WORKTREE_SLUG}"
  LASH_GATE_WORKTREE_LOCK_PATH="${LASH_GATE_STATE_DIR}/worktree.lock"
  LASH_GATE_PORT_LOCK_PATH="${LASH_GATE_STATE_ROOT}/port-slot-${LASH_GATE_PORT_SLOT}.lock"

  export LASH_GATE_WORKTREE_ROOT LASH_GATE_WORKTREE_SLUG
  export LASH_GATE_PORT_SLOT LASH_E2E_PORT_BASE
  export LASH_E2E_NETWORK LASH_GATE_LABEL LASH_GATE_STATE_ROOT LASH_GATE_STATE_DIR
  export LASH_GATE_WORKTREE_LOCK_PATH LASH_GATE_PORT_LOCK_PATH
}

lash_gate_owned_container_names() {
  local container_id
  while IFS= read -r container_id; do
    [ -n "$container_id" ] || continue
    docker inspect --format '{{.Name}}' "$container_id"
  done < <(docker ps -aq --filter "label=${LASH_GATE_LABEL}") \
    | sed 's#^/##' \
    | sort
}

lash_gate_refuse_leftovers() {
  local containers container project config_file
  local -a direct_containers=() config_files=()
  local -A compose_configs=()

  containers="$(lash_gate_owned_container_names)"
  if [ -z "$containers" ]; then
    return
  fi

  echo "Refusing to start: worktree '${LASH_GATE_WORKTREE_SLUG}' owns leftover gate containers:" >&2
  while IFS= read -r container; do
    [ -n "$container" ] || continue
    printf '  %s\n' "$container" >&2
    project="$(docker inspect -f '{{index .Config.Labels "com.docker.compose.project"}}' "$container" 2>/dev/null || true)"
    if [ -n "$project" ] && [ "$project" != "<no value>" ]; then
      config_file="$(docker inspect -f '{{index .Config.Labels "com.docker.compose.project.config_files"}}' "$container" 2>/dev/null || true)"
      compose_configs["$project"]="$config_file"
    else
      direct_containers+=("$container")
    fi
  done <<<"$containers"

  echo "Remove only that owned state, including Compose volumes, then retry:" >&2
  while IFS= read -r project; do
    [ -n "$project" ] || continue
    printf '  docker compose -p %q' "$project" >&2
    IFS=',' read -r -a config_files <<<"${compose_configs[$project]}"
    for config_file in "${config_files[@]}"; do
      [ -n "$config_file" ] && [ "$config_file" != "<no value>" ] \
        && printf ' -f %q' "$config_file" >&2
    done
    printf ' down -v --remove-orphans\n' >&2
  done < <(printf '%s\n' "${!compose_configs[@]}" | sort)
  if ((${#direct_containers[@]})); then
    printf '  docker rm -fv' >&2
    for container in "${direct_containers[@]}"; do
      printf ' %q' "$container" >&2
    done
    printf '\n' >&2
  fi
  return 73
}

lash_gate_lock_value() {
  local key="$1" path="$2"
  sed -n "s/^${key}=//p" "$path" 2>/dev/null | head -n 1
}

lash_gate_lock_owner_is_live() {
  local lock_path="$1" owner_pid owner_start current_start

  owner_pid="$(lash_gate_lock_value pid "$lock_path")"
  owner_start="$(lash_gate_lock_value pid_start "$lock_path")"
  [[ "$owner_pid" =~ ^[0-9]+$ && "$owner_start" =~ ^[0-9]+$ ]] || return 1
  [ -r "/proc/${owner_pid}/stat" ] || return 1
  current_start="$(awk '{print $22}' "/proc/${owner_pid}/stat" 2>/dev/null || true)"
  [ "$current_start" = "$owner_start" ]
}

lash_gate_refuse_lock() {
  local battery="$1" lock_path="$2" scope="$3"
  local owner_battery owner_pid owner_root

  owner_battery="$(lash_gate_lock_value battery "$lock_path")"
  owner_pid="$(lash_gate_lock_value pid "$lock_path")"
  owner_root="$(lash_gate_lock_value worktree_root "$lock_path")"
  owner_battery="${owner_battery:-another-gate}"
  owner_pid="${owner_pid:-unknown}"
  owner_root="${owner_root:-another worktree}"

  if [ "$scope" = "worktree" ]; then
    echo "Refusing to start ${battery}: ${owner_battery} (PID ${owner_pid}) already holds the worktree gate for '${LASH_GATE_WORKTREE_SLUG}'." >&2
  else
    echo "Refusing to start ${battery}: ${owner_battery} (PID ${owner_pid}) from '${owner_root}' holds port slot ${LASH_GATE_PORT_SLOT}." >&2
  fi
  echo "Lock: ${lock_path}" >&2
  if [ "$owner_pid" = "unknown" ] || ! lash_gate_lock_owner_is_live "$lock_path"; then
    echo "Inspect the holder, stop it if orphaned, then retry:" >&2
    printf '  fuser -v %q\n' "$lock_path" >&2
  else
    echo "Wait for that PID to exit, or stop it if it is orphaned, then retry:" >&2
    printf '  kill %q\n' "$owner_pid" >&2
  fi
  if [ "$scope" = "slot" ]; then
    echo "To use a different deterministic block instead, retry with:" >&2
    printf '  LASH_GATE_SLOT_OVERRIDE=<0..89> <gate-command>\n' >&2
  fi
  return 73
}

lash_gate_acquire_locks() {
  local battery="${1:-gate}" owner_start marker status_fd holder_pid
  local attempt failed_lock failed_scope
  local holder_script="${LASH_GATE_WORKTREE_ROOT}/scripts/worktree-gate-lock-holder.sh"

  LASH_GATE_ACQUIRED_HERE=0
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
  owner_start="$(awk '{print $22}' "/proc/$$/stat")"
  for attempt in 1 2; do
    marker=""
    failed_lock="$LASH_GATE_WORKTREE_LOCK_PATH"
    failed_scope=worktree
    coproc LASH_GATE_LOCK_HOLDER_PROCESS {
      flock -w 2 -o "$LASH_GATE_WORKTREE_LOCK_PATH" \
        "$holder_script" worktree \
        "$LASH_GATE_WORKTREE_LOCK_PATH" "$LASH_GATE_PORT_LOCK_PATH" \
        "$battery" "$$" "$owner_start" "$LASH_GATE_WORKTREE_SLUG" \
        "$LASH_GATE_WORKTREE_ROOT" "$LASH_GATE_PORT_SLOT"
    }
    status_fd="${LASH_GATE_LOCK_HOLDER_PROCESS[0]}"
    holder_pid="$LASH_GATE_LOCK_HOLDER_PROCESS_PID"

    if IFS= read -r marker <&"$status_fd" && [ "$marker" = "worktree-acquired" ]; then
      failed_lock="$LASH_GATE_PORT_LOCK_PATH"
      failed_scope=slot
      if IFS= read -r marker <&"$status_fd" && [ "$marker" = "slot-acquired" ]; then
        eval "exec ${status_fd}<&-"
        break
      fi
      kill "$holder_pid" >/dev/null 2>&1 || true
    fi
    wait "$holder_pid" 2>/dev/null || true

    # Metadata for a dead/reused PID describes a release in progress, not an
    # actionable owner. Retry once so the caller takes over after the holder's
    # crash-backstop poll instead of being told to kill a dead process.
    if [ "$attempt" -eq 1 ] && ! lash_gate_lock_owner_is_live "$failed_lock"; then
      continue
    fi
    lash_gate_refuse_lock "$battery" "$failed_lock" "$failed_scope"
    return
  done

  LASH_GATE_LOCK_HOLDER_PID="$holder_pid"
  LASH_GATE_ACQUIRED_HERE=1
  export LASH_GATE_LOCK_HELD=1
  export LASH_GATE_LOCK_SLUG="$LASH_GATE_WORKTREE_SLUG"
}

lash_gate_cleanup() {
  if ! command -v docker >/dev/null 2>&1; then
    return 0
  fi
  local attached
  if docker network inspect "$LASH_E2E_NETWORK" >/dev/null 2>&1; then
    attached="$(docker network inspect -f '{{len .Containers}}' "$LASH_E2E_NETWORK" 2>/dev/null || true)"
    if [ "$attached" = "0" ]; then
      docker network rm "$LASH_E2E_NETWORK" >/dev/null 2>&1 || true
    fi
  fi
}

lash_gate_prune_orphaned_networks() {
  local net_name attached net_root net_slug wt_line wt_path wt_slug
  local -A live_slugs=()

  if [ -d "$LASH_GATE_WORKTREE_ROOT" ]; then
    live_slugs["$LASH_GATE_WORKTREE_SLUG"]="$LASH_GATE_WORKTREE_ROOT"
  fi

  while IFS= read -r wt_line; do
    if [[ "$wt_line" =~ ^worktree[[:space:]]+(.*)$ ]]; then
      wt_path="${BASH_REMATCH[1]}"
      if [ -d "$wt_path" ]; then
        wt_slug="$(lash_gate_slug_for_root "$wt_path" 2>/dev/null || true)"
        if [ -n "$wt_slug" ]; then
          live_slugs["$wt_slug"]="$wt_path"
        fi
      fi
    fi
  done < <(git -C "$LASH_GATE_WORKTREE_ROOT" worktree list --porcelain 2>/dev/null || true)

  while IFS= read -r net_name; do
    [ -n "$net_name" ] || continue
    [[ "$net_name" == lash-e2e-* ]] || continue

    attached="$(docker network inspect -f '{{len .Containers}}' "$net_name" 2>/dev/null || true)"
    [ "$attached" = "0" ] || continue

    net_root="$(docker network inspect -f '{{index .Labels "com.lash.e2e.worktree.root"}}' "$net_name" 2>/dev/null || true)"
    if [ -n "$net_root" ] && [ "$net_root" != "<no value>" ]; then
      if [ ! -d "$net_root" ]; then
        docker network rm "$net_name" >/dev/null 2>&1 || true
      fi
      continue
    fi

    net_slug="$(docker network inspect -f '{{index .Labels "com.lash.e2e.worktree"}}' "$net_name" 2>/dev/null || true)"
    if [ -z "$net_slug" ] || [ "$net_slug" = "<no value>" ]; then
      net_slug="${net_name#lash-e2e-}"
    fi

    if [ -n "$net_slug" ] && [ -z "${live_slugs[$net_slug]:-}" ]; then
      docker network rm "$net_name" >/dev/null 2>&1 || true
    fi
  done < <(docker network ls --format '{{.Name}}' 2>/dev/null || true)
}

lash_gate_prepare_docker() {
  if [ "${LASH_GATE_ACQUIRED_HERE:-0}" != "1" ]; then
    return
  fi
  if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
    lash_gate_refuse_leftovers || return
    lash_gate_prune_orphaned_networks
    docker network inspect "$LASH_E2E_NETWORK" >/dev/null 2>&1 \
      || docker network create \
        --label "$LASH_GATE_LABEL" \
        --label "com.lash.e2e.worktree.root=${LASH_GATE_WORKTREE_ROOT}" \
        "$LASH_E2E_NETWORK" >/dev/null
  fi
}

lash_gate_acquire() {
  lash_gate_acquire_locks "${1:-gate}" || return
  lash_gate_prepare_docker
}

lash_gate_configure
