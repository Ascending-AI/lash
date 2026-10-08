use super::*;

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn double_invalidation_preserves_first_decision_id() {
    let backend = sqlite_memory_store_backend().await;
    let mut runtime = runtime_with_plugins_and_tools(
        &backend,
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
    )
    .await;
    assert_eq!(
        *runtime.resident_session.validity(),
        ResidentSessionState::Valid
    );

    runtime.invalidate_resident_session_state();
    let initial_decision_id = match runtime.resident_session.validity() {
        ResidentSessionState::Invalidated { decision_id } => decision_id.clone(),
        ResidentSessionState::Valid => panic!("expected invalidated resident state"),
    };
    assert!(!initial_decision_id.is_empty());

    // A second invalidation while already invalidated must preserve the first decision id
    runtime.invalidate_resident_session_state();
    match runtime.resident_session.validity() {
        ResidentSessionState::Invalidated { decision_id } => {
            assert_eq!(
                decision_id, &initial_decision_id,
                "subsequent invalidation must not overwrite the initial decision identity"
            );
        }
        ResidentSessionState::Valid => panic!("expected invalidated resident state"),
    }
}

/// `unbound_recording_store`'s store-only-backend twin (D1 F3): an unbound
/// store on `backend`'s session catalog under the recording decorator, for
/// tests that run no effect.
pub(super) async fn recording_unbound_store_on(
    backend: &lash_core::Backend,
) -> Arc<RecordingStore> {
    Arc::new(RecordingStore::over(backend.session_store_factory()))
}
