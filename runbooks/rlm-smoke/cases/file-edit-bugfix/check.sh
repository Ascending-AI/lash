#!/usr/bin/env bash
set -euo pipefail

workspace="$(cd "${1:?workspace path required}" && pwd)"
scenario_dir="$(cd "${2:?scenario path required}" && pwd)"

cd "$workspace"

sh test.sh
grep -Eq 'echo[[:space:]]+[$][(][(]a[[:space:]]*[+][[:space:]]*b[)][)]' calc.sh
cmp -s test.sh "$scenario_dir/workspace/test.sh"
