#!/usr/bin/env bash
set -euo pipefail

# Run from a kiln fork. Defaults: 10,000 fresh scripts, 20 median samples,
# and Monty's one-second idle interval before each fresh-sandbox sample.
cd "$(dirname "$0")/.."
exec kiln run --config=optimized //crates/lash-typescript:monty_comparison__example -- "$@"
