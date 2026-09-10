use lash_core::runtime::InMemorySessionStore as RecordingStore;
use lash_sansio::SessionId;
use std::sync::Arc;
async fn recording_store_satisfies_runtime_persistence_conformance(
    law: crate::conformance::RuntimePersistenceLaw,
) {
    let clock = Arc::new(crate::testing::TestClock::new(10_000));
    let store_clock = Arc::clone(&clock);
    crate::conformance::runtime_persistence(
        move |session_id| {
            let store = RecordingStore::with_clock(store_clock.clone());
            store.bind_session_for_conformance(&SessionId::from(session_id));
            std::sync::Arc::new(store) as std::sync::Arc<dyn crate::RuntimePersistence>
        },
        crate::conformance::RuntimePersistenceLeaseTiming::controlled({
            let clock = Arc::clone(&clock);
            move |duration_ms| clock.advance(duration_ms)
        }),
        law,
    )
    .await;
}

#[tokio::test]
async fn recording_store_enforces_core_lease_fence_authority() {
    let store = RecordingStore::default();
    crate::conformance::session_execution_lease_fence_authority(&store).await;
}

#[tokio::test]
async fn in_memory_append_receipt_replays_after_ancestor_superseded() {
    let store = Arc::new(RecordingStore::default());
    let mutation_store = Arc::clone(&store);
    crate::conformance::append_request_receipt_replays_after_ancestor_superseded(
        Arc::clone(&store) as Arc<dyn crate::RuntimePersistence>,
        move |leaf_node_id| async move {
            mutation_store.force_active_leaf_for_testing(leaf_node_id);
        },
    )
    .await;
}

#[tokio::test]
async fn in_memory_inactive_append_ancestor_precedes_stale_head() {
    let store = Arc::new(RecordingStore::default());
    let mutation_store = Arc::clone(&store);
    crate::conformance::inactive_append_ancestor_precedes_stale_head(
        Arc::clone(&store) as Arc<dyn crate::RuntimePersistence>,
        move |leaf_node_id| async move {
            mutation_store.force_active_leaf_for_testing(leaf_node_id);
        },
    )
    .await;
}

#[tokio::test]
async fn in_memory_tombstoned_old_leaf_is_rejected() {
    let store = Arc::new(RecordingStore::default());
    let mutation_store = Arc::clone(&store);
    crate::conformance::tombstoned_old_leaf_is_rejected(
        Arc::clone(&store) as Arc<dyn crate::RuntimePersistence>,
        move |node_id| async move {
            mutation_store.tombstone_node_for_testing(node_id);
        },
    )
    .await;
}

#[tokio::test]
async fn in_memory_append_receipt_restores_mixed_usage_envelope() {
    crate::conformance::append_receipt_mixed_usage_envelope(Arc::new(RecordingStore::default()))
        .await;
}

fn recording_runtime_persistence() -> Arc<dyn crate::RuntimePersistence> {
    Arc::new(RecordingStore::default())
}

fn controlled_recording_runtime_persistence() -> (
    Arc<dyn crate::RuntimePersistence>,
    crate::conformance::RuntimePersistenceLeaseTiming,
) {
    let clock = Arc::new(crate::testing::TestClock::new(10_000));
    let store =
        Arc::new(RecordingStore::with_clock(clock.clone())) as Arc<dyn crate::RuntimePersistence>;
    let timing =
        crate::conformance::RuntimePersistenceLeaseTiming::controlled(move |duration_ms| {
            clock.advance(duration_ms)
        });
    (store, timing)
}

#[tokio::test]
async fn queued_work_claims_supersede_across_session_lease_generations() {
    let (store, timing) = controlled_recording_runtime_persistence();
    crate::conformance::queued_work_claims_supersede_across_session_lease_generations(
        store, timing,
    )
    .await;
}

#[tokio::test]
async fn turn_input_claims_supersede_across_session_lease_generations() {
    let (store, timing) = controlled_recording_runtime_persistence();
    crate::conformance::turn_input_claims_supersede_across_session_lease_generations(store, timing)
        .await;
}

#[tokio::test]
async fn active_turn_input_claim_reacquires_after_unrecorded_checkpoint() {
    crate::conformance::active_turn_input_claim_reacquires_after_unrecorded_checkpoint(
        recording_runtime_persistence(),
    )
    .await;
}

#[tokio::test]
async fn same_generation_claim_scans_reach_rows_beyond_the_scan_surplus() {
    crate::conformance::same_generation_claim_scans_reach_rows_beyond_the_scan_surplus(
        recording_runtime_persistence(),
    )
    .await;
}

#[tokio::test]
async fn checkpoint_claim_probe_avoids_quiescent_write_transactions() {
    let store = Arc::new(RecordingStore::default());
    crate::conformance::checkpoint_claim_probe_transaction_counts(
        Arc::clone(&store) as Arc<dyn crate::RuntimePersistence>,
        &SessionId::from("root"),
        || store.checkpoint_claim_counts(),
    )
    .await;
}

#[tokio::test]
async fn in_memory_direct_turn_accepts_before_driving() {
    Box::pin(crate::conformance::direct_turn_accepts_before_driving(
        "in-memory",
        Arc::new(crate::InMemorySessionStore::new()) as Arc<dyn crate::RuntimePersistence>,
    ))
    .await;
}

#[tokio::test]
async fn in_memory_orphaned_direct_turn_input_is_drivable_by_another_worker() {
    Box::pin(
        crate::conformance::orphaned_direct_turn_input_is_drivable_by_another_worker(
            "in-memory",
            Arc::new(crate::InMemorySessionStore::new()) as Arc<dyn crate::RuntimePersistence>,
        ),
    )
    .await;
}

#[tokio::test]
async fn in_memory_direct_turn_acceptance_mints_no_idempotency_key() {
    Box::pin(
        crate::conformance::direct_turn_acceptance_mints_no_idempotency_key(
            "in-memory",
            Arc::new(crate::InMemorySessionStore::new()) as Arc<dyn crate::RuntimePersistence>,
        ),
    )
    .await;
}

#[tokio::test]
async fn in_memory_busy_execution_lane_refuses_direct_turn_before_acceptance() {
    Box::pin(
        crate::conformance::busy_execution_lane_refuses_direct_turn_before_acceptance(
            "in-memory",
            Arc::new(crate::InMemorySessionStore::new()) as Arc<dyn crate::RuntimePersistence>,
        ),
    )
    .await;
}

#[tokio::test]
async fn in_memory_unclaimed_turn_input_settlement_is_a_conditional_write() {
    Box::pin(
        crate::conformance::unclaimed_turn_input_settlement_is_a_conditional_write(
            "in-memory",
            Arc::new(crate::InMemorySessionStore::new()) as Arc<dyn crate::RuntimePersistence>,
        ),
    )
    .await;
}
mod runtime_laws {
    use super::*;
    crate::runtime_persistence_tests!(recording_store_satisfies_runtime_persistence_conformance);
}
