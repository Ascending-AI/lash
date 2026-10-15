#!/usr/bin/env bash
# A host's library graph must compile without development feature unification,
# and each optional capability links only what its feature names (FIG-6378):
# code mode and dialect helper releases link no editor, view or in-process
# parser; a viewer links no dialect; authoring is the editor's opt-in.
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

# Each named selection, then the packages its graph may not contain. Normal and
# build edges count for the editor and view; parsers are refused on normal
# edges only, because a helper release's build script compiles its dialect to
# generate release data without linking it into the host.
inference='codemode,rlm,worker,dialect-typescript,dialect-python,sqlite'
parent='codemode,rlm,dialect-typescript,dialect-python,sqlite'
viewer='workflow-view'
authoring='workflow-authoring'
selections=(
  "$inference|normal,build|lash-kernel-edit lash-kernel-view"
  "$parent|normal|lash-dialect-typescript lash-dialect-python"
  "$viewer|normal,build|lash-kernel-edit lash-dialect-typescript lash-dialect-python lash-vm-runtime"
  "$authoring|normal,build|lash-dialect-typescript lash-dialect-python"
)

# Every selection's graph is read before anything compiles, so one run names
# every leaked edge.
leaks=0
for selection in "${selections[@]}"; do
  IFS='|' read -r selected edges refused <<<"$selection"
  if ! graph="$(cargo tree "${args[@]}" --features "$selected" --edges "$edges" --prefix none --format '{p}' 2>&1)"; then
    printf 'facade selection %s does not resolve:\n%s\n' "$selected" "$graph" >&2
    leaks=1
    continue
  fi
  for package in $refused; do
    if grep -Eq "^$package v" <<<"$graph"; then
      printf 'facade selection %s links %s through %s edges\n' "$selected" "$package" "$edges" >&2
      leaks=1
    fi
  done
done
if [ "$leaks" -ne 0 ]; then
  exit 1
fi

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
done < <(printf '%s\n' "$feature_sets" "$inference" "$parent" "$viewer" "$authoring")
printf 'facade production passed: %s feature sets, including the inference, parent, viewer and authoring selections, testing off\n' "$checked"
