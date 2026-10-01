#!/usr/bin/env bash
# Build every test binary the named store suites execute, in one remote build,
# before any service starts.
#
# Buck2 links a test binary only when a test asks for it. Left to the suites,
# that link runs inside `store-tests.sh`'s test invocation, whose `--jobs` is
# the PostgreSQL slot count: compiles queue behind the running tests, four
# actions at a time, and a slot sits idle until its binary arrives. This build
# runs at the driver's default jobs and materializes the binaries, so the
# suites that follow only execute tests.
#
# The suites own their labels (`store-tests.sh --labels`), so a binary cannot
# join a suite without joining this build.
#
# Usage: scripts/ci/store-build.sh <suite>...
set -euo pipefail

if (($# == 0)); then
  echo "usage: scripts/ci/store-build.sh <suite>..." >&2
  exit 2
fi

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${repo_root}"

targets=()
for suite in "$@"; do
  listed="$(bash scripts/ci/store-tests.sh --labels "$suite")"
  mapfile -t -O "${#targets[@]}" targets <<<"$listed"
done
listed="$(printf '%s\n' "${targets[@]}" | LC_ALL=C sort -u)"
mapfile -t targets <<<"$listed"

report_base="${RUNNER_TEMP:-.buck2}/store-test-results"
mkdir -p "$report_base"
report_root="$(mktemp -d "$report_base/build.XXXXXX")"
"${HERMETIC_BUILD:-scripts/hermetic-build.sh}" build \
  --materializations final \
  --build-report "$report_root/build.json" \
  --event-log "$report_root/events.json-lines" \
  "${targets[@]}"
