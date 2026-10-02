#!/usr/bin/env bash
# Compare the candidate to the explicit landing baseline. At the cut, the
# reset commit must land before this gate is activated.
# Argument order matches the release-cut version gate: <head> <base>.
set -euo pipefail

if [[ "$#" != 2 ]]; then
  echo "usage: $0 <head> <base>" >&2
  exit 2
fi

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
head="$(git -C "$repo" rev-parse --verify "$1^{commit}")"
base="$(git -C "$repo" rev-parse --verify "$2^{commit}")"
exec python3 "$repo/scripts/check_version_bumps.py" --repo "$repo" --base "$base" --head "$head"
