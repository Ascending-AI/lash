use super::*;
use crate::PartKind;
use crate::SessionCommitStore as _;
use crate::facade_support::RuntimeSessionStateFacadeOps;
use lash_sansio::core_support::*;

struct AppendRollbackProtocolFactory {
    store: Arc<RecordingStore>,
    protocol_dirty: Arc<AtomicBool>,
    restore_called: Arc<AtomicBool>,
    fail_restore: Arc<AtomicBool>,
    advance_store_head: bool,
}

impl crate::PluginFactory for AppendRollbackProtocolFactory {
    fn id(&self) -> &'static str {
        "protocol_standard"
    }

    fn build(
        &self,
        _ctx: &crate::PluginSessionContext,
    ) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError> {
        Ok(Arc::new(AppendRollbackProtocolPlugin {
            store: Arc::clone(&self.store),
            protocol_dirty: Arc::clone(&self.protocol_dirty),
            restore_called: Arc::clone(&self.restore_called),
            fail_restore: Arc::clone(&self.fail_restore),
            advance_store_head: self.advance_store_head,
        }))
    }
}

struct AppendRollbackProtocolPlugin {
    store: Arc<RecordingStore>,
    protocol_dirty: Arc<AtomicBool>,
    restore_called: Arc<AtomicBool>,
    fail_restore: Arc<AtomicBool>,
    advance_store_head: bool,
}

impl crate::SessionPlugin for AppendRollbackProtocolPlugin {
    fn id(&self) -> &'static str {
        "protocol_standard"
    }

    fn register(&self, reg: &mut crate::PluginRegistrar) -> Result<(), crate::PluginError> {
        reg.protocol()
            .session(Arc::new(AppendRollbackProtocolSession {
                store: Arc::clone(&self.store),
                protocol_dirty: Arc::clone(&self.protocol_dirty),
                restore_called: Arc::clone(&self.restore_called),
                fail_restore: Arc::clone(&self.fail_restore),
                advance_store_head: self.advance_store_head,
            }))?;
        reg.protocol()
            .protocol_driver(Arc::new(UnusedAppendRollbackProtocolDriver))?;
        Ok(())
    }
}

struct AppendRollbackProtocolSession {
    store: Arc<RecordingStore>,
    protocol_dirty: Arc<AtomicBool>,
    restore_called: Arc<AtomicBool>,
    fail_restore: Arc<AtomicBool>,
    advance_store_head: bool,
}

#[async_trait::async_trait]
impl crate::plugin::ProtocolSessionPlugin for AppendRollbackProtocolSession {
    async fn append_session_nodes(
        &self,
        _ctx: crate::plugin::ProtocolSessionContext<'_>,
        _nodes: &[crate::SessionAppendNode],
    ) -> Result<(), crate::SessionError> {
        self.protocol_dirty.store(true, Ordering::SeqCst);
        if self.advance_store_head {
            self.store
                .save_session_head_meta(crate::SessionHeadMeta::assemble(
                    crate::SessionHeadPayload::default(),
                    1,
                    None,
                    None,
                ))
                .await;
        }
        Ok(())
    }

    async fn restore_session(
        &self,
        _ctx: crate::plugin::ProtocolSessionContext<'_>,
        _state: &crate::RuntimeSessionState,
    ) -> Result<(), crate::SessionError> {
        self.protocol_dirty.store(false, Ordering::SeqCst);
        self.restore_called.store(true, Ordering::SeqCst);
        if self.fail_restore.load(Ordering::SeqCst) {
            return Err(crate::SessionError::Protocol(
                "injected protocol restore failure".to_string(),
            ));
        }
        Ok(())
    }
}

struct UnusedAppendRollbackProtocolDriver;

impl crate::plugin::ProtocolDriverPlugin for UnusedAppendRollbackProtocolDriver {
    fn build_preamble(&self, _input: crate::ProtocolBuildInput) -> crate::TurnDriverPreamble {
        panic!("append rollback test never builds a turn")
    }
}

