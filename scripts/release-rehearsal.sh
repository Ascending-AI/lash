#!/usr/bin/env bash
# Perform the 1.0 cut on this tree, or rehearse it: apply the baseline reset,
# which also unmarks and runs every cut-only law, build the reset tree, check
# it against the release baseline, and hold the versions a compiled operator
# reports in both tiers to the inventory. Nothing compiles the reset tree
# until this runs, so run it on origin/main on a schedule and burn each red
# down on main by turning the literal it found into its owner's constant or
# its generator.
#
# Run it in a disposable Kiln fork cut from origin/main, through that fork's
# private PostgreSQL gate. It rewrites the working tree and never commits: a
# rehearsal removes the fork afterwards, and the cut commits the tree it
# leaves.
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
# A red cut-only law leaves a written and checked reset tree; build and
# check it anyway, so one run reports every red.
laws=0
python3 scripts/release_reset.py --apply || laws=$?
if [ "$laws" -ne 0 ] && [ "$laws" -ne 3 ]; then
  exit "$laws"
fi
if [ "$#" -eq 0 ]; then
  set -- //...
fi
kiln build "$@"
python3 scripts/release_baseline.py check
python3 scripts/release_baseline.py probe
if [ "$laws" -ne 0 ]; then
  echo "release rehearsal: the reset tree of $(git rev-parse HEAD) builds and is at the release baseline, but a cut-only law is red" >&2
  exit 1
fi
echo "release rehearsal: the reset tree of $(git rev-parse HEAD) builds, passes its cut-only laws and is at the release baseline" >&2
