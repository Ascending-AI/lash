#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
helper="$repo/scripts/worktree-gate-env.sh"
attachment_usage_gate="$repo/scripts/agent-workbench-attachment-usage-gate.sh"
test_tmp="$(mktemp -d "${TMPDIR:-/tmp}/lash-gate-env-test.XXXXXX")"
holder_pid=""
leaked_child_pid=""

cleanup() {
  [ -z "$holder_pid" ] || kill "$holder_pid" >/dev/null 2>&1 || true
  [ -z "$leaked_child_pid" ] || kill "$leaked_child_pid" >/dev/null 2>&1 || true
  [ ! -d "$test_tmp" ] || rm -rf "$test_tmp"
  lash_gate_cleanup
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

expected_state_root="/tmp/lash-gate-$(id -u)"
[ "$LASH_GATE_STATE_ROOT" = "$expected_state_root" ] \
  || fail "lock root is not stable: $LASH_GATE_STATE_ROOT"
for ambient_root in "$test_tmp/xdg" "$test_tmp/tmpdir"; do
  mkdir -p "$ambient_root"
done
alternate_state_root="$({
  XDG_RUNTIME_DIR="$test_tmp/xdg" TMPDIR="$test_tmp/tmpdir" \
    bash -c 'source "$1"; printf "%s" "$LASH_GATE_STATE_ROOT"' _ "$helper"
})"
[ "$alternate_state_root" = "$expected_state_root" ] \
  || fail "ambient temp directories changed the lock root to $alternate_state_root"

for workbench_port in 3030 3032 3042 65535; do
  postgres_port="$({
    AGENT_WORKBENCH_USAGE_GATE_PORT_PROBE=1 \
      bash "$attachment_usage_gate" "$workbench_port"
  })"
  postgres_offset=$((postgres_port - LASH_E2E_PORT_BASE))
  ((postgres_offset >= 0 && postgres_offset <= 9)) \
    || fail "workbench port $workbench_port derived reserved/out-of-block Postgres offset $postgres_offset"
done

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

peer_root="$test_tmp/peer-checkout"
mkdir -p "$peer_root/scripts"
cp "$helper" "$repo/scripts/worktree-gate-lock-holder.sh" "$peer_root/scripts/"
slot_refusal_log="$test_tmp/slot-refusal.log"
set +e
LASH_GATE_SLOT_OVERRIDE="$override_slot" bash -c '
  set -euo pipefail
  source "$1/scripts/worktree-gate-env.sh"
  lash_gate_acquire_locks regression-cross-worktree-contender
' _ "$peer_root" >"$slot_refusal_log" 2>&1
slot_refusal_status=$?
set -e
[ "$slot_refusal_status" -eq 73 ] \
  || fail "cross-worktree slot contender exited $slot_refusal_status, expected 73"
grep -Fq "holds port slot $override_slot" "$slot_refusal_log" \
  || fail "slot refusal omitted occupied slot"
grep -Fq "from '$repo'" "$slot_refusal_log" \
  || fail "slot refusal omitted owner worktree"
grep -Fq 'LASH_GATE_SLOT_OVERRIDE=<0..89> <gate-command>' "$slot_refusal_log" \
  || fail "slot refusal omitted override remedy"

kill "$holder_pid"
wait "$holder_pid" 2>/dev/null || true
holder_pid=""

# Reacquisition must absorb the holder's /proc polling tail. The owner PID is
# already dead here, so a refusal would also print an unusable kill remedy.
LASH_GATE_SLOT_OVERRIDE="$override_slot" bash -c '
  set -euo pipefail
  source "$1"
  lash_gate_acquire_locks regression-after-dead-owner
' _ "$helper" >"$test_tmp/dead-owner-reacquire.log" 2>&1 \
  || fail "dead owner did not trigger takeover: $(tr '\n' ' ' <"$test_tmp/dead-owner-reacquire.log")"
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

# Docker network lifecycle stub testing:
stub_bin="$test_tmp/bin"
stub_docker="$stub_bin/docker"
docker_state="$test_tmp/docker-state"
mkdir -p "$stub_bin" "$docker_state/networks"

