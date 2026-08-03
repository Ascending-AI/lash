#!/usr/bin/env bash
# Fail on any rustdoc diagnostic in the two host-facing crates and require
# integrator-contract rustdoc on lash-core's retained public member surface.
#
# The public API docs are the contract hosts read, so a link that silently
# stopped resolving is drift the API example-coverage inventory cannot catch:
# the symbol is still exported, only its documentation is now wrong. Both
# feature configurations are linted because `all-features` exposes surface
# (`testing`, conformance suites) the default build never documents.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

export RUSTDOCFLAGS="${RUSTDOCFLAGS:-} -D warnings"

cargo doc -p lash-runtime -p lash-core --no-deps --locked
cargo doc -p lash-runtime -p lash-core --no-deps --all-features --locked

# `missing_docs` is crate-wide and also fires on thousands of items outside
# ADR 0051's member-level closure. Generate rustdoc JSON instead so the
# compiler resolves root exports, ownership, visibility, and inherent impls;
# the targeted check then fails every future undocumented core-owned member.
RUSTC_BOOTSTRAP=1 RUSTDOCFLAGS="${RUSTDOCFLAGS} -Z unstable-options --output-format json" \
  cargo doc -p lash-core --no-deps --all-features --locked
target_dir="$({ cargo metadata --no-deps --format-version 1; } | \
  python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
python3 scripts/check_core_public_member_docs.py "$target_dir/doc/lash_core.json"
