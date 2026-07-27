use std::sync::Arc;

use lash_core::{
    LashRuntime, Message, MessageRole, ModelSpec, Part, PartKind, PersistedSessionConfig,
    PersistedTurnState, PruneState, RuntimePersistence, RuntimeSessionState, SessionGraph,
    SessionHead, TokenUsage,
};
use lash_sqlite_store::Store;

fn test_model_spec() -> ModelSpec {
    ModelSpec::from_token_limits("gpt-5.4-mini", Default::default(), 200_000, None)
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
    let checkpoint_ref = store
        .put_checkpoint(&lash_core::store::HydratedSessionCheckpoint {
            turn_state: PersistedTurnState {
                turn_index: 3,
                token_usage: TokenUsage {
                    input_tokens: 20,
                    output_tokens: 5,
                    cache_read_input_tokens: 2,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 1,
                },
                last_prompt_usage: None,
                protocol_turn_options: Default::default(),
            },
            tool_state_ref: None,
            tool_state: None,
            plugin_snapshot_ref: None,
            plugin_snapshot_revision: None,
            plugin_snapshot: None,
            execution_state_ref: None,
            execution_state: None,
        })
        .await
        .checkpoint_ref;
    let mut state = RuntimeSessionState {
        session_id: "stored-session".to_string(),
        ..RuntimeSessionState::default()
    };
    state.ensure_agent_frame_initialized();
    state.append_active_read_delta(&[text_message("u0", MessageRole::User, "stored question")]);
    store
        .save_session_head(SessionHead {
            session_id: "stored-session".to_string(),
            head_revision: 0,
            current_frame_node_id: state.current_frame_node_id,
            graph: state.session_graph,
            config: PersistedSessionConfig {
                provider_id: "openai-compatible".into(),
                model: test_model_spec(),
            },
            checkpoint_ref: Some(checkpoint_ref),
            token_ledger: Vec::new(),
        })
        .await;

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
    store
        .save_session_head(SessionHead {
            session_id: "alpha".to_string(),
            head_revision: 0,
            current_frame_node_id: None,
            graph: SessionGraph::default(),
            config: PersistedSessionConfig {
                provider_id: "openai-compatible".into(),
                model: test_model_spec(),
            },
            checkpoint_ref: None,
            token_ledger: Vec::new(),
        })
        .await;

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
