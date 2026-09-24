#!/usr/bin/env bash
# The `--run_under` prefix of `scripts/ci/store-tests.sh pg-store` (FIG-3572).
#
# The PostgreSQL store suites share a database per process: `reset()`
# truncates every lash_* table, and the suites serialize on an in-process
# guard. Run in parallel -- the conformance and integration binaries are
# sharded -- two test actions on one database would truncate each other's
# rows. `scripts/ci/with-service.sh` creates LASH_POSTGRES_SLOT_COUNT
# databases, `lash_slot_0` onwards, beside the default one. This takes the
# first free slot under an exclusive lock in LASH_POSTGRES_SLOT_DIR, points
# LASH_POSTGRES_DATABASE_URL at that slot's database, and holds the lock for
# the life of the test: the lock descriptor survives `exec`, so it is
# released when the test and everything it spawned have exited.
# `store-tests.sh` runs at most LASH_POSTGRES_SLOT_COUNT tests at once, so a
# slot is always free by the time a test starts; the wait below only covers
# the moment between one test's exit and the next one's start.
#
# A sharded test also records, in its undeclared outputs, the binary's whole
# `--list` and this shard's share of it. `scripts/ci/check_test_shard_coverage.py`
# proves from those files that the shards of each label partition the list.
set -euo pipefail

# The store job forwards all three with `--test_env`.
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

# postgres://user:password@host:port/database[?query]: replace the database.
base=${url%%\?*}
query=${url#"$base"}
export LASH_POSTGRES_DATABASE_URL="${base%/*}/lash_slot_${slot}${query}"

if ((${TEST_TOTAL_SHARDS:-0} != 0)); then
    coverage="${TEST_UNDECLARED_OUTPUTS_DIR:?Bazel sets TEST_UNDECLARED_OUTPUTS_DIR}/shard-coverage"
    mkdir -p "$coverage"
    # Unsharded, `rust_test`'s sharding wrapper runs the binary directly; with
    # this action's shard variables it lists exactly the cases it will run.
    env -u TEST_TOTAL_SHARDS -u TEST_SHARD_INDEX \
        -u RULES_RUST_TEST_TOTAL_SHARDS -u RULES_RUST_TEST_SHARD_INDEX \
        "$@" --list --format terse >"${coverage}/all.txt"
    "$@" --list --format terse >"${coverage}/shard-${TEST_SHARD_INDEX}-of-${TEST_TOTAL_SHARDS}.txt"
fi

exec "${here}/test_xml_runner.sh" "$@"
