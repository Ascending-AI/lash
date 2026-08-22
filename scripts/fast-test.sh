#!/usr/bin/env bash
# Change-scoped test selection for iteration.
#
# Maps the files changed since the merge base onto the workspace crates that
# own them, then runs the workspace suite narrowed to those crates and
# everything that depends on them.
#
# Narrowing is only ever safe when the mapping is certain, so this script is
# fail-closed: anything it cannot attribute to exactly one workspace crate, and
# any input that is shared across the whole build graph (manifests, the lock
# file, the toolchain pin, cargo/nextest config, these scripts, CI workflows),
# falls back to the full workspace suite and says so. An empty changed-crate set
# is a fallback too, never a "nothing to run".
#
# This is an iteration and re-run tool. The pre-push battery and CI stay
# full-suite; see docs/agents/way-of-working.md.
#
# nextest's `rdeps(P)` filterset matches tests in the reverse-dependency closure
# of P *including P itself* (nexte.st/docs/filtersets: rdeps is "the package and
# all packages that depend on it"), so the union of `rdeps()` over the changed
# crates already covers the changed crates' own tests — there is no separate
# `package()` term to add. scripts/test_fast_test.py pins that claim against the
# real workspace.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

base_rev=""
dry_run=0
classify_only=0

usage() {
  cat <<'EOF'
usage: scripts/fast-test.sh [--base <rev>] [--dry-run]
       scripts/fast-test.sh --classify [path ...]

  --base <rev>  Compare against <rev> instead of `git merge-base HEAD origin/main`.
  --dry-run     Print the cargo nextest command that would run, and exit 0.
  --classify    Classify the given repo-relative paths (or paths on stdin, one
                per line) without touching git or cargo nextest. Prints either
                `FULL <reason>` or `CRATES <name> [<name> ...]`. Everything
                after --classify is a path, so it must come last.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --base)
      [ $# -ge 2 ] || { echo "fast-test: --base needs a revision" >&2; exit 2; }
      base_rev="$2"
      shift 2
      ;;
    --base=*)
      base_rev="${1#--base=}"
      shift
      ;;
    --dry-run)
      dry_run=1
      shift
      ;;
    --classify)
      classify_only=1
      shift
      break
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "fast-test: unknown argument '$1'" >&2
      usage >&2
      exit 2
      ;;
  esac
done

# Everything after --classify is taken as a path, so a flag there would be
# classified as a file and produce a baffling FULL verdict. Reject it instead.
if [ "$classify_only" -eq 1 ]; then
  for arg in "$@"; do
    case "$arg" in
      -*)
        echo "fast-test: --classify takes paths, not flags ('$arg'); put flags before --classify" >&2
        exit 2
        ;;
    esac
  done
fi

metadata_file="$(mktemp)"
# `exec` replaces the shell image and never fires an EXIT trap, so the two run
# paths call this explicitly before handing off to cargo.
cleanup() {
  rm -f "$metadata_file"
}
trap cleanup EXIT
cargo metadata --no-deps --format-version 1 >"$metadata_file"

# Renders a command so it can be pasted back into a shell: the `-E` filterset
# argument contains parentheses, which are a syntax error unquoted.
format_command() {
  local part
  local rendered=""
  for part in "$@"; do
    rendered+="${rendered:+ }$(printf '%q' "$part")"
  done
  printf '%s' "$rendered"
}

# Reads repo-relative paths on stdin, writes one line: `FULL <reason>` or
# `CRATES <name>...`. Kept in one place so --classify and the real run share it.
# The program is held in a variable rather than piped as a heredoc so that the
# classifier's own stdin stays available for the path list.
classify_program="$(cat <<'PY'
import json
import os
import sys

metadata_path = sys.argv[1]
with open(metadata_path, encoding="utf-8") as handle:
    metadata = json.load(handle)

# The caller has already cd'd to the repository root.
root = os.getcwd()

# package name -> repo-relative directory owning it
owners = []
for package in metadata["packages"]:
    manifest_dir = os.path.dirname(os.path.abspath(package["manifest_path"]))
    rel = os.path.relpath(manifest_dir, root)
    rel = "" if rel == "." else rel
    owners.append((package["name"], rel))

# Inputs that no single crate owns: a change to any of them can move any test in
# the workspace, so they are unconditionally full-suite.
SHARED_PREFIXES = (".config/", ".cargo/", "scripts/", ".github/")
SHARED_EXACT = ("Cargo.lock",)