#[tokio::test]
async fn tool_result_projector_only_changes_model_observation() {
    let committed_results = Arc::new(tokio::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let committed_results_hook = Arc::clone(&committed_results);
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(move |_| {
            let committed_results = Arc::clone(&committed_results_hook);
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                tool_result_projector: Some(Arc::new(|ctx| {
                    Box::pin(async move {
                        Ok(crate::ModelToolReturn::text(
                            ctx.call_id,
                            ctx.tool_name,
                            "model projection",
                        ))
                    })
                })),
                runtime_event: Some(Arc::new(move |event| {
                    let committed_results = Arc::clone(&committed_results);
                    Box::pin(async move {
                        if let crate::plugin::PluginLifecycleEvent::TurnFinalized(turn) = event {
                            committed_results.lock().await.push(
                                turn.tool_calls
                                    .first()
                                    .map(|call| call.output.value_for_projection().clone())
                                    .unwrap_or(serde_json::Value::Null),
                            );
                        }
                        Ok(())
                    })
                })),
                external_registrar: None,
            }))
        }),
    });
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![
                    LlmOutputPart::Text {
                        text: "checking tool".to_string(),
                        response_meta: None,
                    },
                    LlmOutputPart::ToolCall {
                        call_id: "tool-1".to_string(),
                        tool_name: "echo_tool".to_string(),
                        input_json: r#"{"value":"sample"}"#.to_string(),
                        replay: None,
                    },
                ],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "done".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(EchoTool);
    let mut runtime = runtime_with_plugins_and_tools(vec![plugin], tools, transport).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "run the tool".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "projection-tool-turn"),
        )
        .await
        .expect("turn");

    assert!(
        active_conversation_messages(&turn.state)
            .iter()
            .any(|message| {
                message.parts.iter().any(|part| {
                    part.content.contains("model projection")
                        && matches!(part.kind, PartKind::ToolResult)
                })
            })
    );
    let committed = committed_results.lock().await;
    assert_eq!(
        committed.as_slice(),
        &[serde_json::json!({ "payload": "raw:sample" })]
    );
    assert_eq!(turn.tool_calls.len(), 1);
    assert_eq!(turn.tool_calls[0].call_id.as_deref(), Some("tool-1"));
    assert_eq!(
        turn.tool_calls[0].output.value_for_projection(),
        serde_json::json!({ "payload": "raw:sample" })
    );
}

#[tokio::test]
async fn completed_turns_are_persisted_for_custom_runtime_store() {
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![LlmStreamEvent::Delta("Stored answer".to_string())],
        response: Ok(LlmResponse {
            full_text: "Stored answer".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "Stored answer".to_string(),
                response_meta: None,
            }],
            usage: LlmUsage {
                input_tokens: 12,
                output_tokens: 4,
                cache_read_input_tokens: 1,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 2,
            },
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);

    let store = Arc::new(RecordingStore::default());
    let plugins = plugin_session_with_tools("root", Arc::new(EmptyTools));
    let mut runtime = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::PersistentRuntimeServices::new(
            Arc::clone(&plugins),
            store.clone() as Arc<dyn crate::store::RuntimePersistence>,
        ),
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded)),
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");
    let realized_meta = store
        .load_session_meta()
        .await
        .expect("load realized metadata")
        .expect("persistent constructor realizes metadata");
    assert_eq!(
        runtime.export_persistence_state().session_id,
        realized_meta.session_id,
        "a new persistent runtime must bind its host-provided id to the store"
    );
    set_runtime_provider(&mut runtime, transport.clone().into_handle());

    let _turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "where did this go?".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "custom-store-projection-turn"),
        )
        .await
        .expect("turn");

    let read_model = crate::store::SessionCommitStore::load_session(store.as_ref())
        .await
        .expect("load session")
        .expect("session head")
        .graph
        .read_model();
    let messages = read_model.messages.as_slice();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, MessageRole::User);
    assert_eq!(messages[0].parts[0].content, "where did this go?");
    assert_eq!(messages[1].role, MessageRole::Assistant);
    assert_eq!(messages[1].parts[0].content, "Stored answer");
}

