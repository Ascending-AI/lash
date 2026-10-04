use super::*;
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn runtime_plugin_state_park_law(store: Arc<dyn RuntimeStore>) {
    let id = "plugin-state-lifecycle";
    let fixture = MockPlugin {
        writes_on_ready: true,
        ..Default::default()
    };
    let policy = crate::SessionPolicy {
        model: Some(crate::testing::test_llm_profile_config(
            "plugin-state-model",
            crate::LlmProfileMetadata::builder("plugin-state-model")
                .context_window_tokens(4096)
                .build()
                .unwrap(),
        )),
        ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024))
    };
    let mut state = RuntimeSessionState {
        session_id: id.into(),
        ..RuntimeSessionState::new(policy.clone())
    };
    let plugins = support::construct(&fixture.host(), id, None, Default::default()).await;
    state.capture_plugin_states(&plugins).unwrap();
    let hook_session = plugins.clone();
    let runtime_host = crate::EmbeddedRuntimeHost::new(crate::StoreLawBackend::new().host_config(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    ));
    let runtime_services = crate::PersistentRuntimeServices::new(
        plugins,
        crate::conformance::helpers::session_view(&store, id),
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let runtime = crate::LashRuntime::from_persistent_embedded_state(
        policy.clone(),
        runtime_host,
        runtime_services,
        state,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .unwrap();
    let hook_error = Box::pin(hook_session.dispatch(None).before_turn_decisions(
        crate::plugin::TurnHookContext {
            session_id: id.into(),
            state: runtime.read_view(),
            sessions: runtime.session_read_service().unwrap(),
            turn_context: crate::TurnContext::default(),
            plugin_config: Default::default(),
        },
    ))
    .await
    .expect_err("the hook deliberately fails");
    assert!(hook_error.to_string().contains("deliberate hook failure"));
    support::publish(
        &hook_session,
        MOCK,
        "published-write",
        StateCommands::new().set("counter", serde_json::json!(11)),
    )
    .await;
    Box::pin(runtime.park()).await.unwrap();
    let state = crate::conformance::helpers::load_window_state(&store, &SessionId::from(id))
        .await
        .unwrap()
        .unwrap();
    let durable = state.plugin_state().unwrap();
    assert_eq!(
        durable.plugins[MOCK].values["counter"],
        serde_json::json!(11)
    );
    assert_eq!(
        durable.plugins[MOCK].values["ready"],
        serde_json::json!(true)
    );
    assert_eq!(
        durable.plugins[MOCK].values.len(),
        2,
        "a failed hook publishes nothing"
    );
    let generation = durable.plugins[MOCK].generation;
    let rebuilt = MockPlugin {
        writes_on_ready: true,
        ..Default::default()
    };
    let plugins = support::construct(
        &rebuilt.host(),
        id,
        Some(durable),
        SessionAuthorityContext {
            plugin_config: state.admitted_plugin_config(),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(rebuilt.state(id).generation(), generation);
    let runtime_host = crate::EmbeddedRuntimeHost::new(crate::StoreLawBackend::new().host_config(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    ));
    let runtime_services = crate::PersistentRuntimeServices::new(
        plugins,
        crate::conformance::helpers::session_view(&store, id),
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let runtime = crate::LashRuntime::from_persistent_embedded_state(
        policy,
        runtime_host,
        runtime_services,
        state,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .unwrap();
    assert_eq!(
        rebuilt.state(id).generation(),
        generation,
        "runtime assembly must preserve initialized state"
    );
    Box::pin(runtime.park()).await.unwrap();
    let final_state = crate::conformance::helpers::load_window_state(&store, &SessionId::from(id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        final_state.plugin_state().unwrap().plugins[MOCK].generation,
        generation,
        "an otherwise idle park preserves the recorded initialization"
    );
}
