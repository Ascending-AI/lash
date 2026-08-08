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

#[tokio::test]
async fn perf_store_enforces_core_lease_fence_authority() {
    let store = RuntimePerfStore::default();
    lash_core::testing::conformance::session_execution_lease_fence_authority(&store).await;
}

#[tokio::test]
async fn perf_store_enforces_borrowed_commit_contract() {
    lash_core::testing::conformance::borrowed_session_execution_lease_commit_contract(Arc::new(
        RuntimePerfStore::default(),
    ))
    .await;
}

#[tokio::test]
async fn perf_store_pins_durable_claim_id_dialects() {
    let store = RuntimePerfStore::default();
    let session_id = "perf-claim-id-dialects";
    let owner = LeaseOwnerIdentity::opaque("perf-owner", "perf-incarnation");
    let queued = store
        .enqueue_queued_work(QueuedWorkBatchDraft::new(
            session_id,
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
            vec![QueuedWorkPayload::agent_frame_task("frame", "task", None)],
        ))
        .await
        .expect("enqueue perf queued work");
    let pending = store
        .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
            session_id,
            lash_core::TurnInputIngress::next_turn(),
            lash_core::TurnInput::text("input"),
        ))
        .await
        .expect("enqueue perf turn input");
    let lease = store
        .try_claim_session_execution_lease(session_id, &owner, 60_000)
        .await
        .expect("claim perf session lease")
        .acquired()
        .expect("perf session lease acquired");
    let queued_claim = store
        .claim_ready_queued_work(
            session_id,
            &lease.fence(),
            &owner,
            lash_core::runtime::QueuedWorkClaimBoundary::Idle,
            1,
        )
        .await
        .expect("claim perf queued work")
        .expect("perf queued claim");
    let input_claim = store
        .claim_next_turn_inputs(session_id, &lease.fence(), &owner, 1)
        .await
        .expect("claim perf turn input")
        .expect("perf turn-input claim");

    assert_eq!(
        queued_claim.claim_id,
        format!("perf-qwc:{}:1", queued.enqueue_seq)
    );
    assert_eq!(
        input_claim.claim_id,
        format!("perf-tic:{}:1", pending.enqueue_seq)
    );
    assert_eq!(
        store.queued_work.lock().expect("perf queued work")[0]
            .claim_id
            .as_deref(),
        Some(queued_claim.claim_id.as_str())
    );
    assert_eq!(
        store
            .pending_turn_inputs
            .lock()
            .expect("perf pending inputs")[0]
            .claim_id
            .as_deref(),
        Some(input_claim.claim_id.as_str())
    );
}