#[tokio::test]
async fn preopened_store_binds_without_remapping_initial_frame() {
    let store = Arc::new(RecordingStore::default());
    let policy = standard_test_policy();
    store
        .admit_and_bind_session(&crate::SessionBinding::root("preopened-session"))
        .await
        .expect("preopen store binding");
    let mut state = RuntimeSessionState {
        session_id: "preopened-session".to_string(),
        policy: policy.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    let provisional_frame = state
        .current_frame_node_id
        .clone()
        .expect("provisional initial frame");
    let runtime = LashRuntime::from_persistent_embedded_state(
        policy,
        test_host_config(),
        crate::PersistentRuntimeServices::new(
            plugin_session_with_tools("preopened-session", Arc::new(EmptyTools)),
            store as Arc<dyn crate::store::RuntimePersistence>,
        ),
        state,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("preopened persistent runtime");
    let bound = runtime.export_persistence_state();
    let frame = bound.current_agent_frame().expect("bound initial frame");
    let crate::SessionNodePayload::FrameOpen { frame_key, .. } = &bound
        .session_graph
        .find_node(&frame.frame_node_id)
        .expect("bound frame node")
        .payload
    else {
        panic!("current agent frame must resolve to FrameOpen");
    };
    assert_eq!(frame.frame_node_id, provisional_frame);
    assert_eq!(
        frame.frame_node_id,
        crate::frame_node_id("preopened-session", frame_key),
        "frame identity is stable before and after store binding"
    );
    assert!(matches!(
        bound.turn_scope("first-turn"),
        crate::ExecutionScope::Turn {
            ref session_id,
            ref turn_id,
        } if session_id == "preopened-session" && turn_id == "first-turn"
    ));
}

#[tokio::test]
async fn park_returns_error_when_final_commit_fails() {
    let store = Arc::new(RecordingStore::default());
    store
        .save_session_head_meta(crate::SessionHeadMeta::assemble(
            crate::SessionHeadPayload {
                session_id: "other-session".to_string(),
                ..crate::SessionHeadPayload::default()
            },
            0,
            None,
            None,
        ))
        .await;
    let plugins = plugin_session_with_tools("park-session", Arc::new(EmptyTools));
    let runtime = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::PersistentRuntimeServices::new(
            plugins,
            store as Arc<dyn crate::store::RuntimePersistence>,
        ),
        RuntimeSessionState {
            session_id: "park-session".to_string(),
            policy: standard_test_policy(),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
        },
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");

    let err = match runtime.park().await {
        Ok(_) => panic!("park should fail when final persistence fails"),
        Err(err) => err,
    };

    let message = err.to_string();
    assert!(message.contains("failed to persist runtime state"));
    assert!(message.contains("other-session"));
    assert!(message.contains("park-session"));
}

#[tokio::test]
async fn failed_append_restores_runtime_and_protocol_session_state() {
    let store = Arc::new(RecordingStore::default());
    let protocol_dirty = Arc::new(AtomicBool::new(false));
    let restore_called = Arc::new(AtomicBool::new(false));
    let plugin_host = crate::PluginHost::new(vec![Arc::new(AppendRollbackProtocolFactory {
        store: Arc::clone(&store),
        protocol_dirty: Arc::clone(&protocol_dirty),
        restore_called: Arc::clone(&restore_called),
        fail_restore: Arc::new(AtomicBool::new(false)),
        advance_store_head: true,
    })]);
    let plugins = plugin_host.build_session("root", None).expect("plugins");
    let mut runtime = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::PersistentRuntimeServices::new(
            plugins,
            store as Arc<dyn crate::store::RuntimePersistence>,
        ),
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded)),
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");
    protocol_dirty.store(false, Ordering::SeqCst);
    restore_called.store(false, Ordering::SeqCst);

    let err = runtime
        .append_session_nodes(crate::AppendSessionNodesRequest {
            operation_id: "append-rollback".to_string(),
            nodes: vec![crate::SessionAppendNode::plugin(
                "rollback-test",
                serde_json::json!({"value": 1}),
            )],
            requires_ancestor_node_id: None,
        })
        .await
        .expect_err("concurrent head movement must reject the append");

    assert!(err.to_string().contains("head revision conflict"));
    assert!(
        restore_called.load(Ordering::SeqCst),
        "persistence failure must restore the protocol session"
    );
    assert!(
        !protocol_dirty.load(Ordering::SeqCst),
        "protocol session must match the rolled-back runtime state"
    );
    assert_eq!(runtime.state.session_graph.nodes.len(), 1);
    assert!(matches!(
        runtime.state.session_graph.nodes[0].payload,
        crate::SessionNodePayload::FrameOpen { .. }
    ));
}

