#!/usr/bin/env bash
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mkdir -p "$repo/target/loadtest-tools"
if [[ ! -x "$repo/target/loadtest-tools/helm" ]]; then
  archive="$repo/target/loadtest-tools/helm.tar.gz"
  curl -fsSL --retry 3 https://get.helm.sh/helm-v3.18.6-linux-amd64.tar.gz -o "$archive"
  printf '%s  %s\n' 3f43c0aa57243852dd542493a0f54f1396c0bc8ec7296bbb2c01e802010819ce "$archive" | sha256sum -c
  tar xzf "$archive" -C "$repo/target/loadtest-tools" --strip-components=1 linux-amd64/helm
fi
if [[ "${1:-}" == kind && ! -x "$repo/target/loadtest-tools/kind" ]]; then
  binary="$repo/target/loadtest-tools/kind"
  curl -fsSL --retry 3 https://github.com/kubernetes-sigs/kind/releases/download/v0.29.0/kind-linux-amd64 -o "$binary"
  printf '%s  %s\n' c72eda46430f065fb45c5f70e7c957cc9209402ef309294821978677c8fb3284 "$binary" | sha256sum -c
  chmod +x "$binary"
fi
