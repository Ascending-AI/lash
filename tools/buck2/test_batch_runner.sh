#!/usr/bin/env bash
# Batch runner for lash_batch_test (FIG-3365). The manifest lists one
# runfiles-relative path per member test binary; at most LASH_BATCH_JOBS run
# at once, each with its output captured to its own log, and the action fails
# iff any member fails, printing every failing log in full. The batch writes
# its own JUnit report, one suite per member (`junit_xml.py`).
#
# The external runner starts in the project root, which is also the cwd each
# member Rust test has when run alone.
set -uo pipefail

export BUILD_WORKSPACE_DIRECTORY=.
export INSTA_WORKSPACE_ROOT=.
junit_xml="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/junit_xml.py"
libtest_selection="${junit_xml%/*}/libtest_selection.py"
xml=${XML_OUTPUT_FILE:?the test runner sets XML_OUTPUT_FILE}

logs="${TEST_TMPDIR:-$(mktemp -d)}/batch-logs"
mkdir -p "$logs"

# The batch rule reserves LASH_BATCH_JOBS member-sized slots for this action;
# the worker's own core count says nothing about that reservation.
jobs_cap=${LASH_BATCH_JOBS:?LASH_BATCH_JOBS is set by lash_batch_test}
if ! [[ "$jobs_cap" =~ ^[1-9][0-9]*$ ]]; then
    echo "FAIL: LASH_BATCH_JOBS must be a positive integer, got '$jobs_cap'" >&2
    exit 1
fi

member_count=${1:?batch launcher supplies the member count}
shift
if ! [[ "$member_count" =~ ^[1-9][0-9]*$ ]] || ((member_count > $#)); then
    echo "FAIL: invalid batch member count '$member_count' for $# arguments" >&2
    exit 1
fi
# Each member arrives as `<n> <n NAME=value assignments> <binary>`: the
# environment its own Rust test target declares, then the binary.
members=()
member_envs=()
for ((member = 0; member < member_count; member++)); do
    env_count=${1:-}
    if ! [[ "$env_count" =~ ^[0-9]+$ ]] || ((env_count + 2 > $#)); then
        echo "FAIL: invalid environment count '$env_count' for batch member $member" >&2
        exit 1
    fi
    shift
    assignments=("${@:1:env_count}")
    shift "$env_count"
    member_envs+=("$(printf '%s\n' "${assignments[@]}")")
    members+=("$1")
    shift
done
args=("$@")
list_only=0
help_only=0
for arg in "${args[@]}"; do
    [[ "$arg" == "--list" ]] && list_only=1
    [[ "$arg" == "--help" || "$arg" == "-h" ]] && help_only=1
done

declare -a pids=()
declare -a names=()
run_member() {
    local rloc="$1" environment="$2" name started code elapsed
    local -a assignments=()
    [[ -n $environment ]] && mapfile -t assignments <<<"$environment"
    name="$(basename "$rloc")"
    started=${EPOCHREALTIME/./}
    /usr/bin/env "${assignments[@]}" "$rloc" "${args[@]}" >"$logs/$name.log" 2>&1
    code=$?
    elapsed=$((${EPOCHREALTIME/./} - started))
    printf '%d %d.%03d %s\n' "$code" $((elapsed / 1000000)) \
        $((elapsed % 1000000 / 1000)) "$rloc" >> "$logs/status"
}

for index in "${!members[@]}"; do
    rloc=${members[index]}
    while [ "$(jobs -rp | wc -l)" -ge "$jobs_cap" ]; do
        sleep 0.05
    done
    run_member "$rloc" "${member_envs[index]}" &
    pids+=("$!")
    names+=("$rloc")
done

wait

total=${#names[@]}
failures=()
if [ -f "$logs/status" ]; then
    while IFS=' ' read -r code _ rloc; do
        [ "$code" = "0" ] || failures+=("$rloc")
    done < "$logs/status"
fi

# One suite per member, in manifest order. A member with no status line never
# reported an exit code.
report=()
for rloc in "${names[@]}"; do
    code="?" seconds=0
    if [ -f "$logs/status" ]; then
        while IFS=' ' read -r c s r; do
            [ "$r" = "$rloc" ] && code=$c seconds=$s
        done < "$logs/status"
    fi
    report+=("${rloc#_main/}" "$code" "$seconds" "$logs/$(basename "$rloc").log")
done
python3 "$junit_xml" "$xml" "${report[@]}"

# A member that never wrote a status line died before its exit was recorded
# (kill -9, harness abort). Count it as failed.
ran=$(sort -u "$logs/status" 2>/dev/null | wc -l)
if [ "$ran" -lt "$total" ]; then
    for rloc in "${names[@]}"; do
        grep -q " $rloc\$" "$logs/status" 2>/dev/null || failures+=("$rloc (no exit recorded)")
    done
fi

if [ "${#failures[@]}" -eq 0 ]; then
    # Explicit libtest arguments request observable output, especially --list
    # and --nocapture. Print after joining to avoid interleaved member output.
    if [ "${#args[@]}" -gt 0 ]; then
        matched=0
        for rloc in "${names[@]}"; do
            log="$logs/$(basename "$rloc").log"
            cat "$log"
            if ((list_only)); then
                grep -Eq ': (test|benchmark)$' "$log" && matched=1
            fi
        done
        if ((!list_only && !help_only)); then
            python3 "$libtest_selection" batch-members "$xml" \
                "$member_count" "${members[@]}" "${args[@]}" || {
                echo "FAIL: no tests matched the batch arguments with non-ignored execution" >&2
                exit 1
            }
        elif ((!matched && !help_only)); then
            echo "FAIL: no tests matched the batch arguments" >&2
            exit 1
        fi
    fi
    echo "PASS: $total test binaries in batch"
    exit 0
fi

echo "FAIL: ${#failures[@]} of $total test binaries failed:" >&2
for rloc in "${failures[@]}"; do
    echo "=== $rloc ===" >&2
    log="$logs/$(basename "${rloc%% *}").log"
    cat "$log" >&2 2>/dev/null || echo "(no output captured)" >&2
done
exit 1
