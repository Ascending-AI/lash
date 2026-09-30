#!/usr/bin/env bash
# Test launcher wrapper used by the external Buck2 runner. It preserves the
# test's argv, stdin, cwd, environment, and exit code; streams output while
# retaining a copy; and writes the declared JUnit report unless a batch runner
# already produced it. The launcher watchdog owns timeout and cancellation.
set -uo pipefail

xml=${XML_OUTPUT_FILE:?the Buck2 test runner sets XML_OUTPUT_FILE}
log=$(mktemp) || exec "$@"
trap 'rm -f "$log"' EXIT
for signal in TERM INT HUP; do
    trap : "$signal"
done

exec {copy}> >(trap '' TERM INT HUP; exec tee -- "$log")
copier=$!
started=${EPOCHREALTIME/./}
"$@" >&"$copy" 2>&1 {copy}>&-
code=$?
elapsed=$((${EPOCHREALTIME/./} - started))
exec {copy}>&-

# The copy ends when every writer of the pipe is gone. A process the test left
# behind can hold it open; test-setup.sh kills that group once this exits, so
# report what arrived within two seconds rather than wait on it.
for ((i = 0; i < 200; i++)); do
    kill -0 "$copier" 2>/dev/null || break
    sleep 0.01
done

if [[ ! -e "$xml" ]]; then
    name=${TEST_BINARY#./}
    name=${name#../}
    if ((${TEST_TOTAL_SHARDS:-0} != 0)); then
        name+="_shard_$((TEST_SHARD_INDEX + 1))/$TEST_TOTAL_SHARDS"
    fi
    seconds=$(printf '%d.%03d' $((elapsed / 1000000)) $((elapsed % 1000000 / 1000)))
    python3 "${BASH_SOURCE[0]%/*}/junit_xml.py" "$xml" "$name" "$code" "$seconds" "$log"
fi
exit "$code"
