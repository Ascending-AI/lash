#!/usr/bin/env bash
# Run after the lander's rebase and before its push:
#   scripts/ci/landing-gates.sh <base> <head>
# Activate only after the baseline reset. The cut supplies the tagged corpus
# at the default path; LASH_REPLAY_CORPUS_ROOT selects a rehearsal corpus.
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
  echo "landing-gates: candidate must be checked out before replay" >&2
  exit 2
fi
if ! git diff --quiet HEAD --; then
  echo "landing-gates: candidate has tracked edits; commit them before replay" >&2
  exit 2
fi

corpus="${LASH_REPLAY_CORPUS_ROOT:-$repo/fixtures/release/v1.0.0/replay-corpus}"
if [[ ! -d "$corpus" ]]; then
  echo "landing-gates: replay corpus does not exist: $corpus" >&2
  exit 2
fi
corpus="$(cd "$corpus" && pwd)"

bash "$repo/scripts/ci/version-bump-gate.sh" "$head" "$base"

mkdir -p "$repo/.buck2/landing-gates"
evidence="$(mktemp -d "$repo/.buck2/landing-gates/run-XXXXXXXX")"
law=tests::replay_corpus::replay_corpus_fixtures_match_current_controller
kiln test //crates/lash-restate:lash-restate__unit_test \
  --local-test-execution --no-test-cache \
  --test_arg="$law" --test_arg=--exact --test_arg=--nocapture \
  --test_env "LASH_REPLAY_CORPUS_ROOT=$corpus" \
  --test-report "$evidence/test-report.json" \
  --test-output-dir "$evidence/results"

# A renamed or omitted law must not turn an empty test selection green.
python3 - "$evidence/test-report.json" "$law" <<'PY'
import json
from pathlib import Path
import sys
import xml.etree.ElementTree as ET

report = json.loads(Path(sys.argv[1]).read_text())
results = list(report["results"].values())
if not report["session_complete"] or report["infrastructure_errors"] or len(results) != 1:
    raise SystemExit("landing-gates: replay execution report is incomplete")
result = results[0]
cases = list(ET.parse(result["outputs"]["junit_xml"]).iter("testcase"))
if result["status"] != "PASS" or len(cases) != 1 or cases[0].get("name") != sys.argv[2]:
    raise SystemExit("landing-gates: the replay law did not execute exactly once")
if any(cases[0].find(kind) is not None for kind in ("failure", "error", "skipped")):
    raise SystemExit("landing-gates: replay law did not pass")
print(f"landing-gates: 1 replay law passed; evidence {sys.argv[1]}")
PY
