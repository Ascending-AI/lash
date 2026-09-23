#!/usr/bin/env bash
# The `--run_under` prefix of every Lash test action (see `.bazelrc`).
#
# Bazel 9.1 runs a second remote spawn, `generate-xml.sh`, after every test
# that did not write `$XML_OUTPUT_FILE`, and that spawn queues for the test's
# whole run request to do 0.1 s of work. This runs the test unchanged -- same
# argv, stdin, cwd and environment, output still streamed to test.log -- keeps
# a copy of its output, and writes the report itself unless the test already
# did (`test_batch_runner.sh` writes one suite per member). The exit code is
# the test's own.
#
# Signals: test-setup.sh forwards a timeout or interrupt to this whole process
# group, so the test receives it directly. This script and the output copy
# outlive it just long enough to record what the test printed.
set -uo pipefail

xml=${XML_OUTPUT_FILE:?the Bazel test runner sets XML_OUTPUT_FILE}
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
