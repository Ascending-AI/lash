#!/usr/bin/env bash
set -euo pipefail

production_limit="${LASH_PRODUCTION_RUST_LINE_LIMIT:-1600}"
test_limit="${LASH_TEST_RUST_LINE_LIMIT:-2500}"

if (($#)); then
  roots=("$@")
else
  roots=(".")
fi

is_test_rust_file() {
  local file="$1"
  case "$file" in
    */tests/*|*/test/*|*/testing/*|*/src/tests.rs|*/src/test.rs|*/src/*/tests.rs|*/src/*/test.rs|*/src/*_tests.rs|*/language/support.rs)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

rust_line_limit_for() {
  local rel="$1"
  if is_test_rust_file "$rel"; then
    printf '%s' "$test_limit"
  else
    printf '%s' "$production_limit"
  fi
}

budget_line_count() {
  local file="$1"
  # Rustdoc expands the public contract without increasing implementation
  # complexity. Keep the production ceiling on source lines while allowing a
  # documentation-only change to describe an otherwise unchanged file.
  awk '!/^[[:space:]]*\/\/[/!]/ { lines += 1 } END { print lines + 0 }' "$file"
}

# Pre-existing files that exceed the budget, exempt with a one-line reason.
# This is an explicit, closed list — a new file over budget still fails hard.
# The intended direction of travel is to shrink an entry below its limit and
# delete its line here, never to raise the global budget. Entries are
# self-expiring: this script fails when an allowlisted path no longer exists or
# has shrunk to its limit, so a stale exemption cannot sit here unnoticed.
declare -A allowlist=(
  ["crates/lash-sim/src/oracles.rs"]="cross-backend divergence oracle catalogue; split tracked separately"
  ["crates/lash-sim/src/provider.rs"]="scripted deterministic sim provider surface"
  ["crates/lash-sqlite-store/src/persistence.rs"]="SQLite RuntimePersistence implementation"
  ["crates/lash-postgres-store/src/postgres/runtime_persistence.rs"]="Postgres RuntimePersistence implementation"
  ["crates/lashlang/src/parser.rs"]="Lashlang recursive-descent parser"
  ["crates/lash-protocol-rlm/src/executor.rs"]="RLM protocol turn executor"
  ["crates/lash-remote-protocol/src/core_conversions/processes.rs"]="process DTO <-> core conversions"
  ["crates/lash-core/src/runtime/turn_loop.rs"]="core runtime turn loop"
  ["crates/lash-perf/src/runtime_perf/providers.rs"]="dev-only runtime perf provider harness"
  ["runbooks/restate-postgres-workers/src/bin/runner.rs"]="distributed-workers e2e runner binary"
  ["crates/lash-restate/src/tests.rs"]="Restate backend test suite"
  ["crates/lashlang/tests/language.rs"]="Lashlang language conformance test suite"
  ["crates/lash-core/src/runtime/tests/turns.rs"]="core turn-loop test suite"
  ["crates/lash-core/src/testing/conformance/runtime_persistence.rs"]="RuntimePersistence conformance suite"
  ["crates/lash/src/tests/turn_streaming.rs"]="facade turn-streaming test suite"
)

# Allowlist entries are measured before the tree walk so an exemption that no
# longer earns its place fails the gate instead of silently lingering. Paths are
# repository-relative; run this script from the repository root.
stale_exemptions=()
for rel in "${!allowlist[@]}"; do
  if [[ ! -f "$rel" ]]; then
    stale_exemptions+=("missing:$rel")
    continue
  fi
  limit="$(rust_line_limit_for "$rel")"
  lines=$(wc -l < "$rel")
  if ((lines <= limit)); then
    stale_exemptions+=("within-budget:${lines}<=${limit}:$rel")
  fi
done

if ((${#stale_exemptions[@]})); then
  echo "Stale entries in the file-size allowlist (delete them):" >&2
  echo "  production limit: ${production_limit} lines" >&2
  echo "  test/support limit: ${test_limit} lines" >&2
  mapfile -t stale_exemptions < <(printf '%s\n' "${stale_exemptions[@]}" | sort)
  printf '  %s\n' "${stale_exemptions[@]}" >&2
  exit 1
fi

failures=()
while IFS= read -r -d '' file; do
  rel="${file#./}"
  if [[ -n "${allowlist[$rel]+set}" ]]; then
    continue
  fi
  limit="$(rust_line_limit_for "$rel")"
  if is_test_rust_file "$rel"; then
    kind="test"
  else
    kind="production"
  fi

  lines=$(budget_line_count "$file")
  if ((lines > limit)); then
    failures+=("$kind:$lines:$rel")
  fi
done < <(
  find "${roots[@]}" \
    \( \
      -path '*/.git' -o \
      -path '*/.git/*' -o \
      -path '*/.claude' -o \
      -path '*/.claude/*' -o \
      -path '*/target' -o \
      -path '*/target/*' -o \
      -path '*/.tgt' -o \
      -path '*/.tgt/*' -o \
      -path '*/vendor' -o \
      -path '*/vendor/*' -o \
      -path '*/crates/lash-regress' -o \
      -path '*/crates/lash-regress/*' -o \
      -path '*/vendored' -o \
      -path '*/vendored/*' -o \
      -path '*/generated' -o \
      -path '*/generated/*' \
    \) -prune -o \
    -type f -name '*.rs' -print0
)

if ((${#failures[@]})); then
  echo "Rust files over line budget:" >&2
  echo "  production limit: ${production_limit} lines" >&2
  echo "  test/support limit: ${test_limit} lines" >&2
  printf '  %s\n' "${failures[@]}" >&2
  exit 1
fi
