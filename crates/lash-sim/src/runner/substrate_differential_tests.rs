//! The substrate differential: one generated workload, driven serialized on the
//! SQLite memory backend and on a SQLite file backend, reaches the same
//! abstract durable state on both, and the file run keeps its committed
//! content intact when read back cold. Neither side is the oracle; the two
//! substrates are held to each other.

use super::*;

#[tokio::test]
async fn divergent_seed_substrates_agree_on_durable_state() {
    // Regression guard for full-random seed 14123330213291275571, whose durable
    // cross-backend re-run previously hung (a `next_turn` queued ingress ran an
    // unmodeled native turn under serialized execution) and then diverged (a
    // slow async store let later boundaries overtake a live turn's completion,
    // drifting the seeded delivery order). The memory and file runs share the
    // serialize-provider-turn discipline and differ only in the substrate, so
    // their abstract durable-state summaries must be byte-identical.
    let seed = 14_123_330_213_291_275_571u64;
    let workload = generate_workload(seed, "full-random", 384).expect("workload");
    let memory = replay_workload_serialized_on_memory(&workload)
        .await
        .expect("serialized SQLite memory run");
    let tmp = tempfile::tempdir().expect("tempdir");
    let DurableRerun {
        summary: file_summary,
        content,
    } = replay_workload_on_sqlite(&workload, &tmp.path().join("sqlite-store"))
        .await
        .expect("SQLite file run");
    assert!(content.is_passed(), "{}", content.message);
    assert!(
        replay_determinism(&memory, &file_summary).is_passed(),
        "the substrates' durable state diverged for seed {seed}: memory={memory:#?} file={file_summary:#?}"
    );
    println!(
        "OK seed={seed} sessions={} digest={}",
        memory.session_count, memory.digest
    );
}

#[tokio::test]
async fn absolute_fence_drift_seed_substrates_agree() {
    // Regression for weekly-full seed 14526660659617982248. One substrate consumed
    // one fewer opaque fencing token for two workers while ownership transitions,
    // stale-writer rejection, and all user-visible durable state matched.
    let seed = 14_526_660_659_617_982_248u64;
    let workload = generate_workload(seed, "full-random", 384).expect("workload");
    let memory = replay_workload_serialized_on_memory(&workload)
        .await
        .expect("serialized SQLite memory run");
    let tmp = tempfile::tempdir().expect("tempdir");
    let DurableRerun {
        summary: file_summary,
        content,
    } = replay_workload_on_sqlite(&workload, &tmp.path().join("sqlite-store"))
        .await
        .expect("SQLite file run");
    assert!(content.is_passed(), "{}", content.message);
    assert!(
        replay_determinism(&memory, &file_summary).is_passed(),
        "the substrates' durable state diverged for seed {seed}: memory={memory:#?} file={file_summary:#?}"
    );
}
