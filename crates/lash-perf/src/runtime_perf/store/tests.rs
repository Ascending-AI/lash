use super::*;
use lash_core::runtime::{DeliveryPolicy, QueuedWorkPayload, RuntimeSessionState, SlotPolicy};

#[tokio::test]
async fn runtime_commit_rejects_cross_session_queue_batches_atomically() {
    let store = RuntimePerfStore::default();
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        turn_index: 1,
        ..RuntimeSessionState::default()
    };
    let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    commit.enqueued_queue_batches = vec![QueuedWorkBatchDraft::new(
        "other-session",
        DeliveryPolicy::AfterCurrentTurnCommit,
        SlotPolicy::Exclusive,
        vec![QueuedWorkPayload::agent_frame_task(
            "follow-frame",
            "follow-on task",
            None,
        )],
    )];

    let error = store
        .commit_runtime_state(commit)
        .await
        .expect_err("cross-session queue batch must reject the commit");

    assert!(matches!(
        error,
        StoreError::SessionBindingMismatch {
            bound_session_id,
            attempted_session_id,
        } if bound_session_id == "root" && attempted_session_id == "other-session"
    ));
    assert!(
        store
            .load_session()
            .await
            .expect("load session after rejected commit")
            .is_none(),
        "rejected commit must not persist session state"
    );
    assert!(
        store
            .list_queued_work("other-session")
            .await
            .expect("list queued work after rejected commit")
            .is_empty(),
        "rejected commit must not enqueue cross-session work"
    );
}

/// The perf harness's store is a real [`SessionExecutionLeaseStore`], so it owes
/// the same displacement contract as the durable backends.
///
/// A double that reports no displacement silently disables
/// `session_execution_lease.taken_over` for everything running on it, and that
/// absence is invisible until an operator needs the event. This runs the shared
/// conformance vector rather than a local copy, so the perf store cannot drift
/// away from the contract the durable backends are held to.
#[tokio::test]
async fn perf_store_reports_the_holder_a_claim_displaces() {
    let store = RuntimePerfStore::default();
    lash_core::testing::conformance::session_execution_lease_displacement(
        &store,
        "perf-lease-displacement",
    )
    .await;
}
