#!/usr/bin/env bash
set -euo pipefail

if (($# != 0)); then
  echo "usage: $0" >&2
  exit 2
fi

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"
lash_gate_acquire "slack-clone-full-host-e2e"

port="${LASH_SLACK_CLONE_E2E_PORT:-$((LASH_E2E_PORT_BASE + 48))}"
if ! [[ "$port" =~ ^[0-9]+$ ]] || ((10#$port < 1 || 10#$port > 65534)); then
  echo "slack-clone E2E platform port must be an integer in 1..65534, got '$port'." >&2
  exit 2
fi
if ((10#$port == 3056 || 10#$port == 3057 || 10#$port + 1 == 3056 || 10#$port + 1 == 3057)); then
  echo "slack-clone E2E refuses reserved ports 3056/3057." >&2
  exit 2
fi

artifact_dir="${LASH_SLACK_CLONE_E2E_ARTIFACT_DIR:-}"
if [[ -z "$artifact_dir" ]]; then
  artifact_dir="$(mktemp -d "${TMPDIR:-/tmp}/lash-slack-clone-e2e-${LASH_GATE_WORKTREE_SLUG}.XXXXXX")"
fi
mkdir -p "$artifact_dir"
artifact_dir="$(cd "$artifact_dir" && pwd -P)"
state_dir="$(mktemp -d "$artifact_dir/state.XXXXXX")"
provider_dir="$state_dir/provider"
mkdir -p "$state_dir" "$provider_dir"

export SLACK_CLONE_STATE_DIR="$state_dir"
export SLACK_CLONE_OPEN=0
export SLACK_CLONE_E2E_PROVIDER=scripted-v1
export SLACK_CLONE_E2E_PROVIDER_DIR="$provider_dir"

cleanup() {
  status=$?
  trap - EXIT INT TERM
  teardown_status=0
  if ! bash "$repo/scripts/slack-clone-dev.sh" down --port "$port" \
    >>"$artifact_dir/teardown.log" 2>&1; then
    teardown_status=1
  fi
  for candidate_port in "$port" "$((10#$port + 1))"; do
    if timeout 1 bash -c "</dev/tcp/127.0.0.1/$candidate_port" 2>/dev/null; then
      echo "teardown left port $candidate_port reachable" >>"$artifact_dir/teardown.log"
      teardown_status=1
    fi
  done
  if [[ "$status" -eq 0 && "$teardown_status" -ne 0 ]]; then
    status=1
  fi
  if [[ "$status" -ne 0 ]]; then
    echo "slack-clone full-host E2E failed with status $status; artifacts: $artifact_dir" >&2
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

start_epoch="$(date +%s)"
printf '{"platform_port":%s,"bot_port":%s}\n' \
  "$port" "$((10#$port + 1))" \
  >"$artifact_dir/00-run.json"

bash "$repo/scripts/slack-clone-dev.sh" up --port "$port" \
  2>&1 | tee "$artifact_dir/00-boot.log"

uv run "$repo/scripts/slack-clone-full-host-e2e.py" \
  --repo "$repo" \
  --port "$port" \
  --state-dir "$state_dir" \
  --artifact-dir "$artifact_dir" \
  2>&1 | tee "$artifact_dir/e2e.log"

elapsed="$(( $(date +%s) - start_epoch ))"
printf '%s\n' "$elapsed" >"$artifact_dir/runtime-seconds.txt"
echo "slack-clone full-host E2E passed: runtime=${elapsed}s artifacts=$artifact_dir"
