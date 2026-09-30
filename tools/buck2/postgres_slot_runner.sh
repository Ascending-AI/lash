#!/usr/bin/env bash
set -euo pipefail

slot_dir=${LASH_POSTGRES_SLOT_DIR:?the slot lock directory is not set}
slot_count=${LASH_POSTGRES_SLOT_COUNT:?the slot count is not set}
url=${LASH_POSTGRES_DATABASE_URL:?the database URL is not set}
here=${BASH_SOURCE[0]%/*}

slot=
until [[ -n "$slot" ]]; do
    for ((index = 0; index < slot_count; index++)); do
        exec {lock}>"${slot_dir}/slot-${index}.lock"
        if flock --nonblock "$lock"; then
            slot=$index
            break
        fi
        exec {lock}>&-
    done
    [[ -n "$slot" ]] || sleep 0.2
done

base=${url%%\?*}
query=${url#"$base"}
export LASH_POSTGRES_DATABASE_URL="${base%/*}/lash_slot_${slot}${query}"
exec /usr/bin/bash "${here}/test_xml_runner.sh" "$@"
