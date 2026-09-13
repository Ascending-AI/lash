#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
if [[ -f "$repo/env.sh" ]]; then
  # Orb generates this file. Cargo metadata must see the same environment as
  # every other Cargo command in the fork.
  # shellcheck source=/dev/null
  source "$repo/env.sh"
fi

bazel="${BAZEL:-bazel}"
if ! command -v "$bazel" >/dev/null 2>&1; then
  echo "hermetic-build: '$bazel' is unavailable; install Bazelisk as bazel" >&2
  exit 127
fi

config=shared
if [[ "${1:-}" == "--local" ]]; then
  config=local
  shift
elif [[ "${1:-}" == "--shared" ]]; then
  shift
fi

operation="${1:-}"
if [[ -z "$operation" ]]; then
  operation=build
else
  shift
fi

cd "$repo"

case "$operation" in
  sync)
    if (($#)); then
      echo "usage: scripts/hermetic-build.sh [--local|--shared] sync" >&2
      exit 2
    fi
    python3 tools/bazel/generate_build_files.py
    "$bazel" mod deps --lockfile_mode=update >/dev/null
    ;;
  clean)
    if (($#)); then
      echo "usage: scripts/hermetic-build.sh clean" >&2
      exit 2
    fi
    # Expunge only this workspace's hashed output base and stop its Bazel
    # server. The shared repository and action caches are separate paths.
    "$bazel" clean --expunge
    ;;
  analyze)
    if (($#)); then
      echo "usage: scripts/hermetic-build.sh [--local|--shared] analyze" >&2
      exit 2
    fi
    python3 tools/bazel/generate_build_files.py --check
    "$bazel" build "--config=$config" --nobuild //:workspace_compile
    ;;
  build)
    python3 tools/bazel/generate_build_files.py --check
    if (($# == 0)); then
      set -- //:workspace_compile
    fi
    "$bazel" build "--config=$config" "$@"
    ;;
  test)
    if (($# == 0)); then
      set -- //:workspace_tests
    fi
    python3 tools/bazel/generate_build_files.py --check
    "$bazel" test "--config=$config" "$@"
    ;;
  *)
    echo "usage: scripts/hermetic-build.sh [--local|--shared] {sync|clean|analyze|build|test} [labels...]" >&2
    exit 2
    ;;
esac
