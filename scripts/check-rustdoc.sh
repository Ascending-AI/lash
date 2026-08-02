#!/usr/bin/env bash
# Fail on any rustdoc diagnostic in the two host-facing crates.
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
