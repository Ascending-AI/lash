use super::*;
use lash_core::SessionCommitStore;
use lash_core::runtime::{
    DeliveryPolicy, QueuedWorkBatchDraft, QueuedWorkPayload, RuntimeSessionState,
};

fn test_state(session_id: &str) -> RuntimeSessionState {
    RuntimeSessionState {
        session_id: session_id.to_string(),
        turn_index: 1,
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    }
}

fn state_with_one_pending_node(session_id: &str) -> RuntimeSessionState {
    let mut state = test_state(session_id);
    state.ensure_agent_frame_initialized();
    state
}

#[tokio::test]
async fn perf_factory_reopens_created_root_session_by_id() {
    let factory = RuntimePerfStoreFactory::new(Arc::new(RuntimePerfStore::default()));
    let request = SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: "runtime-perf-turn_cancel_round_trip".to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };

    factory
        .create_store(&request)
        .await
        .expect("create benchmark root session store");

    assert!(
        factory
            .open_existing_store_by_id(&request.session_id)
            .await
            .expect("reopen benchmark root session store")
            .is_some(),
        "turn cancellation must resolve the benchmark session through the decorated factory"
    );
    assert!(
        factory
            .open_existing_store_by_id("runtime-perf-never-created")
            .await
            .expect("look up an unknown benchmark session")
            .is_none(),
        "the perf decorator must not alias an unknown id to its root store"
    );
}

#[tokio::test]
async fn successful_commits_are_counted_after_the_inner_store_accepts_them() {
    let store = RuntimePerfStore::default();
    let commit =
        RuntimeCommit::persisted_state_for_test(&state_with_one_pending_node("counted"), &[]);
    let expected_node_count = commit.graph.nodes.len();
    assert!(expected_node_count > 0, "fixture must commit graph nodes");

    SessionCommitStore::commit_runtime_state(&store, commit)
        .await
        .expect("in-memory commit succeeds");

    assert_eq!(store.graph_node_count(), expected_node_count);
}

#[tokio::test]
async fn rejected_commits_do_not_change_the_instrumentation_counter() {
    let store = RuntimePerfStore::default();
    let mut commit = RuntimeCommit::persisted_state_for_test(&test_state("root"), &[]);
    commit.enqueued_queue_batches = vec![QueuedWorkBatchDraft::new(
        "other-session",
        DeliveryPolicy::AfterCurrentTurnCommit,
        vec![QueuedWorkPayload::agent_frame_task(
            lash_core::facade_support::frame_node_id("other-session", "follow-frame"),
            "follow-on task",
            None,
        )],
    )];

    let error = SessionCommitStore::commit_runtime_state(&store, commit)
        .await
        .expect_err("cross-session queue batch must reject the commit");

    assert!(matches!(error, StoreError::SessionBindingMismatch { .. }));
    assert_eq!(store.graph_node_count(), 0);
}
