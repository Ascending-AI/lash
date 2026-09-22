#!/usr/bin/env bash
# Batch runner for lash_test_batch (FIG-3365). The manifest lists one
# runfiles-relative path per member test binary; each runs in parallel with
# its output captured to its own log, and the action fails iff any member
# fails, printing every failing log in full.
#
# Bazel test actions execute with the runfiles root as cwd, which is the cwd
# each member rust_test would have had alone. INSTA_WORKSPACE_ROOT matches the
# `_test_env` every lash_rust_*_test macro applies.
set -uo pipefail

export INSTA_WORKSPACE_ROOT=.
cd "${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"

logs="${TEST_TMPDIR:-$(mktemp -d)}/batch-logs"
mkdir -p "$logs"

jobs_cap=$(( $(nproc 2>/dev/null || echo 4) ))
[ "$jobs_cap" -gt 8 ] && jobs_cap=8
[ "$jobs_cap" -lt 1 ] && jobs_cap=1

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
    local rloc="$1" name
    name="$(basename "$rloc")"
    "$TEST_SRCDIR/$rloc" "${args[@]}" >"$logs/$name.log" 2>&1
    echo "$? $rloc" >> "$logs/status"
}

while IFS= read -r rloc; do
    [ -n "$rloc" ] || continue
    while [ "$(jobs -rp | wc -l)" -ge "$jobs_cap" ]; do
        sleep 0.05
    done
    run_member "$rloc" &
    pids+=("$!")
    names+=("$rloc")
done < "${LASH_BATCH_MANIFEST:?}"

wait

total=${#names[@]}
failures=()
if [ -f "$logs/status" ]; then
    while IFS=' ' read -r code rloc; do
        [ "$code" = "0" ] || failures+=("$rloc")
    done < "$logs/status"
fi

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
            else
                grep -Eq 'running [1-9][0-9]* tests?|test result: .* ([1-9][0-9]* passed|[1-9][0-9]* ignored)' "$log" && matched=1
            fi
        done
        if ((!matched && !help_only)); then
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
