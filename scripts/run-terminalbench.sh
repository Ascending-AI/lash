#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

usage() {
  cat <<'EOF'
Run Terminal Bench 2 with lash via Harbor.

Usage:
  scripts/run-terminalbench.sh [options] [-- <extra harbor args>]

Options:
  --dataset <name@version>      Dataset to run (default: terminal-bench-sample@2.0)
  --sample                      Shortcut for --dataset terminal-bench-sample@2.0
  --full                        Shortcut for --dataset terminal-bench@2.0
  --task <glob>                 Task include pattern (repeatable)
  --exclude-task <glob>         Task exclude pattern (repeatable)
  --model <model>               Model passed to lash (optional)
  --jobs-dir <path>             Harbor jobs output dir (default: jobs)
  --job-name <name>             Harbor job name (optional)
  --n-concurrent <int>          Concurrent trials (default: 1)
  --attempts <int>              Attempts per trial (default: 1)
  --timeout-multiplier <float>  Task timeout multiplier (default: 1.0)
  --env <name>                  Harbor environment backend (default: docker)
  --registry-url <url>          Dataset registry URL
                                (default: https://raw.githubusercontent.com/laude-institute/harbor/main/registry.json)
  --build-mode <mode>           Binary build mode: docker-bookworm|host
                                (default: docker-bookworm)
  --no-build                    Skip building lash binary (uses existing path for selected mode)
  --delete                      Delete benchmark environments after run
  --no-delete                   Keep benchmark environments after run (default)
  --allow-no-config             Do not require ~/.lash/config.json
  --dry-run                     Print command and exit
  --help                        Show this help

Examples:
  scripts/run-terminalbench.sh --sample
  scripts/run-terminalbench.sh --full --task "git-*"
  scripts/run-terminalbench.sh --sample --task chess-best-move --model gpt-5.3-codex
EOF
}

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: required command not found: $1" >&2
    exit 1
  fi
}

DATASET="terminal-bench-sample@2.0"
JOBS_DIR="jobs"
JOB_NAME=""
MODEL=""
N_CONCURRENT="1"
ATTEMPTS="1"
TIMEOUT_MULT="1.0"
ENV_BACKEND="docker"
REGISTRY_URL="https://raw.githubusercontent.com/laude-institute/harbor/main/registry.json"
BUILD_MODE="docker-bookworm"
DO_BUILD=1
DELETE_AFTER_RUN=0
REQUIRE_CONFIG=1
DRY_RUN=0
DEBUG=1

TASK_PATTERNS=()
EXCLUDE_PATTERNS=()
EXTRA_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dataset)
      DATASET="${2:?missing value for --dataset}"
      shift 2
      ;;
    --sample)
      DATASET="terminal-bench-sample@2.0"
      shift
      ;;
    --full)
      DATASET="terminal-bench@2.0"
      shift
      ;;
    --task)
      TASK_PATTERNS+=("${2:?missing value for --task}")
      shift 2
      ;;
    --exclude-task)
      EXCLUDE_PATTERNS+=("${2:?missing value for --exclude-task}")
      shift 2
      ;;
    --model)
      MODEL="${2:?missing value for --model}"
      shift 2
      ;;
    --jobs-dir)
      JOBS_DIR="${2:?missing value for --jobs-dir}"
      shift 2
      ;;
    --job-name)
      JOB_NAME="${2:?missing value for --job-name}"
      shift 2
      ;;
    --n-concurrent)
      N_CONCURRENT="${2:?missing value for --n-concurrent}"
      shift 2
      ;;
    --attempts)
      ATTEMPTS="${2:?missing value for --attempts}"
      shift 2
      ;;
    --timeout-multiplier)
      TIMEOUT_MULT="${2:?missing value for --timeout-multiplier}"
      shift 2
      ;;
    --env)
      ENV_BACKEND="${2:?missing value for --env}"
      shift 2
      ;;
    --registry-url)
      REGISTRY_URL="${2:?missing value for --registry-url}"
      shift 2
      ;;
    --build-mode)
      BUILD_MODE="${2:?missing value for --build-mode}"
      shift 2
      ;;
    --no-build)
      DO_BUILD=0
      shift
      ;;
    --delete)
      DELETE_AFTER_RUN=1
      shift
      ;;
    --no-delete)
      DELETE_AFTER_RUN=0
      shift
      ;;
    --allow-no-config)
      REQUIRE_CONFIG=0
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    --)
      shift
      EXTRA_ARGS+=("$@")
      break
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

require_cmd cargo
require_cmd harbor
require_cmd docker

