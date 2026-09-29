use lash_sansio::SessionId;
use std::sync::Arc;

use lash_core::{
    DeploymentStore, Message, MessageRole, ModelSpec, Part, RuntimeCommit, RuntimeSessionState,
    SessionPolicy, TokenUsage, facade_support::LashRuntime,
};
use lash_sqlite_store::SqliteStoreSet;

#[expect(
    clippy::expect_used,
    reason = "test support: a fixed, always-valid model spec; any builder refusal is a broken test fixture"
)]
fn test_model_spec() -> ModelSpec {
    ModelSpec::builder("gpt-5.4-mini")
        .context_window_tokens(200_000)
        .build()
        .expect("valid test model spec")
}

fn text_message(id: &str, role: MessageRole, content: &str) -> Message {
    Message {
        id: id.to_string(),
        role,
        parts: vec![Part::text(format!("{id}.p0"), content.to_string(), None)].into(),
        origin: None,
    }
}

#[tokio::test]
async fn embedded_runtime_builder_loads_state_from_store() {
    // Storage only (D1 F3): the test reaches the store port directly and the
    // runtime's backend is the recording double over the same store set.
    let stores = Arc::new(SqliteStoreSet::memory().await.expect("memory store set"));
    let catalog: Arc<dyn DeploymentStore> = stores.session_store_factory();
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("stored-session"),
        policy: SessionPolicy {
            provider_id: "openai-compatible".into(),
            model: test_model_spec(),
            ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        },
        turn_index: 3,
        token_usage: TokenUsage {
            input_tokens: 20,
            output_tokens: 5,
            cache_read_input_tokens: 2,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 1,
        },
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    state.ensure_agent_frame_initialized();
    let store = lash_core::runtime::admit_session_view(
        &catalog,
        &lash_core::testing::store_fixtures::root_session_request(&state.session_id),
    )
    .await
    .expect("admit the session");
    state.append_active_read_delta(&[text_message("u0", MessageRole::User, "stored question")]);
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit session state");

    let runtime = Box::pin(
        LashRuntime::builder(
            lash_core::facade_support::RuntimeHostConfig::new(
                lash_conformance::recording_backend_over(stores.clone()),
                lash_core::CommitBudget::bounded(1024 * 1024, 512),
                lash_core::QueuedWorkBatchingConfig::new(1),
            ),
            lash_core::LeaseOwnerIdentity::opaque("protocol-test-worker", "protocol-test-boot"),
        )
        .with_store(store.clone())
        .with_plugin_factories(vec![Arc::new(
            lash_protocol_standard::StandardProtocolPluginFactory::new(),
        )])
        .build(),
    )
    .await
    .expect("runtime");

    let state = runtime.export_state();
    let read_view = state.read_view();
    assert_eq!(read_view.messages().len(), 1);
    assert_eq!(
        read_view.messages()[0].parts[0].content(),
        "stored question"
    );
    assert_eq!(state.turn_index, 3);
    assert_eq!(state.token_usage.input_tokens, 20);
    assert_eq!(state.policy.model.id, "gpt-5.4-mini");
    assert_eq!(state.session_id, "stored-session");
}

#[tokio::test]
async fn embedded_runtime_builder_rejects_store_bound_to_different_session_id() {
    // Storage only (D1 F3): the test reaches the store port directly and the
    // runtime's backend is the recording double over the same store set.
    let stores = Arc::new(SqliteStoreSet::memory().await.expect("memory store set"));
    let catalog: Arc<dyn DeploymentStore> = stores.session_store_factory();
    let state = RuntimeSessionState {
        session_id: SessionId::from("alpha"),
        policy: SessionPolicy {
            provider_id: "openai-compatible".into(),
            model: test_model_spec(),
            ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        },
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    let store = lash_core::runtime::admit_session_view(
        &catalog,
        &lash_core::testing::store_fixtures::root_session_request(&state.session_id),
    )
    .await
    .expect("admit the session");
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit session state");

    let err = match Box::pin(
        LashRuntime::builder(
            lash_core::facade_support::RuntimeHostConfig::new(
                lash_conformance::recording_backend_over(stores.clone()),
                lash_core::CommitBudget::bounded(1024 * 1024, 512),
                lash_core::QueuedWorkBatchingConfig::new(1),
            ),
            lash_core::LeaseOwnerIdentity::opaque("protocol-test-worker", "protocol-test-boot"),
        )
        .with_store(store)
        .with_session_id("beta")
        .with_plugin_factories(vec![Arc::new(
            lash_protocol_standard::StandardProtocolPluginFactory::new(),
        )])
        .build(),
    )
    .await
    {
        Ok(_) => panic!("mismatched store session should fail"),
        Err(err) => err,
    };

    assert!(err.to_string().contains("bound to session `alpha`"));
}