#[tokio::test]
async fn storeless_append_rejects_inactive_ancestor_before_mutation() {
    let store = Arc::new(RecordingStore::default());
    let protocol_dirty = Arc::new(AtomicBool::new(false));
    let restore_called = Arc::new(AtomicBool::new(false));
    let plugin_host = crate::PluginHost::new(vec![Arc::new(AppendRollbackProtocolFactory {
        store,
        protocol_dirty: Arc::clone(&protocol_dirty),
        restore_called: Arc::clone(&restore_called),
        fail_restore: Arc::new(AtomicBool::new(false)),
        advance_store_head: false,
    })]);
    let plugins = plugin_host.build_session("root", None).expect("plugins");
    let mut runtime = LashRuntime::from_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::RuntimeServices::new(plugins),
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded)),
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("storeless runtime");
    protocol_dirty.store(false, Ordering::SeqCst);
    restore_called.store(false, Ordering::SeqCst);
    let before = runtime.state.session_graph.clone();

    let result = runtime
        .append_session_nodes(crate::AppendSessionNodesRequest {
            operation_id: "storeless-stale-ancestor".to_string(),
            nodes: vec![crate::SessionAppendNode::plugin(
                "storeless-stale-ancestor",
                serde_json::json!({"value": 1}),
            )],
            requires_ancestor_node_id: Some("not-on-active-path".to_string()),
        })
        .await
        .expect("storeless ancestor fence is a typed result");

    assert!(matches!(
        result,
        crate::AppendSessionNodesResult::StaleBranch { ref required_node_id }
            if required_node_id == "not-on-active-path"
    ));
    assert!(!protocol_dirty.load(Ordering::SeqCst));
    assert!(!restore_called.load(Ordering::SeqCst));
    assert_eq!(
        serde_json::to_value(&runtime.state.session_graph).expect("encode storeless graph"),
        serde_json::to_value(&before).expect("encode original storeless graph")
    );
}

#[test]
fn append_session_nodes_lost_response_retry_replays_and_refreshes_resident_state() {
    const CHILD_ENV: &str = "LASH_FIG850_APPEND_RETRY_CHILD";
    if std::env::var_os(CHILD_ENV).is_some() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("FIG-850 child runtime")
            .block_on(append_session_nodes_retry_after_head_advance_is_typed_scenario());
        return;
    }

    crate::test_watchdog::assert_exact_test_completes(
        "runtime::tests::projection::append_session_nodes_lost_response_retry_replays_and_refreshes_resident_state",
        CHILD_ENV,
        "append retry",
    );
}

async fn append_session_nodes_retry_after_head_advance_is_typed_scenario() {
    let store = Arc::new(RecordingStore::default());
    let protocol_dirty = Arc::new(AtomicBool::new(false));
    let restore_called = Arc::new(AtomicBool::new(false));
    let plugin_host = crate::PluginHost::new(vec![Arc::new(AppendRollbackProtocolFactory {
        store: Arc::clone(&store),
        protocol_dirty: Arc::clone(&protocol_dirty),
        restore_called: Arc::clone(&restore_called),
        fail_restore: Arc::new(AtomicBool::new(false)),
        advance_store_head: false,
    })]);
    let plugins = plugin_host.build_session("root", None).expect("plugins");
    let mut runtime = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::PersistentRuntimeServices::new(
            plugins,
            store as Arc<dyn crate::store::RuntimePersistence>,
        ),
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded)),
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");
    runtime
        .append_session_nodes(crate::AppendSessionNodesRequest {
            operation_id: "retry-after-advance-seed".to_string(),
            nodes: vec![crate::SessionAppendNode::plugin(
                "retry-test",
                serde_json::json!({"value": 0}),
            )],
            requires_ancestor_node_id: None,
        })
        .await
        .expect("persist the initial frame before the retried operation");
    let request = crate::AppendSessionNodesRequest {
        operation_id: "retry-after-advance".to_string(),
        nodes: vec![crate::SessionAppendNode::plugin(
            "retry-test",
            serde_json::json!({"value": 1}),
        )],
        requires_ancestor_node_id: None,
    };

    let first = runtime
        .append_session_nodes(request.clone())
        .await
        .expect("first append");
    let crate::AppendSessionNodesResult::Appended {
        node_ids: first_node_ids,
        leaf_node_id: first_leaf_node_id,
    } = first
    else {
        panic!("first append must report its persisted node id");
    };
    let persisted_node_id = first_node_ids.first().cloned().expect("persisted node id");
    // Model a response lost after the durable commit: the caller retains only
    // its request and retries after unrelated work has advanced the head.
    runtime
        .append_session_nodes(crate::AppendSessionNodesRequest {
            operation_id: "advance-head".to_string(),
            nodes: vec![crate::SessionAppendNode::plugin(
                "retry-test",
                serde_json::json!({"value": 2}),
            )],
            requires_ancestor_node_id: None,
        })
        .await
        .expect("head advance");
    let state_after_advance = runtime.state.clone();
    protocol_dirty.store(false, Ordering::SeqCst);
    restore_called.store(false, Ordering::SeqCst);

    let replay = runtime
        .append_session_nodes(request)
        .await
        .expect("the lost-response retry must replay its durable receipt");
    let crate::AppendSessionNodesResult::Appended {
        node_ids: replayed_node_ids,
        leaf_node_id: replayed_leaf_node_id,
    } = replay
    else {
        panic!("the lost-response retry must report the original append");
    };
    assert_eq!(replayed_node_ids, first_node_ids);
    assert_eq!(replayed_leaf_node_id, first_leaf_node_id);
    assert!(
        restore_called.load(Ordering::SeqCst),
        "receipt replay must restore the protocol session from durable history"
    );
    assert!(
        !protocol_dirty.load(Ordering::SeqCst),
        "the protocol session must match the refreshed durable runtime state"
    );
    assert_eq!(
        runtime.state.head_revision, state_after_advance.head_revision,
        "receipt replay must not advance the durable head"
    );
    assert_eq!(
        serde_json::to_value(&runtime.state.session_graph).expect("encode rolled-back graph"),
        serde_json::to_value(&state_after_advance.session_graph).expect("encode expected graph"),
        "resident graph must converge with durable history after receipt replay"
    );
    assert_eq!(
        runtime
            .state
            .session_graph
            .nodes
            .iter()
            .filter(|node| node.node_id == persisted_node_id)
            .count(),
        1,
        "the replayed node must exist exactly once in resident history"
    );
}

