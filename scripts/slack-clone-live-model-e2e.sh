#!/usr/bin/env bash
set -euo pipefail

if (($# > 1)) || [[ $# -eq 1 && "$1" != "--smoke-only" ]]; then
  echo "usage: $0 [--smoke-only]" >&2
  exit 2
fi

if [[ -z "${OPENROUTER_API_KEY:-}" ]]; then
  echo "SKIP: OPENROUTER_API_KEY is unset; live Slack-clone E2E was not run"
  exit 0
fi

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"
lash_gate_acquire "slack-clone-live-model-e2e"

port="${LASH_SLACK_CLONE_LIVE_E2E_PORT:-$((LASH_E2E_PORT_BASE + 48))}"
if ! [[ "$port" =~ ^[0-9]+$ ]] || ((10#$port < 1 || 10#$port > 65535)); then
  echo "slack-clone live E2E platform port must be an integer in 1..65535, got '$port'." >&2
  exit 2
fi
if ((10#$port == 3056 || 10#$port == 3057)); then
  echo "slack-clone live E2E refuses reserved ports 3056/3057." >&2
  exit 2
fi
if timeout 1 bash -c "</dev/tcp/127.0.0.1/$port" 2>/dev/null; then
  echo "slack-clone live E2E refuses occupied port $port." >&2
  exit 73
fi

artifact_dir="${LASH_SLACK_CLONE_LIVE_E2E_ARTIFACT_DIR:-}"
if [[ -z "$artifact_dir" ]]; then
  artifact_dir="$(mktemp -d "${TMPDIR:-/tmp}/lash-slack-clone-live-${LASH_GATE_WORKTREE_SLUG}.XXXXXX")"
fi
mkdir -p "$artifact_dir"
artifact_dir="$(cd "$artifact_dir" && pwd -P)"
state_dir="$(mktemp -d "$artifact_dir/state.XXXXXX")"
platform_log="$artifact_dir/platform.log"
platform_pid=""
base_url="http://127.0.0.1:$port"

cleanup() {
  status=$?
  trap - EXIT INT TERM
  if [[ "$status" -ne 0 ]] && [[ -n "$platform_pid" ]] && kill -0 "$platform_pid" 2>/dev/null; then
    uv run "$repo/scripts/slack-clone-live-model-ui.py" \
      --base-url "$base_url" --artifact-dir "$artifact_dir" --capture-only \
      >>"$artifact_dir/failure-capture.log" 2>&1 || true
  fi
  if [[ -n "$platform_pid" ]] && kill -0 "$platform_pid" 2>/dev/null; then
    kill -TERM "$platform_pid" 2>/dev/null || true
    wait "$platform_pid" 2>/dev/null || true
  fi
  if timeout 1 bash -c "</dev/tcp/127.0.0.1/$port" 2>/dev/null; then
    echo "teardown left port $port reachable" >>"$artifact_dir/teardown.log"
    status=1
  fi
  if [[ "$status" -ne 0 ]]; then
    echo "slack-clone live-model E2E failed with status $status; artifacts: $artifact_dir" >&2
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

target_dir="${CARGO_TARGET_DIR:-$repo/target}"
cargo build -p slack-clone --features live-e2e --bin slack-clone-platform \
  --bin slack-clone-live-e2e --locked 2>&1 | tee "$artifact_dir/build.log"

SLACK_CLONE_ADDR="127.0.0.1:$port" \
SLACK_CLONE_DATA_DIR="$state_dir/platform" \
  "$target_dir/debug/slack-clone-platform" >"$platform_log" 2>&1 &
platform_pid=$!
printf '%s\n' "$platform_pid" >"$artifact_dir/platform.pid"

deadline=$((SECONDS + 30))
until curl -fsS "$base_url/healthz" >"$artifact_dir/platform-health.json" 2>/dev/null; do
  if ! kill -0 "$platform_pid" 2>/dev/null; then
    echo "slack-clone platform exited during startup" >&2
    exit 1
  fi
  if ((SECONDS >= deadline)); then
    echo "slack-clone platform did not become ready at $base_url" >&2
    exit 1
  fi
  sleep 0.25
done

args=(--base-url "$base_url" --artifact-dir "$artifact_dir")
if [[ "${1:-}" == "--smoke-only" ]]; then
  args+=(--smoke-only)
fi
"$target_dir/debug/slack-clone-live-e2e" "${args[@]}" \
  2>&1 | tee "$artifact_dir/live-e2e.log"

echo "slack-clone live-model E2E passed: artifacts=$artifact_dir"
