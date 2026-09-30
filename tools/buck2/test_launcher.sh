#!/usr/bin/env bash
set -euo pipefail

here=${BASH_SOURCE[0]%/*}
watchdog=(/usr/bin/python3 "$here/test_timeout.py")
if [[ -n ${LASH_POSTGRES_SLOT_DIR:-} ]]; then
    exec /usr/bin/bash "$here/postgres_slot_runner.sh" "${watchdog[@]}" "$@"
fi
exec /usr/bin/bash "$here/test_xml_runner.sh" "${watchdog[@]}" "$@"