#[tokio::test]
async fn replay_refresh_failure_restores_pre_append_runtime_and_protocol_state() {
    let store = Arc::new(RecordingStore::default());
    let protocol_dirty = Arc::new(AtomicBool::new(false));
    let restore_called = Arc::new(AtomicBool::new(false));
    let plugin_host = crate::PluginHost::new(vec![Arc::new(AppendRollbackProtocolFactory {
        store: Arc::clone(&store),
        protocol_dirty: Arc::clone(&protocol_dirty),
        restore_called: Arc::clone(&restore_called),
        fail_restore: Arc::new(AtomicBool::new(false)),
        advance_store_head: false,
    })]);
    let plugins = plugin_host.build_session("root", None).expect("plugins");
    let mut runtime = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::PersistentRuntimeServices::new(
            plugins,
            Arc::clone(&store) as Arc<dyn crate::store::RuntimePersistence>,
        ),
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded)),
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");
    let request = crate::AppendSessionNodesRequest {
        operation_id: "replay-refresh-failure".to_string(),
        nodes: vec![crate::SessionAppendNode::plugin(
            "replay-refresh-failure",
            serde_json::json!({"value": 1}),
        )],
        requires_ancestor_node_id: None,
    };
    runtime
        .append_session_nodes(request.clone())
        .await
        .expect("first append");
    let state_before_retry = runtime.state.clone();
    protocol_dirty.store(false, Ordering::SeqCst);
    restore_called.store(false, Ordering::SeqCst);
    store.fail_load_session_on_call(store.load_session_count() + 1);

    let error = runtime
        .append_session_nodes(request)
        .await
        .expect_err("post-replay refresh failure must reject and roll back");

    assert!(matches!(
        error,
        crate::SessionError::Store { ref context, ref source }
            if context.contains("failed to refresh resident state after append receipt replay")
                && matches!(source, crate::StoreError::Backend(message) if message == "injected load-session failure")
    ));
    assert!(restore_called.load(Ordering::SeqCst));
    assert!(!protocol_dirty.load(Ordering::SeqCst));
    assert_eq!(
        runtime.state.head_revision,
        state_before_retry.head_revision
    );
    assert_eq!(
        serde_json::to_value(&runtime.state.session_graph).expect("encode restored graph"),
        serde_json::to_value(&state_before_retry.session_graph).expect("encode expected graph")
    );
}