cat >"$stub_docker" <<'DOCKER_STUB'
#!/usr/bin/env bash
set -euo pipefail

state_dir="${DOCKER_STUB_STATE:-/tmp/docker-stub-state}"
mkdir -p "$state_dir/networks"
log_file="$state_dir/calls.log"

printf '%s\n' "$*" >>"$log_file"

cmd="${1:-}"
case "$cmd" in
  info)
    exit 0
    ;;
  ps)
    exit 0
    ;;
  inspect)
    exit 0
    ;;
  network)
    subcmd="${2:-}"
    case "$subcmd" in
      ls)
        if [ -d "$state_dir/networks" ]; then
          for net_dir in "$state_dir/networks"/*; do
            [ -d "$net_dir" ] || continue
            basename "$net_dir"
          done
        fi
        exit 0
        ;;
      inspect)
        format=""
        shift 2
        while [[ $# -gt 1 ]]; do
          case "$1" in
            -f|--format) format="$2"; shift 2 ;;
            *) shift ;;
          esac
        done
        net_name="$1"
        net_dir="$state_dir/networks/$net_name"
        if [ ! -d "$net_dir" ]; then
          exit 1
        fi
        if [ -z "$format" ]; then
          exit 0
        fi
        if [[ "$format" == *"len .Containers"* ]]; then
          if [ -f "$net_dir/attached_containers" ]; then
            cat "$net_dir/attached_containers"
          else
            echo "0"
          fi
          exit 0
        elif [[ "$format" == *'index .Labels "com.lash.e2e.worktree.root"'* ]]; then
          if [ -f "$net_dir/label.worktree.root" ]; then
            cat "$net_dir/label.worktree.root"
          else
            echo "<no value>"
          fi
          exit 0
        elif [[ "$format" == *'index .Labels "com.lash.e2e.worktree"'* ]]; then
          if [ -f "$net_dir/label.worktree" ]; then
            cat "$net_dir/label.worktree"
          else
            echo "<no value>"
          fi
          exit 0
        fi
        exit 0
        ;;
      create)
        shift 2
        labels=()
        while [[ $# -gt 1 ]]; do
          case "$1" in
            --label) labels+=("$2"); shift 2 ;;
            *) shift ;;
          esac
        done
        net_name="$1"
        net_dir="$state_dir/networks/$net_name"
        mkdir -p "$net_dir"
        echo "0" >"$net_dir/attached_containers"
        for label in "${labels[@]}"; do
          case "$label" in
            com.lash.e2e.worktree.root=*)
              printf '%s\n' "${label#com.lash.e2e.worktree.root=}" >"$net_dir/label.worktree.root"
              ;;
            com.lash.e2e.worktree=*)
              printf '%s\n' "${label#com.lash.e2e.worktree=}" >"$net_dir/label.worktree"
              ;;
          esac
        done
        exit 0
        ;;
      rm)
        net_name="${3:-}"
        net_dir="$state_dir/networks/$net_name"
        if [ -d "$net_dir" ]; then
          rm -rf "$net_dir"
        fi
        exit 0
        ;;
      *)
        echo "stub docker: unhandled network subcmd $subcmd" >&2
        exit 1
        ;;
    esac
    ;;
  *)
    echo "stub docker: unhandled cmd $cmd" >&2
    exit 1
    ;;
esac
DOCKER_STUB
chmod +x "$stub_docker"

create_mock_network() {
  local net_name="$1" attached="${2:-0}" root="${3:-}" slug="${4:-}"
  local net_dir="$docker_state/networks/$net_name"
  mkdir -p "$net_dir"
  printf '%s\n' "$attached" >"$net_dir/attached_containers"
  if [ -n "$root" ]; then
    printf '%s\n' "$root" >"$net_dir/label.worktree.root"
  fi
  if [ -n "$slug" ]; then
    printf '%s\n' "$slug" >"$net_dir/label.worktree"
  fi
}

# (a) Cleanup removes the created network when empty:
rm -rf "${docker_state:?}"/*
mkdir -p "$docker_state/networks"

fresh_worktree="$test_tmp/fresh-checkout"
mkdir -p "$fresh_worktree/scripts"
cp "$helper" "$repo/scripts/worktree-gate-lock-holder.sh" "$fresh_worktree/scripts/"
fresh_slug="$(lash_gate_slug_for_root "$fresh_worktree")"
fresh_network="lash-e2e-${fresh_slug}"

PATH="$stub_bin:$PATH" DOCKER_STUB_STATE="$docker_state" bash -c '
  set -euo pipefail
  source "$1/scripts/worktree-gate-env.sh"
  lash_gate_acquire test-lifecycle-fresh
  [ -d "'"$docker_state"'/networks/'"$fresh_network"'" ] || exit 11
  lash_gate_cleanup
' _ "$fresh_worktree"

[ ! -d "$docker_state/networks/$fresh_network" ] \
  || fail "cleanup did not remove empty network $fresh_network"
grep -Fq "network rm $fresh_network" "$docker_state/calls.log" \
  || fail "cleanup did not invoke docker network rm $fresh_network"

# (b) Cleanup leaves a network with attached containers:
rm -rf "${docker_state:?}"/*
mkdir -p "$docker_state/networks"

busy_worktree="$test_tmp/busy-checkout"
mkdir -p "$busy_worktree/scripts"
cp "$helper" "$repo/scripts/worktree-gate-lock-holder.sh" "$busy_worktree/scripts/"
busy_slug="$(lash_gate_slug_for_root "$busy_worktree")"
busy_network="lash-e2e-${busy_slug}"

PATH="$stub_bin:$PATH" DOCKER_STUB_STATE="$docker_state" bash -c '
  set -euo pipefail
  source "$1/scripts/worktree-gate-env.sh"
  lash_gate_acquire test-lifecycle-busy
  printf "1\n" >"'"$docker_state"'/networks/'"$busy_network"'/attached_containers"
  lash_gate_cleanup
' _ "$busy_worktree"

[ -d "$docker_state/networks/$busy_network" ] \
  || fail "cleanup removed busy network $busy_network with attached containers"
! grep -Fq "network rm $busy_network" "$docker_state/calls.log" \
  || fail "cleanup invoked docker network rm on busy network $busy_network"

# (c) Prune removes only orphaned lash-e2e-* networks of dead worktrees:
rm -rf "${docker_state:?}"/*
mkdir -p "$docker_state/networks"

test_git_root="$test_tmp/test-git-repo"
git init -q "$test_git_root"
(
  cd "$test_git_root"
  git config user.email "test@example.com"
  git config user.name "Test User"
  git commit --allow-empty -m "initial" -q
)
mkdir -p "$test_git_root/scripts"
cp "$helper" "$repo/scripts/worktree-gate-lock-holder.sh" "$test_git_root/scripts/"

live_wt_dir="$test_tmp/test-git-peer"
git -C "$test_git_root" worktree add -q "$live_wt_dir"
mkdir -p "$live_wt_dir/scripts"
cp "$helper" "$repo/scripts/worktree-gate-lock-holder.sh" "$live_wt_dir/scripts/"
live_slug="$(lash_gate_slug_for_root "$live_wt_dir")"

dead_wt_dir="$test_tmp/test-git-dead"
git -C "$test_git_root" worktree add -q "$dead_wt_dir"
dead_slug="$(lash_gate_slug_for_root "$dead_wt_dir")"
rm -rf "$dead_wt_dir"

create_mock_network "lash-e2e-${dead_slug}" 0 "$dead_wt_dir" "$dead_slug"
create_mock_network "lash-e2e-dead-historical-99999999" 0 "" "dead-historical-99999999"
create_mock_network "lash-e2e-${live_slug}" 0 "$live_wt_dir" "$live_slug"
create_mock_network "lash-e2e-live-hist-${live_slug}" 0 "" "$live_slug"
create_mock_network "lash-e2e-busy-dead-88888888" 2 "$dead_wt_dir" "busy-dead-88888888"
create_mock_network "custom-non-lash-network" 0 "" ""

PATH="$stub_bin:$PATH" DOCKER_STUB_STATE="$docker_state" bash -c '
  set -euo pipefail
  source "$1/scripts/worktree-gate-env.sh"
  lash_gate_acquire test-prune-runner
' _ "$test_git_root"

[ ! -d "$docker_state/networks/lash-e2e-${dead_slug}" ] \
  || fail "prune failed to remove dead labeled network lash-e2e-${dead_slug}"
[ ! -d "$docker_state/networks/lash-e2e-dead-historical-99999999" ] \
  || fail "prune failed to remove dead historical network lash-e2e-dead-historical-99999999"
[ -d "$docker_state/networks/lash-e2e-${live_slug}" ] \
  || fail "prune improperly removed live network lash-e2e-${live_slug}"
[ -d "$docker_state/networks/lash-e2e-live-hist-${live_slug}" ] \
  || fail "prune improperly removed live historical network lash-e2e-live-hist-${live_slug}"
[ -d "$docker_state/networks/lash-e2e-busy-dead-88888888" ] \
  || fail "prune improperly removed dead network with attached containers"
[ -d "$docker_state/networks/custom-non-lash-network" ] \
  || fail "prune improperly removed non-lash network"

# (d) Parallel worktrees get isolated networks and teardown remains isolated:
rm -rf "${docker_state:?}"/*
mkdir -p "$docker_state/networks"

par_wt_1="$test_tmp/parallel-wt-1"
par_wt_2="$test_tmp/parallel-wt-2"
mkdir -p "$par_wt_1/scripts" "$par_wt_2/scripts"
cp "$helper" "$repo/scripts/worktree-gate-lock-holder.sh" "$par_wt_1/scripts/"
cp "$helper" "$repo/scripts/worktree-gate-lock-holder.sh" "$par_wt_2/scripts/"
par_slug_1="$(lash_gate_slug_for_root "$par_wt_1")"
par_slug_2="$(lash_gate_slug_for_root "$par_wt_2")"
par_net_1="lash-e2e-${par_slug_1}"
par_net_2="lash-e2e-${par_slug_2}"

[ "$par_net_1" != "$par_net_2" ] || fail "parallel worktrees derived identical networks"

PATH="$stub_bin:$PATH" DOCKER_STUB_STATE="$docker_state" bash -c '
  set -euo pipefail
  source "$1/scripts/worktree-gate-env.sh"
  lash_gate_acquire parallel-1
' _ "$par_wt_1"

PATH="$stub_bin:$PATH" DOCKER_STUB_STATE="$docker_state" bash -c '
  set -euo pipefail
  source "$1/scripts/worktree-gate-env.sh"
  lash_gate_acquire parallel-2
' _ "$par_wt_2"

[ -d "$docker_state/networks/$par_net_1" ] || fail "parallel worktree 1 network missing"
[ -d "$docker_state/networks/$par_net_2" ] || fail "parallel worktree 2 network missing"

# Teardown worktree 1: only network 1 is cleaned up
PATH="$stub_bin:$PATH" DOCKER_STUB_STATE="$docker_state" bash -c '
  set -euo pipefail
  source "$1/scripts/worktree-gate-env.sh"
  lash_gate_cleanup
' _ "$par_wt_1"

[ ! -d "$docker_state/networks/$par_net_1" ] || fail "worktree 1 cleanup left network 1"
[ -d "$docker_state/networks/$par_net_2" ] || fail "worktree 1 cleanup removed worktree 2 network"

# Teardown worktree 2: network 2 is cleaned up, leaving zero networks
PATH="$stub_bin:$PATH" DOCKER_STUB_STATE="$docker_state" bash -c '
  set -euo pipefail
  source "$1/scripts/worktree-gate-env.sh"
  lash_gate_cleanup
' _ "$par_wt_2"

[ ! -d "$docker_state/networks/$par_net_2" ] || fail "worktree 2 cleanup left network 2"
remaining_networks="$(find "$docker_state/networks" -mindepth 1 -maxdepth 1 | wc -l)"
[ "$remaining_networks" -eq 0 ] || fail "gated runs left $remaining_networks leftover networks"

printf 'worktree gate env regressions passed: distinct_slugs=%s,%s override_slot=%s leaked_child_lock=released\n' \
  "$slug_a" "$slug_b" "$override_slot"
