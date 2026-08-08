use std::sync::Arc;

use lash_core::{
    Message, MessageRole, ModelSpec, Part, PartKind, PruneState, RuntimeCommit, RuntimePersistence,
    RuntimeSessionState, SessionCommitStore, SessionPolicy, TokenUsage,
    facade_support::LashRuntime,
};
use lash_sqlite_store::Store;

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
        parts: vec![Part {
            id: format!("{id}.p0"),
            kind: PartKind::Text,
            content: content.to_string(),
            attachment: None,
            tool_call_id: None,
            tool_name: None,
            tool_replay: None,
            prune_state: PruneState::Intact,
            reasoning_meta: None,
            response_meta: None,
        }]
        .into(),
        origin: None,
    }
}

#[tokio::test]
async fn embedded_runtime_builder_loads_state_from_store() {
    let store = Arc::new(Store::memory().await.expect("store"));
    let mut state = RuntimeSessionState {
        session_id: "stored-session".to_string(),
        policy: SessionPolicy {
            provider_id: "openai-compatible".into(),
            model: test_model_spec(),
            ..SessionPolicy::default()
        },
        turn_index: 3,
        token_usage: TokenUsage {
            input_tokens: 20,
            output_tokens: 5,
            cache_read_input_tokens: 2,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 1,
        },
        ..RuntimeSessionState::default()
    };
    state.ensure_agent_frame_initialized();
    store
        .admit_and_bind_session(&lash_core::SessionBinding::root(
            state.session_id.clone(),
            &state.policy,
        ))
        .await
        .expect("bind session to store");
    state.append_active_read_delta(&[text_message("u0", MessageRole::User, "stored question")]);
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit session state");

    let runtime = LashRuntime::builder()
        .with_store(store.clone() as Arc<dyn RuntimePersistence>)
        .with_plugin_factories(vec![Arc::new(
            lash_protocol_standard::StandardProtocolPluginFactory,
        )])
        .build()
        .await
        .expect("runtime");

    let state = runtime.export_state();
    let read_view = state.read_view();
    assert_eq!(read_view.messages().len(), 1);
    assert_eq!(read_view.messages()[0].parts[0].content, "stored question");
    assert_eq!(state.turn_index, 3);
    assert_eq!(state.token_usage.input_tokens, 20);
    assert_eq!(state.policy.model.id, "gpt-5.4-mini");
    assert_eq!(state.session_id, "stored-session");
}

#[tokio::test]
async fn embedded_runtime_builder_rejects_store_bound_to_different_session_id() {
    let store = Arc::new(Store::memory().await.expect("store"));
    let state = RuntimeSessionState {
        session_id: "alpha".to_string(),
        policy: SessionPolicy {
            provider_id: "openai-compatible".into(),
            model: test_model_spec(),
            ..SessionPolicy::default()
        },
        ..RuntimeSessionState::default()
    };
    store
        .admit_and_bind_session(&lash_core::SessionBinding::root(
            state.session_id.clone(),
            &state.policy,
        ))
        .await
        .expect("bind session to store");
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit session state");

    let err = match LashRuntime::builder()
        .with_store(store as Arc<dyn RuntimePersistence>)
        .with_session_id("beta")
        .with_plugin_factories(vec![Arc::new(
            lash_protocol_standard::StandardProtocolPluginFactory,
        )])
        .build()
        .await
    {
        Ok(_) => panic!("mismatched store session should fail"),
        Err(err) => err,
    };

    assert!(err.to_string().contains("bound to session `alpha`"));
}
