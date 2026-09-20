#!/usr/bin/env bash
# Runs the Bazel-built seal harness (//crates/lash:ui__test) directly, outside
# `bazel test`. trybuild discovers the fixture feature set from the running
# binary's Cargo fingerprint: the binary must be named `deps/<name>-<16 lowercase
# hex>` under the target directory, with exactly one matching
# `.fingerprint/*-<hash>/*.json` file carrying the enabled feature list
# (trybuild 1.0.118 `src/features.rs`). This script stages that layout under
# CARGO_TARGET_DIR using the fingerprint the generator emitted from the same
# `crate_features` the harness was compiled with, then execs the binary from
# the crate manifest directory, which is where trybuild resolves the fixture
# paths (`tests/ui/*.rs`) and the workspace root.
set -euo pipefail

cd "$(dirname "$0")/../.."

# Any 16 lowercase-hex suffix satisfies trybuild's parser; it only needs to
# join the binary name to the fingerprint directory we stage here.
hash=5ea1feed00000001
root="${CARGO_TARGET_DIR:-$PWD/target-seal}/debug"

mkdir -p "$root/deps" "$root/.fingerprint/ui-$hash"
cp bazel-bin/crates/lash/ui__test "$root/deps/ui-$hash"
cp bazel-bin/crates/lash/ui__test.cargo_fingerprint.json \
    "$root/.fingerprint/ui-$hash/test-ui.json"

cd crates/lash
CARGO_MANIFEST_DIR="$PWD" exec "$root/deps/ui-$hash"
