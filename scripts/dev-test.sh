#!/usr/bin/env bash
# Path-plan local tests the same way CI classifies a diff.
#
# Implementer loop default: never starts Postgres, S3, Restate, or E2E.
# Those live on CI. A live URL in the environment is a refuse, not a skip.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

if [ -n "${LASH_POSTGRES_DATABASE_URL:-}" ] || [ -n "${LASH_REQUIRE_POSTGRES:-}" ] \
  || [ -n "${LASH_S3_ENDPOINT:-}" ] || [ -n "${LASH_REQUIRE_S3:-}" ] \
  || [ -n "${LASH_MINIO_ENDPOINT:-}" ]; then
  echo "dev-test: refusing live store URLs; CI owns Postgres/S3/E2E" >&2
  exit 2
fi

if ! command -v kiln >/dev/null 2>&1 || [ ! -f .kiln.bazelrc ]; then
  echo "dev-test: run inside a kiln fork (kiln fork lash <name>)" >&2
  exit 2
fi

base_rev=""
dry_run=0
while [ $# -gt 0 ]; do
  case "$1" in
    --base)
      [ $# -ge 2 ] || { echo "dev-test: --base needs a revision" >&2; exit 2; }
      base_rev="$2"
      shift 2
      ;;
    --dry-run)
      dry_run=1
      shift
      ;;
    -h|--help)
      cat <<'EOF'
usage: scripts/dev-test.sh [--base <rev>] [--dry-run]

Runs the developer suite //:dev_tests narrowed to the changed package
directories: each crates/, examples/, or runbooks/ path selects its
package's `:all` (which excludes manual service gates); a shared input
(manifest, lockfile, toolchain, tools/, scripts/, .github/) widens to the
whole suite. A diff touching the facade crate or a root manifest also
names `//crates/lash:ui_fixtures` explicitly — it is `manual`-tagged, so
no wildcard reaches it (FIG-2801). CI owns //:workspace_tests, the
services, E2E, the deferred Unicode suite, and the workbench browser
test.
EOF
      exit 0
      ;;
    *)
      echo "dev-test: unknown argument '$1'" >&2
      exit 2
      ;;
  esac
done

if [ -z "$base_rev" ]; then
  base_rev="$(git merge-base HEAD origin/main 2>/dev/null || git merge-base HEAD main 2>/dev/null || echo HEAD)"
fi

declare -A packages=()
broad=0
# The trybuild compile-fail fixtures on the public API surface run as the
# `manual`-tagged `//crates/lash:ui_fixtures` target, so no `:all` wildcard
# (and therefore no narrowed or widened package set below) ever compiles
# them — they are data to `cargo check --workspace --all-targets` too. A
# facade diff that breaks a fixture would otherwise report green here and
# red on CI's seal lane one round trip later (FIG-2801). The trigger set
# mirrors CI's `scripts/ci_plan.py` `_is_facade_path`: the facade crate plus
# the root manifests.
facade=0
while IFS= read -r -d '' path; do
  case "$path" in
    *.md|docs/*|LICENSE*|.gitignore)
      ;;
    crates/*/*|examples/*/*|runbooks/*/*)
      dir="$(dirname "$path")"
      while [[ "$dir" =~ ^(crates|examples|runbooks)/[^/]+/.+ ]]; do
        dir="$(dirname "$dir")"
      done
      packages["//${dir}"]=1
      [ "$dir" = "crates/lash" ] && facade=1
      ;;
    Cargo.toml|Cargo.lock)
      broad=1
      facade=1
      ;;
    *)
      broad=1
      ;;
  esac
done < <(
  {
    git diff --name-only --no-renames -z "$base_rev" HEAD
    git diff --name-only --no-renames -z HEAD
    git ls-files --others --exclude-standard -z
  } | sort -zu
)

if [ "$broad" -eq 0 ] && [ "${#packages[@]}" -eq 0 ]; then
  echo "dev-test: nothing to test"
  exit 0
fi

run() {
  echo "+ $*"
  if [ "$dry_run" -eq 0 ]; then
    "$@"
  fi
}

if [ "$broad" -eq 1 ]; then
  echo "dev-test: shared input changed; running //:dev_tests"
  run kiln test
  if [ "$facade" -eq 1 ]; then
    run kiln test //crates/lash:ui_fixtures
  fi
else
  mapfile -t labels < <(printf '%s:all\n' "${!packages[@]}" | sort)
  if [ "$facade" -eq 1 ]; then
    labels+=("//crates/lash:ui_fixtures")
  fi
  echo "dev-test: narrowed to ${#packages[@]} package(s)"
  run kiln test "${labels[@]}"
fi
