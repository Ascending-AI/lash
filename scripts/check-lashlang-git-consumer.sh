#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
requested_ref="${1:-HEAD}"
source_sha="$(git -C "$repo_root" rev-parse "${requested_ref}^{commit}")"
consumer_dir="$(mktemp -d "${TMPDIR:-/tmp}/lashlang-git-consumer.XXXXXX")"

cleanup() {
  find "$consumer_dir" -depth -delete
}
trap cleanup EXIT

mkdir -p "$consumer_dir/src"
cat > "$consumer_dir/Cargo.toml" <<EOF
[package]
name = "lashlang-git-consumer"
version = "0.0.0"
edition = "2024"

[dependencies]
lashlang = { package = "lash-internal-lashlang", git = "file://${repo_root}", rev = "${source_sha}" }
EOF
cat > "$consumer_dir/src/main.rs" <<'EOF'
fn main() {}
EOF

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${repo_root}/target/lashlang-git-consumer}"
cargo check --manifest-path "$consumer_dir/Cargo.toml"
echo "lashlang Git consumer passed without a patch mirror at ${source_sha}"
