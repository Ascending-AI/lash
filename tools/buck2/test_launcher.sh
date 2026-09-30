#!/usr/bin/env bash
set -euo pipefail

here=${BASH_SOURCE[0]%/*}
watchdog=(/usr/bin/python3 "$here/test_timeout.py")
prefix_count=${LASH_TEST_EXECUTION_PREFIX_ARG_COUNT:-0}
unset LASH_TEST_EXECUTION_PREFIX_ARG_COUNT
if [[ ! $prefix_count =~ ^[0-9]+$ ]] || ((prefix_count >= $#)); then
    echo "invalid test execution prefix argument count: $prefix_count" >&2
    exit 2
fi
execution=("$@")
shift "$prefix_count"
selection=("$@")
if ((prefix_count == 0)); then
    marker=-1
    for index in "${!execution[@]}"; do
        if [[ ${execution[index]} == --lash-libtest-args ]]; then
            marker=$index
            break
        fi
    done
    if ((marker >= 0)); then
        execution=("${execution[@]:0:marker}" "${execution[@]:marker+1}")
    fi
fi
runner_args=(
    --selection-argv-count "${#selection[@]}" "${selection[@]}"
    "${watchdog[@]}" "${execution[@]}")
if [[ -n ${LASH_POSTGRES_SLOT_DIR:-} ]]; then
    exec /usr/bin/bash "$here/postgres_slot_runner.sh" "${runner_args[@]}"
fi
exec /usr/bin/bash "$here/test_xml_runner.sh" "${runner_args[@]}"
