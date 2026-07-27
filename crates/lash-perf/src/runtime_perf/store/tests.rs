use super::*;
use lash_core::runtime::{DeliveryPolicy, QueuedWorkPayload, RuntimeSessionState, SlotPolicy};

#[tokio::test]
async fn refcount_scrub_refuses_to_report_false_success() {
    let store = RuntimePerfStore::default();

    let error = store
        .verify_node_refcounts()
        .await
        .expect_err("perf store does not maintain node refcounts");

    assert!(matches!(
        error,
        StoreError::UnsupportedStoreOperation {
            operation: "verify_node_refcounts"
        }
    ));
}

#[tokio::test]
async fn runtime_commit_rejects_cross_session_queue_batches_atomically() {
    let store = RuntimePerfStore::default();
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        session_lifetime: lash_core::SessionLifetime::durable(
            lash_core::IncarnationId::mint_for_store(),
        ),
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
