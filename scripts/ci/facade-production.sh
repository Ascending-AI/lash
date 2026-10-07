#!/usr/bin/env bash
# A host's library graph must compile without development feature unification.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo"
. ./env.sh
out="${LASH_CARGO_PARITY_OUT_DIR:-$repo/.kiln/cargo-parity}"
mkdir -p "$out"
export KILN_CARGO_ROUTE=
# synthetic-next is an upgrade-harness build, not a host extension.
features="$(python3 - <<'PY'
import tomllib
from pathlib import Path
manifest = tomllib.loads(Path('crates/lash/Cargo.toml').read_text())
print(','.join(sorted(manifest['features'].keys() - {'testing', 'synthetic-next'})))
PY
)"
args=(--package lash-runtime --no-default-features --locked --offline)
for extensions in '' "$features"; do
  request=("${args[@]}")
  if [ -n "$extensions" ]; then
    request+=(--features "$extensions")
  fi
  graph="$(cargo tree "${request[@]}" --edges normal,build --prefix none --format '{p} {f}')"
  if [[ "$graph" =~ (^|[[:space:],])testing($|[[:space:],]) ]]; then
    printf '%s\n' "$graph" >&2
    printf 'facade production graph enables testing\n' >&2
    exit 1
  fi
  cargo check "${request[@]}" --lib --target-dir "$out/target"
done
printf 'facade production parity passed: baseline and every host extension, testing off\n'
