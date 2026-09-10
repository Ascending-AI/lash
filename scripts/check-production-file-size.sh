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
    */lash-conformance/src/*|*/tests/*|*/test/*|*/testing/*|*/src/tests.rs|*/src/test.rs|*/src/*/tests.rs|*/src/*/test.rs|*/src/*_tests.rs|*/language/support.rs)
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

failures=()
while IFS= read -r -d '' file; do
  rel="${file#./}"
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
