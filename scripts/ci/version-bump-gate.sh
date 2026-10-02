#!/usr/bin/env bash
# The strict version-bump gate (FIG-4494) against the baseline a CI event names.
#
#   version-bump-gate.sh <head> [<base>]
#
# With a base -- a pull request's, a merge group's, or main's previous tip --
# the gate compares the two commits.
#
# A dispatch and a release name no base. Their baseline is the last release:
# the newest `v1`-or-later tag that is not on the candidate itself. Before any
# such tag exists the candidate is the first release, so it must be exactly the
# release baseline (scripts/release_baseline.py check), and every guard
# must still evaluate on it.
#
# The caller fetches the commits and tags this reads; nothing here guesses a
# baseline it cannot resolve, and there is no report-only exit.
set -euo pipefail

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  echo "usage: $0 <head> [<base>]" >&2
  exit 2
fi

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
head="$(git -C "$repo" rev-parse --verify "$1^{commit}")"
base=""
if [ "$#" -eq 2 ]; then
  base="$(git -C "$repo" rev-parse --verify "$2^{commit}")"
fi

if [ -z "$base" ]; then
  while IFS= read -r tag; do
    tagged="$(git -C "$repo" rev-parse --verify "refs/tags/${tag}^{commit}")"
    if [ "$tagged" != "$head" ]; then
      base="$tagged"
      echo "version-bump gate: baseline is release ${tag} (${tagged})"
      break
    fi
  done < <(git -C "$repo" tag --list 'v[1-9]*' --sort=-v:refname)
fi

if [ -z "$base" ]; then
  echo "version-bump gate: no release precedes ${head}; checking the release baseline"
  python3 "$repo/scripts/release_baseline.py" --repo "$repo" check
  base="$head"
fi

python3 "$repo/scripts/check_version_bumps.py" --repo "$repo" --base "$base" --head "$head"
