#!/usr/bin/env bash
# Recorded kernel baseline versus current kernel; strict rows, advisory timing.
set -euo pipefail
exec python3 "$(dirname "$0")/measure.py" "$@"
