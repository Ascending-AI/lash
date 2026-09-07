#!/usr/bin/env bash
set -euo pipefail

echo "Cache filesystem capacity:"
df -h "${RUNNER_TEMP}"
