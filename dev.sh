#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

# Load .env if present.
if [ -f .env ]; then
  set -a
  source .env
  set +a
fi

# cargo run always rebuilds if sources changed.
exec cargo run -p lash-cli --bin lash -- "$@"
