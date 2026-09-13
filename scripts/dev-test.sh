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

Classify merge-base..HEAD (plus a dirty tree) with scripts/ci_plan.py and run
only the matching local families:

  docs-only     nothing to compile
  workbench     cargo nextest with the workbench filter
  rust          kiln test (cacheable Bazel partition) when kiln is available,
                otherwise cargo nextest with the cargo-owned PR filter
  regress       bazel test //:deferred_tests or cargo -p lash-regress unicodesets

Never runs Postgres, S3, or E2E.
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

paths_file="$(mktemp)"
trap 'rm -f "$paths_file"' EXIT
{
  git diff --name-status --no-renames -z "$base_rev" HEAD
  git diff --name-status --no-renames -z HEAD
} >"$paths_file"

if [ ! -s "$paths_file" ]; then
  echo "dev-test: empty diff against $base_rev; nothing to run"
  exit 0
fi

plan="$(python3 scripts/ci_plan.py classify --paths-file "$paths_file")"
docs_only="$(printf '%s\n' "$plan" | awk -F= '/^docs_only=/{print $2}')"
rust="$(printf '%s\n' "$plan" | awk -F= '/^rust=/{print $2}')"
workbench="$(printf '%s\n' "$plan" | awk -F= '/^workbench=/{print $2}')"
regress="$(printf '%s\n' "$plan" | awk -F= '/^regress=/{print $2}')"
reason="$(printf '%s\n' "$plan" | awk -F= '/^reason=/{print substr($0,8)}')"

echo "dev-test: $reason (rust=$rust workbench=$workbench regress=$regress)"

run() {
  echo "+ $*"
  if [ "$dry_run" -eq 0 ]; then
    "$@"
  fi
}

if [ "$docs_only" = "true" ]; then
  echo "dev-test: docs-only; skipping compile/test"
  exit 0
fi

if [ "$rust" = "true" ]; then
  if command -v kiln >/dev/null 2>&1 && [ -x scripts/hermetic-build.sh ]; then
    run kiln test
  else
    run cargo nextest run --profile ci --workspace --locked \
      -E "$(<tools/bazel/cargo_owned_nextest_filter.txt)"
  fi
fi

if [ "$workbench" = "true" ]; then
  run cargo nextest run --profile ci --workspace --locked \
    -E "$(<tools/bazel/workbench_nextest_filter.txt)"
fi

if [ "$regress" = "true" ]; then
  if command -v bazel >/dev/null 2>&1; then
    run bazel test //:deferred_tests
  else
    run cargo test -p lash-regress --test unicodesets --locked
  fi
fi
