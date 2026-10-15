#!/usr/bin/env bash
# A host's library graph must compile without development feature unification.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo"
if [ -f env.sh ]; then
  . ./env.sh
fi
out="${LASH_CARGO_PARITY_OUT_DIR:-$repo/.kiln/cargo-parity}"
mkdir -p "$out"
export KILN_CARGO_ROUTE=
# synthetic-next is an upgrade-harness build, not a host extension.
feature_sets="$(python3 - <<'PY'
import tomllib
from pathlib import Path
manifest = tomllib.loads(Path('crates/lash/Cargo.toml').read_text())
features = sorted(manifest['features'].keys() - {'testing', 'synthetic-next'})
# Minimal, explicit default, each host extension, then their union.
# Include default by name so changes to its members are exercised too.
print('')
for feature in features:
    print(feature)
print(','.join(features))
PY
)"
args=(--package lash-runtime --no-default-features --locked)
checked=0
while IFS= read -r extensions; do
  request=("${args[@]}")
  if [ -n "$extensions" ]; then
    request+=(--features "$extensions")
  fi
  printf 'Checking production facade: %s (testing off)\n' "${extensions:-minimal}"
  graph="$(cargo tree "${request[@]}" --edges normal,build --prefix none --format '{p} {f}')"
  if [[ "$graph" =~ (^|[[:space:],])testing($|[[:space:],]) ]]; then
    printf '%s\n' "$graph" >&2
    printf 'facade production graph enables testing\n' >&2
    exit 1
  fi
  cargo check "${request[@]}" --lib --target-dir "$out/target"
  checked=$((checked + 1))
done <<< "$feature_sets"
printf 'facade production passed: %s feature sets, testing off\n' "$checked"
