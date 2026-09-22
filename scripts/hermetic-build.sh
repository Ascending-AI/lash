#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
if [[ -f "$repo/env.sh" ]]; then
  # Kiln generates this file. Cargo metadata must see the same environment as
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

usage() {
  echo "usage: scripts/hermetic-build.sh [--local|--shared] {sync|clean|fmt|analyze|build|check|test|clippy|doc|run} [labels...] [-- args...]" >&2
}

# The remote-executing operations need .kiln.bazelrc: kiln writes it on every
# fork and golden refresh with the pool endpoint, the client certificate, the
# pinned runtime image and this host's caches. .bazelrc `try-import`s it, so
# without it `--config=shared` would quietly build with no executor at all.
# fmt, clean and sync never execute remote actions and stay usable offline.
case "$operation" in
  analyze|build|check|test|clippy|doc|run)
    if [[ "$config" == shared && ! -f "$repo/.kiln.bazelrc" ]]; then
      echo "hermetic-build: $repo/.kiln.bazelrc is missing, so there is no" \
        "shared executor to build on. Re-create the fork with 'kiln fork'," \
        "or pass --local to execute actions in this checkout." >&2
      exit 1
    fi
    ;;
esac

case "$operation" in
  sync)
    if (($#)); then
      echo "usage: scripts/hermetic-build.sh [--local|--shared] sync" >&2
      exit 2
    fi
    python3 tools/bazel/generate_build_files.py
    python3 scripts/check_runtime_off_graph.py --sync
    "$bazel" mod deps --lockfile_mode=update >/dev/null
    ;;
  clean)
    if (($#)); then
      echo "usage: scripts/hermetic-build.sh clean" >&2
      exit 2
    fi
    # Expunge only this workspace's hashed output base and stop its Bazel
    # The shared repository and action caches are separate paths.
    "$bazel" clean --expunge
    ;;
  fmt)
    # Formatting is a local rewrite (or check with `-- --check`); there is no
    # compilation to cache, so this stays Cargo.
    cargo fmt --all "$@"
    ;;
  analyze)
    if (($#)); then
      echo "usage: scripts/hermetic-build.sh [--local|--shared] analyze" >&2
      exit 2
    fi
    python3 tools/bazel/generate_build_files.py --check
    "$bazel" build "--config=$config" --nobuild //:workspace_compile
    ;;
  build|check)
    python3 tools/bazel/generate_build_files.py --check
    if (($# == 0)); then
      set -- //:workspace_compile
    fi
    "$bazel" build "--config=$config" "$@"
    ;;
  clippy)
    python3 tools/bazel/generate_build_files.py --check
    if (($# == 0)); then
      set -- //:workspace_clippy
    fi
    "$bazel" build "--config=$config" "$@"
    ;;
  doc)
    python3 tools/bazel/generate_build_files.py --check
    if (($# == 0)); then
      set -- //:workspace_docs
    fi
    "$bazel" build "--config=$config" --remote_download_outputs=toplevel "$@"
    ;;
  test)
    if (($# == 0)); then
      set -- //:dev_tests
    fi
    python3 tools/bazel/generate_build_files.py --check
    "$bazel" test "--config=$config" "$@"
    ;;
  run)
    if (($# == 0)); then
      echo "usage: scripts/hermetic-build.sh [--local|--shared] run <label> [-- args...]" >&2
      exit 2
    fi
    python3 tools/bazel/generate_build_files.py --check
    # Compilation goes to the pool; the program itself starts locally, so its
    # toplevel output must be downloaded first.
    "$bazel" run "--config=$config" --remote_download_outputs=toplevel "$@"
    ;;
  *)
    usage
    exit 2
    ;;
esac
