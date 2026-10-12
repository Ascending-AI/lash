//! Process input ownership through the runtime's session services.

/// FIG-5431: cancellation owns submitted inputs, never the linked session.
#[tokio::test]
async fn process_cancel_withdraws_its_inputs_and_preserves_foreign_child_inputs() {
    let backend = crate::testing::sqlite_recording_backend().await;
    let runtime = crate::runtime::tests::helpers::runtime_with_plugins(
        &backend,
        crate::testing::test_standard_protocol_factories(),
        crate::runtime::tests::helpers::mock_provider(Vec::new()),
    )
    .await;
    let services = runtime
        .runtime_session_services()
        .expect("session services");
    let process_id = crate::ProcessId::fixture("input-owner");
    let turn_id = crate::TurnId::from("owned-turn");
    let session_id = crate::SessionId::from("linked-input-child");
    let factory = backend.session_store_factory();
    let store = crate::testing::runtime_helpers::create_session_store(
        &factory,
        &crate::SessionStoreCreateRequest {
            session_id: session_id.clone(),
            relation: crate::SessionRelation::Child {
                parent_session_id: runtime.session_id().into(),
                caused_by: Some(crate::CausalRef::Process {
                    process_id: process_id.clone(),
                }),
            },
            pending_observer_intents: Vec::new(),
            config: crate::PersistedSessionConfig::from_policy(
                &runtime.state.policy().clone(),
                crate::SessionToolAccess::ambient(),
            ),
            head: crate::SessionCreationHead::Config,
            owning_process_id: Some(process_id.clone()),
            retention: crate::Retention::UntilGc,
        },
    )
    .await
    .expect("create linked child");
    for source in [turn_id.as_str(), "foreign-host-input"] {
        store
            .store()
            .enqueue_pending_turn_input(
                crate::PendingTurnInputDraft::new(
                    session_id.clone(),
                    crate::TurnInputIngress::next_turn(),
                    crate::TurnInput::text(source),
                )
                .with_source_key(source),
            )
            .await
            .expect("mail input");
    }
    // The missing requested id covers discovery after a crash during creation.
    services
        .withdraw_process_child_inputs(None, &process_id, &turn_id)
        .await
        .expect("cancel process inputs");
    let inputs = store.list_pending_turn_inputs().await.expect("read inputs");
    assert!(
        !inputs
            .iter()
            .any(|read| read.input.source_key.as_deref() == Some(turn_id.as_str())),
        "the process input is withdrawn"
    );
    let foreign = inputs
        .iter()
        .find(|read| read.input.source_key.as_deref() == Some("foreign-host-input"))
        .expect("foreign input");
    assert!(
        !foreign.input.state.is_terminal(),
        "lineage grants no input ownership"
    );
}