#[tokio::test]
async fn failed_append_rollback_preserves_a_deleted_session_cause() {
    let session_id = "deleted-during-append-rollback";
    let store = Arc::new(RecordingStore::default());
    let protocol_dirty = Arc::new(AtomicBool::new(false));
    let restore_called = Arc::new(AtomicBool::new(false));
    let fail_restore = Arc::new(AtomicBool::new(false));
    let plugin_host = crate::PluginHost::new(vec![Arc::new(AppendRollbackProtocolFactory {
        store: Arc::clone(&store),
        protocol_dirty,
        restore_called: Arc::clone(&restore_called),
        fail_restore: Arc::clone(&fail_restore),
        advance_store_head: false,
    })]);
    let plugins = plugin_host.build_session("root", None).expect("plugins");
    let mut runtime = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::PersistentRuntimeServices::new(
            plugins,
            store.clone() as Arc<dyn crate::store::RuntimePersistence>,
        ),
        RuntimeSessionState {
            session_id: session_id.to_string(),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
        },
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");
    fail_restore.store(true, Ordering::SeqCst);
    store.fail_next_runtime_commit(crate::StoreError::SessionDeleted {
        session_id: session_id.to_string(),
    });

    let error = runtime
        .append_session_nodes(crate::AppendSessionNodesRequest {
            operation_id: "typed-append-rollback".to_string(),
            nodes: vec![crate::SessionAppendNode::plugin(
                "rollback-test",
                serde_json::json!({"value": 1}),
            )],
            requires_ancestor_node_id: None,
        })
        .await
        .expect_err("the injected persistence and rollback failures must reject the append");

    assert!(restore_called.load(Ordering::SeqCst));
    assert!(matches!(
        &error,
        crate::SessionError::Store { context, source }
            if context.contains("failed to persist runtime state")
                && context.contains(
                    "failed to restore protocol session: protocol error: injected protocol restore failure"
                )
                && matches!(
                    source,
                    crate::StoreError::SessionDeleted {
                        session_id: deleted_session_id
                    } if deleted_session_id == session_id
                )
    ));
    assert!(
        error.to_string().contains(
            &crate::StoreError::SessionDeleted {
                session_id: session_id.to_string(),
            }
            .to_string()
        ),
        "the canonical tombstone message must remain renderable: {error}"
    );
}

#[tokio::test]
async fn completed_turns_are_persisted_in_session_graph() {
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta("Stored answer".to_string()),
            LlmStreamEvent::Usage(LlmUsage {
                input_tokens: 12,
                output_tokens: 4,
                cache_read_input_tokens: 1,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 2,
            }),
        ],
        response: Ok(LlmResponse {
            full_text: "Stored answer".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "Stored answer".to_string(),
                response_meta: None,
            }],
            usage: LlmUsage {
                input_tokens: 12,
                output_tokens: 4,
                cache_read_input_tokens: 1,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 2,
            },
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);

    let store = Arc::new(RecordingStore::default());
    let base_provider: Arc<dyn crate::ToolProvider> = Arc::new(EmptyTools);
    let base_provider_factory = Arc::clone(&base_provider);
    let plugin_host = crate::PluginHost::new(vec![Arc::new(StaticPluginFactory::new(
        "base_tools",
        crate::PluginSpec::new().with_tool_provider(Arc::clone(&base_provider_factory)),
    ))]);
    let plugins = plugin_host.build_session("root", None).expect("plugins");
    let mut runtime = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::PersistentRuntimeServices::new(
            Arc::clone(&plugins),
            store.clone() as Arc<dyn crate::store::RuntimePersistence>,
        ),
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded)),
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");
    set_runtime_provider(&mut runtime, transport.clone().into_handle());

    let _turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "where did this go?".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "parked-custom-store-projection-turn"),
        )
        .await
        .expect("turn");

    let read = crate::store::SessionCommitStore::load_session(store.as_ref())
        .await
        .expect("load session")
        .expect("session read");
    let graph = read.graph;
    let read_model = graph.read_model();
    let messages = read_model.messages.as_slice();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].parts[0].content, "where did this go?");
    assert_eq!(messages[1].parts[0].content, "Stored answer");
    let _checkpoint = read.checkpoint.expect("checkpoint");
    let ledger = read.token_ledger;
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].source, "turn");
    assert_eq!(ledger[0].model, standard_test_policy().model.id);
    assert_eq!(ledger[0].usage.input_tokens, 12);
    assert_eq!(ledger[0].usage.output_tokens, 4);
    assert_eq!(ledger[0].usage.cache_read_input_tokens, 1);
    assert_eq!(ledger[0].usage.reasoning_output_tokens, 2);
}
