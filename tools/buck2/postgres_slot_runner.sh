#!/usr/bin/env bash
set -euo pipefail

slot_dir=${LASH_POSTGRES_SLOT_DIR:?the slot lock directory is not set}
slot_count=${LASH_POSTGRES_SLOT_COUNT:?the slot count is not set}
url=${LASH_POSTGRES_DATABASE_URL:?the database URL is not set}
here=${BASH_SOURCE[0]%/*}

# Slots bound actions (including shards), not the laws inside each action.
# Four slots x 32 libtest threads x ~7 live connections per cell already
# exceed the shared server's 400 sessions; reopen laws have several pools.
# Run one law per slot and at most four cells inside a matrix law: with
# four slots, 4 x 1 x 4 x ~7 = 112 live sessions, leaving capacity for
# reopen pools and fixture maintenance. Both matrix harnesses read this
# cell bound. An argv override must not defeat libtest admission.
thread_value=false
for argument in "$@"; do
    if $thread_value; then
        if [[ $argument != 1 ]]; then
            echo "one libtest thread per PostgreSQL slot is required" >&2
            exit 2
        fi
        thread_value=false
    elif [[ $argument == --test-threads ]]; then
        thread_value=true
    elif [[ $argument == --test-threads=* && $argument != --test-threads=1 ]]; then
        echo "one libtest thread per PostgreSQL slot is required" >&2
        exit 2
    fi
done
export RUST_TEST_THREADS=1
export LASH_MATRIX_THREADS=4

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
