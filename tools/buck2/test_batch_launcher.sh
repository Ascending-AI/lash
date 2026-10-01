#!/usr/bin/env bash
set -euo pipefail
here=${BASH_SOURCE[0]%/*}
exec /usr/bin/python3 "$here/test_timeout.py" /usr/bin/bash "$here/test_batch_runner.sh" "$@"
