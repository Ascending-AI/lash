//! The durable SQLite re-run agrees with the serialized in-memory reference and
//! keeps committed content intact when read back cold.

use super::*;

#[tokio::test]
async fn divergent_seed_cross_backend_durable_state_agrees() {
    // Regression guard for full-random seed 14123330213291275571, whose durable
    // cross-backend re-run previously hung (a `next_turn` queued ingress ran an
    // unmodeled native turn under serialized execution) and then diverged (a
    // slow async store let later boundaries overtake a live turn's completion,
    // drifting the seeded delivery order). The serialized in-memory reference
    // and the SQLite durable re-run share the serialize-provider-turn discipline
    // and differ only in the store, so their abstract durable-state summaries
    // must be byte-identical.
    let seed = 14_123_330_213_291_275_571u64;
    let workload = generate_workload(seed, "full-random", 384).expect("workload");
    let reference = replay_workload_serialized_reference(&workload)
        .await
        .expect("serialized in-memory reference");
    let tmp = tempfile::tempdir().expect("tempdir");
    let DurableRerun {
        summary: sqlite_summary,
        content,
    } = replay_workload_on_sqlite(&workload, &tmp.path().join("sqlite-store"))
        .await
        .expect("sqlite re-run");
    assert!(content.is_passed(), "{}", content.message);
    assert!(
        replay_determinism(&reference, &sqlite_summary).is_passed(),
        "cross-backend semantic durable state diverged for seed {seed}: reference={reference:#?} sqlite={sqlite_summary:#?}"
    );
    println!(
        "OK seed={seed} sessions={} digest={}",
        reference.session_count, reference.digest
    );
}

#[tokio::test]
async fn absolute_fence_drift_seed_cross_backend_semantics_agree() {
    // Regression for weekly-full seed 14526660659617982248. SQLite consumed one
    // fewer opaque fencing token for two workers while ownership transitions,
    // stale-writer rejection, and all user-visible durable state matched.
    let seed = 14_526_660_659_617_982_248u64;
    let workload = generate_workload(seed, "full-random", 384).expect("workload");
    let reference = replay_workload_serialized_reference(&workload)
        .await
        .expect("serialized in-memory reference");
    let tmp = tempfile::tempdir().expect("tempdir");
    let DurableRerun {
        summary: sqlite_summary,
        content,
    } = replay_workload_on_sqlite(&workload, &tmp.path().join("sqlite-store"))
        .await
        .expect("sqlite re-run");
    assert!(content.is_passed(), "{}", content.message);
    assert!(
        replay_determinism(&reference, &sqlite_summary).is_passed(),
        "cross-backend semantic durable state diverged for seed {seed}: reference={reference:#?} sqlite={sqlite_summary:#?}"
    );
}
