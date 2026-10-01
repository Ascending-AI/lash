#!/usr/bin/env bash
set -euo pipefail

if (($# < 2)); then
  echo "usage: scripts/ci/buck2-test.sh <report-stem> <labels/options...>" >&2
  exit 2
fi

stem="$1"
shift
report_dir="${RUNNER_TEMP:-.buck2/ci-reports}"
mkdir -p "$report_dir"
build_report="$report_dir/${stem}-build-report.json"
test_report="$report_dir/${stem}-test-report.json"
test_output="$report_dir/${stem}-test-results"
event_log="$report_dir/${stem}-events.json-lines"

set +e
scripts/hermetic-build.sh test \
  --jobs "${BUCK2_JOBS:-32}" \
  --build-report "$build_report" \
  --test-report "$test_report" \
  --test-output-dir "$test_output" \
  --test_output errors \
  --event-log "$event_log" \
  "$@"
status=$?
set -e

if ((status != 0)); then
  summary=()
  if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    summary=(--summary "$GITHUB_STEP_SUMMARY")
  fi
  python3 scripts/ci/report_failed_tests.py \
    --report "$test_report" \
    --stage "$report_dir/failed-testlogs" \
    "${summary[@]}" || true
fi
exit "$status"
