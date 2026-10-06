#!/usr/bin/env bash
# Run after the lander's rebase and before its push:
#   scripts/ci/landing-gates.sh <base> <head>
# Activate only after the baseline reset.
set -euo pipefail

if [[ "$#" != 2 ]]; then
  echo "usage: $0 <base> <head>" >&2
  exit 2
fi

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo"
base="$(git rev-parse --verify "$1^{commit}")"
head="$(git rev-parse --verify "$2^{commit}")"
if [[ "$(git rev-parse HEAD)" != "$head" ]]; then
  echo "landing-gates: candidate must be checked out before the gates" >&2
  exit 2
fi
if ! git diff --quiet HEAD --; then
  echo "landing-gates: candidate has tracked edits; commit them before the gates" >&2
  exit 2
fi

bash "$repo/scripts/ci/version-bump-gate.sh" "$head" "$base"
