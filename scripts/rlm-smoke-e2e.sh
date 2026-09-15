#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$repo"
# The Kiln-provided build geometry is mandatory for every cargo command.
# shellcheck source=env.sh
source "$repo/env.sh"

artifact_root="${LASH_RLM_SMOKE_ARTIFACT_DIR:-}"
if [[ -z "$artifact_root" ]]; then
  artifact_root="$(mktemp -d "${TMPDIR:-/tmp}/lash-rlm-smoke.XXXXXX")"
fi
mkdir -p "$artifact_root"
artifact_root="$(cd "$artifact_root" && pwd -P)"
build_log="$artifact_root/build.log"

# Built through Bazel so this gate shares the box's action cache with every
# other checkout instead of compiling the workspace again into its own Cargo
# target directory (FIG-3153). The geometry is unchanged: `cargo build` without
# a profile and Bazel's default `fastbuild` are both `-C opt-level=0` with
# debug assertions on, which is what this host has always run under.
# `LASH_RLM_SMOKE_HOST_BIN` launches a prebuilt binary and skips the build.
smoke_host_label='//runbooks/rlm-smoke:rlm-smoke'
smoke_host_bin="${LASH_RLM_SMOKE_HOST_BIN:-}"
if [[ -z "$smoke_host_bin" ]]; then
  symlink_prefix="$artifact_root/bazel-"
  if command -v kiln >/dev/null 2>&1; then
    build_command=(kiln build)
  else
    build_command=("$repo/scripts/hermetic-build.sh" build)
  fi
  "${build_command[@]}" "--symlink_prefix=$symlink_prefix" "$smoke_host_label" \
    2>&1 | tee "$build_log"
  smoke_host_bin="${symlink_prefix}bin/runbooks/rlm-smoke/rlm-smoke"
fi
[[ -x "$smoke_host_bin" ]] || {
  echo "rlm-smoke-host binary is missing at $smoke_host_bin" >&2
  exit 1
}

if [[ -z "${OPENROUTER_API_KEY:-}" ]]; then
  echo "STOP: OPENROUTER_API_KEY is unset; built rlm-smoke-host but ran no live-model rows. Artifacts: $artifact_root" >&2
  exit 78
fi

# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"
lash_gate_acquire "rlm-smoke-e2e"
trap lash_gate_cleanup EXIT

if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "Docker is required for the workspace-jailed exec tool." >&2
  exit 69
fi

sandbox_image="${RLM_SMOKE_SANDBOX_IMAGE:-alpine:3.22}"
docker pull "$sandbox_image" >"$artifact_root/sandbox-image.log" 2>&1

scenarios=(file-edit-bugfix missing-helper-file config-contract-edit)
# TypeScript is the sole RLM language (ADR 0096): one row per scenario. The
# `dialect` path segment stays so the artifact layout the matrix declares is
# unchanged.
dialects=(typescript)
model="${OPENROUTER_MODEL:-deepseek/deepseek-v4-flash}"
run_nonce="$(date +%s)-$$"
row_index=0

for scenario in "${scenarios[@]}"; do
  scenario_dir="$repo/runbooks/rlm-smoke/cases/$scenario"
  for dialect in "${dialects[@]}"; do
    row_index=$((row_index + 1))
    row_dir="$artifact_root/$scenario/$dialect"
    workspace="$row_dir/workspace"
    data_dir="$row_dir/data"
    host_artifacts="$row_dir/artifacts"
    session_id="rlm-smoke-$run_nonce-$row_index-$scenario-$dialect"
    port=$((LASH_E2E_PORT_BASE + row_index - 1))
    trace_offset=$((row_index * 1000000))
    mkdir -p "$workspace" "$data_dir" "$host_artifacts"
    cp -a "$scenario_dir/workspace/." "$workspace/"
    chmod -R u+rwX "$workspace"

    "$smoke_host_bin" \
        --scenario "$scenario" \
        --scenario-dir "$scenario_dir" \
        --workspace "$workspace" \
        --data-dir "$data_dir" \
        --artifact-dir "$host_artifacts" \
        --session-id "$session_id" \
        --port "$port" \
        --trace-offset "$trace_offset" \
        --model "$model" \
        --sandbox-image "$sandbox_image" \
        2>&1 | tee "$row_dir/host.log"

    bash "$scenario_dir/check.sh" "$workspace" "$scenario_dir" \
      >"$row_dir/oracle.log" 2>&1
    host_line="$(sed -n 's/^HOST //p' "$row_dir/host.log" | tail -n 1)"
    if [[ -z "$host_line" ]]; then
      echo "Host evidence line missing for $scenario/$dialect" >&2
      exit 1
    fi
    echo "PASS $host_line artifacts=$row_dir"
  done
done