def shared_reason(path: str):
    if os.path.basename(path) == "Cargo.toml":
        return f"{path} is a manifest"
    if path in SHARED_EXACT:
        return f"{path} is a shared build input"
    if os.path.basename(path).startswith("rust-toolchain"):
        return f"{path} pins the toolchain"
    for prefix in SHARED_PREFIXES:
        if path == prefix.rstrip("/") or path.startswith(prefix):
            return f"{path} is under {prefix}"
    return None


def owner_of(path: str):
    # A file belongs to a crate when exactly one crate directory is an ancestor
    # of it. Zero owners (docs, fixtures at the root) and more than one owner
    # (nested crate directories) are both "not certain", hence full-suite.
    #
    # A workspace-root package sits in the repo root and would therefore be an
    # ancestor of every path; an empty directory is not an attribution, so it is
    # excluded here rather than owning the whole tree.
    matches = [
        name
        for name, directory in owners
        if directory and (path == directory or path.startswith(directory + "/"))
    ]
    if len(matches) == 1:
        return matches[0]
    return None


paths = [line.strip() for line in sys.stdin if line.strip()]
selected = set()
for path in paths:
    reason = shared_reason(path)
    if reason is not None:
        print(f"FULL {reason}")
        raise SystemExit(0)
    owner = owner_of(path)
    if owner is None:
        print(f"FULL {path} is not owned by exactly one workspace crate")
        raise SystemExit(0)
    selected.add(owner)

if not selected:
    print("FULL the changed-crate set is empty")
    raise SystemExit(0)

print("CRATES " + " ".join(sorted(selected)))
PY
)"

classify() {
  python3 -c "$classify_program" "$metadata_file"
}

if [ "$classify_only" -eq 1 ]; then
  if [ $# -gt 0 ]; then
    printf '%s\n' "$@" | classify
  else
    classify
  fi
  exit 0
fi

if [ -z "$base_rev" ]; then
  if ! base_rev="$(git merge-base HEAD origin/main 2>/dev/null)"; then
    echo "fast-test: no merge base with origin/main; run 'git fetch origin main'" >&2
    exit 2
  fi
fi

# Committed changes since the base plus the dirty worktree plus untracked files:
# the whole point is to test what you are editing right now.
#
# `--no-renames` is load-bearing: with git's default rename detection a file
# moved from crate A to crate B is reported as the destination path only, so
# A would lose code and never be selected. Disabling detection reports the move
# as a delete plus an add, and both crates are selected.
changed_files="$(
  {
    git diff --name-only --no-renames "$base_rev" --
    git ls-files --others --exclude-standard
  } | sort -u
)"

verdict="$(printf '%s\n' "$changed_files" | classify)"

run_full() {
  echo "fast-test: full workspace suite ($1)"
  local command=(cargo nextest run --workspace --all-targets --locked)
  echo "fast-test: $(format_command "${command[@]}")"
  if [ "$dry_run" -eq 1 ]; then
    exit 0
  fi
  cleanup
  exec "${command[@]}"
}

case "$verdict" in
  FULL\ *)
    run_full "${verdict#FULL }"
    ;;
  CRATES\ *)
    ;;
  *)
    echo "fast-test: classifier produced no verdict" >&2
    exit 2
    ;;
esac

read -r -a crates <<<"${verdict#CRATES }"

filterset=""
for crate in "${crates[@]}"; do
  if [ -n "$filterset" ]; then
    filterset="$filterset + "
  fi
  filterset="${filterset}rdeps(=$crate)"
done

echo "fast-test: base $base_rev"
echo "fast-test: selected packages: ${crates[*]}"
echo "fast-test: filterset: $filterset"

command=(cargo nextest run --workspace --all-targets --locked -E "$filterset")
echo "fast-test: $(format_command "${command[@]}")"

if [ "$dry_run" -eq 1 ]; then
  exit 0
fi

# Count first: a filterset that selects nothing is a selection bug, and a run of
# zero tests exits 0, which would read as a pass.
# `cargo nextest list` writes one `<binary-id> <test-name>` line per selected
# test on stdout; compile progress goes to stderr and is left on the terminal.
listing="$(cargo nextest list --workspace --all-targets --locked -E "$filterset")"
selected_tests="$(printf '%s' "$listing" | grep -c . || true)"
echo "fast-test: selected tests: $selected_tests"
if [ "$selected_tests" -eq 0 ]; then
  echo "fast-test: filterset selected zero tests; refusing to report a pass" >&2
  exit 1
fi

cleanup
exec "${command[@]}"
