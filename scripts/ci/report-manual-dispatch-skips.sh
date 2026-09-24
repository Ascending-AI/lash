#!/usr/bin/env bash
set -euo pipefail

summary_path="${GITHUB_STEP_SUMMARY:?GITHUB_STEP_SUMMARY must be set}"
cat >>"$summary_path" <<'EOF'
## ⚠️ Manual CI run is incomplete

The `Check versioned surface bumps` gate is skipped for `workflow_dispatch` because a manual run has no meaningful triggering base. A green manual run does not prove that gate passed.

The `Check the Test262 outcome ratchet` gate is skipped for the same reason: it compares the Test262 outcome record with the same base.
EOF
echo "::warning title=Versioned surface bump gate skipped::workflow_dispatch skips Check versioned surface bumps; a green manual run does not prove it passed."
echo "::warning title=Test262 outcome ratchet skipped::workflow_dispatch skips Check the Test262 outcome ratchet; a green manual run does not prove it passed."
