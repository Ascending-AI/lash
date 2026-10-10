#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
requested_ref="${1:-HEAD}"
source_sha="$(git -C "$repo_root" rev-parse "${requested_ref}^{commit}")"
consumer_dir="$(mktemp -d "${TMPDIR:-/tmp}/lash-vm-git-consumer.XXXXXX")"

cleanup() {
  find "$consumer_dir" -depth -delete
}
trap cleanup EXIT

mkdir -p "$consumer_dir/src"
cat > "$consumer_dir/Cargo.toml" <<EOF
[package]
name = "lash-vm-git-consumer"
version = "0.0.0"
edition = "2024"

[dependencies]
lash-kernel-vm = { git = "file://${repo_root}", rev = "${source_sha}" }
lash-kernel-lib = { git = "file://${repo_root}", rev = "${source_sha}" }
lash-dialect-typescript = { git = "file://${repo_root}", rev = "${source_sha}" }
EOF
cat > "$consumer_dir/src/main.rs" <<'EOF'
fn main() {}
EOF

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${repo_root}/target/lash-vm-git-consumer}"
cargo check --manifest-path "$consumer_dir/Cargo.toml"
echo "kernel Git consumer passed without a patch mirror at ${source_sha}"