if [[ "${REQUIRE_CONFIG}" -eq 1 && ! -f "${HOME}/.lash/config.json" ]]; then
  cat >&2 <<EOF
error: ${HOME}/.lash/config.json not found.
This runner expects your local lash provider config (including OAuth tokens).
Use --allow-no-config to bypass.
EOF
  exit 1
fi

build_host_binary() {
  echo "==> Building lash release binary on host" >&2
  cargo build --release --manifest-path "${REPO_ROOT}/Cargo.toml" --bin lash >/dev/null
  echo "${REPO_ROOT}/target/release/lash"
}

build_bookworm_binary() {
  local target_dir="${REPO_ROOT}/target-bookworm"
  mkdir -p "${target_dir}"
  echo "==> Building lash release binary in rust:1-bookworm (glibc-compatible)" >&2
  docker run --rm -u root \
    -v "${REPO_ROOT}:/work" \
    -w /work \
    rust:1-bookworm \
    bash -lc \
      'apt-get update >/dev/null &&
       apt-get install -y protobuf-compiler zstd python3-dev >/dev/null &&
       . /usr/local/cargo/env &&
       CARGO_TARGET_DIR=/work/target-bookworm cargo build --release --manifest-path /work/Cargo.toml --bin lash &&
       chown -R $(stat -c "%u:%g" /work) /work/target-bookworm' >/dev/null
  echo "${REPO_ROOT}/target-bookworm/release/lash"
}

if [[ "${BUILD_MODE}" != "docker-bookworm" && "${BUILD_MODE}" != "host" ]]; then
  echo "error: unsupported --build-mode: ${BUILD_MODE} (expected docker-bookworm|host)" >&2
  exit 2
fi

if [[ "${BUILD_MODE}" == "host" ]]; then
  BINARY_PATH="${REPO_ROOT}/target/release/lash"
  if [[ "${DO_BUILD}" -eq 1 ]]; then
    BINARY_PATH="$(build_host_binary)"
  fi
else
  BINARY_PATH="${REPO_ROOT}/target-bookworm/release/lash"
  if [[ "${DO_BUILD}" -eq 1 ]]; then
    BINARY_PATH="$(build_bookworm_binary)"
  fi
fi

if [[ ! -x "${BINARY_PATH}" ]]; then
  echo "error: expected executable lash binary not found at ${BINARY_PATH}" >&2
  exit 1
fi

export LASH_BENCH_BINARY="${BINARY_PATH}"

if [[ -z "${LASH_PROMPT_REPLACE_IDENTITY:-}" ]]; then
  export LASH_PROMPT_REPLACE_IDENTITY="$(cat <<EOF
You are running inside a benchmark harness with an enforced wall-clock time budget.
Work autonomously and prioritize passing the verifier over polish.
Do not ask the user questions; there is no interactive user in this run.
Make concrete progress continuously: inspect, edit, run checks, and converge quickly.
If you are blocked or near timeout, return the best valid result you can and call done() with a concise status + remaining risks.
EOF
)"
fi

# Always capture LLM request/response traces for benchmark debugging.
export LASH_LOG="debug"

export PYTHONPATH="${REPO_ROOT}:${PYTHONPATH:-}"

CMD=(
  harbor run
  --agent-import-path scripts.harbor_lash_agent:LashAgent
  --dataset "${DATASET}"
  --registry-url "${REGISTRY_URL}"
  --env "${ENV_BACKEND}"
  --jobs-dir "${JOBS_DIR}"
  --n-concurrent "${N_CONCURRENT}"
  --n-attempts "${ATTEMPTS}"
  --timeout-multiplier "${TIMEOUT_MULT}"
)

if [[ -n "${JOB_NAME}" ]]; then
  CMD+=(--job-name "${JOB_NAME}")
fi

if [[ -n "${MODEL}" ]]; then
  CMD+=(--model "${MODEL}")
fi

if [[ "${DELETE_AFTER_RUN}" -eq 0 ]]; then
  CMD+=(--no-delete)
fi

if [[ "${DEBUG}" -eq 1 ]]; then
  CMD+=(--debug)
fi

for pattern in "${TASK_PATTERNS[@]}"; do
  CMD+=(--task-name "${pattern}")
done

for pattern in "${EXCLUDE_PATTERNS[@]}"; do
  CMD+=(--exclude-task-name "${pattern}")
done

if [[ ${#EXTRA_ARGS[@]} -gt 0 ]]; then
  CMD+=("${EXTRA_ARGS[@]}")
fi

echo "==> Running: ${CMD[*]}"

if [[ "${DRY_RUN}" -eq 1 ]]; then
  exit 0
fi

"${CMD[@]}"
