#!/usr/bin/env bash
# Rehearse the 1.0 baseline reset on this tree: apply the reset, build the
# reset tree, and check it against the release baseline. Nothing compiles the
# reset tree until this runs, so run it on origin/main on a schedule and burn
# each red down on main by turning the literal it found into its owner's
# constant or its generator.
#
# Run it in a disposable Kiln fork cut from origin/main, through that fork's
# private PostgreSQL gate. It rewrites the working tree and never commits;
# remove the fork afterwards.
#
#   fork="$(kiln fork lash cut-rehearsal)"
#   kiln gate lash cut-rehearsal -- env BAZEL_TRUSTED=true \
#       bash scripts/ci/with-service.sh pg16 -- scripts/release-rehearsal.sh
#   kiln rm lash cut-rehearsal
#
# Arguments are the labels to build; the default is the whole workspace.
set -euo pipefail

cd "$(dirname "$0")/.."
if [ -n "$(git status --porcelain)" ]; then
  echo "release rehearsal: the tree has uncommitted changes; use a fresh fork" >&2
  exit 2
fi
echo "release rehearsal: resetting $(git rev-parse HEAD)" >&2
python3 scripts/release_reset.py --apply
if [ "$#" -eq 0 ]; then
  set -- //...
fi
kiln build "$@"
python3 scripts/release_baseline.py check
echo "release rehearsal: the reset tree of $(git rev-parse HEAD) builds and is at the release baseline" >&2
