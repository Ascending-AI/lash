use lash_core::runtime::InMemorySessionStore as RecordingStore;
use lash_sansio::SessionId;
use std::sync::Arc;

crate::append_head_switch_tests!({
    let store = Arc::new(RecordingStore::default());
    let mutation_store = Arc::clone(&store);
    (
        (),
        Arc::clone(&store) as Arc<dyn crate::RuntimePersistence>,
        move |leaf_node_id| async move {
            mutation_store.force_active_leaf_for_testing(leaf_node_id);
        },
    )
});

crate::append_tombstone_tests!({
    let store = Arc::new(RecordingStore::default());
    let mutation_store = Arc::clone(&store);
    (
        (),
        Arc::clone(&store) as Arc<dyn crate::RuntimePersistence>,
        move |node_id| async move {
            mutation_store.tombstone_node_for_testing(node_id);
        },
    )
});

crate::append_receipt_envelope_tests!({
    (
        (),
        Arc::new(RecordingStore::default()) as Arc<dyn crate::RuntimePersistence>,
    )
});

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

crate::runtime_persistence_targeted_tests!({
    let (store, timing) = controlled_recording_runtime_persistence();
    ((), store, timing)
});

crate::checkpoint_claim_probe_tests!({
    let store = Arc::new(RecordingStore::default());
    let counting_store = Arc::clone(&store);
    (
        (),
        Arc::clone(&store) as Arc<dyn crate::RuntimePersistence>,
        SessionId::from("root"),
        move || counting_store.checkpoint_claim_counts(),
        async {},
    )
});

crate::direct_turn_acceptance_tests!({
    (
        (),
        "in-memory",
        Arc::new(crate::InMemorySessionStore::new()) as Arc<dyn crate::RuntimePersistence>,
    )
});
mod runtime_laws {
    use super::*;
    crate::runtime_persistence_tests!({
        let clock = Arc::new(crate::testing::TestClock::new(10_000));
        let store_clock = Arc::clone(&clock);
        (
            (),
            move |session_id: &str| {
                let store = RecordingStore::with_clock(store_clock.clone());
                store.bind_session_for_conformance(&SessionId::from(session_id));
                Arc::new(store) as Arc<dyn crate::RuntimePersistence>
            },
            crate::conformance::RuntimePersistenceLeaseTiming::controlled(move |duration_ms| {
                clock.advance(duration_ms)
            }),
        )
    });
}

// No runtime_persistence_reopenable_tests!: an in-memory handle has no cold-reopen boundary.
// No append_receipt_identity_corruption_tests!: typed memory records expose no wire bytes to corrupt.
// No append_usage_cancellation_tests!: exactly-once cancellation requires the SQLite worker seam.
// No unbound_session_*_tests!: the in-memory store is always explicitly session-bound.
