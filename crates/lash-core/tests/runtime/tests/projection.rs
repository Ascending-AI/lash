// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use lash_core::PartKind;
use lash_core::SessionCommitStore as _;
use lash_core::facade_support::RuntimeSessionStateFacadeOps;
use lash_core::plugin::PluginSessionRequest;
use lash_core::testing::TestTurnDrive as _;
use lash_sansio::core_support::*;

const SEED: u64 = 0x5_f505;

struct AppendRollbackProtocolFactory {
    store: Arc<RecordingStore>,
    protocol_dirty: Arc<AtomicBool>,
    restore_called: Arc<AtomicBool>,
    fail_restore: Arc<AtomicBool>,
    advance_store_head: bool,
}

impl lash_core::facade_support::PluginFactory for AppendRollbackProtocolFactory {
    fn id(&self) -> &'static str {
        "test_protocol"
    }

    fn declaration(&self) -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(self.id())
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::facade_support::SessionPlugin>, lash_core::PluginError> {
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

impl lash_core::facade_support::SessionPlugin for AppendRollbackProtocolPlugin {
    fn id(&self) -> &'static str {
        "test_protocol"
    }

    fn register(
        &self,
        reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
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
impl lash_core::plugin::ProtocolSessionPlugin for AppendRollbackProtocolSession {
    async fn append_session_nodes(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        _nodes: &[lash_core::SessionAppendNode],
    ) -> Result<(), lash_core::SessionError> {
        self.protocol_dirty.store(true, Ordering::SeqCst);
        if self.advance_store_head {
            // Another writer lands a commit while the append is in flight.
            lash_core::testing::runtime_helpers::advance_session_head(self.store.as_ref(), |_| {})
                .await;
        }
        Ok(())
    }

    async fn restore_session(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        _state: lash_core::plugin::ProtocolSessionRestoreView,
    ) -> Result<(), lash_core::SessionError> {
        self.protocol_dirty.store(false, Ordering::SeqCst);
        self.restore_called.store(true, Ordering::SeqCst);
        if self.fail_restore.load(Ordering::SeqCst) {
            return Err(lash_core::SessionError::Protocol(
                "injected protocol restore failure".to_string(),
            ));
        }
        Ok(())
    }
}

struct UnusedAppendRollbackProtocolDriver;

impl lash_core::plugin::ProtocolDriverPlugin for UnusedAppendRollbackProtocolDriver {
    fn build_preamble(
        &self,
        _input: lash_core::ProtocolBuildInput,
    ) -> lash_core::TurnDriverPreamble {
        panic!("append rollback test never builds a turn")
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn presentation_step_only_changes_model_observation() {
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let committed_results = Arc::new(tokio::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let committed_results_hook = Arc::clone(&committed_results);
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(move |_| {
            let committed_results = Arc::clone(&committed_results_hook);
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                presentation_steps: vec![Arc::new(|input| {
                    Box::pin(async move {
                        Ok(lash_core::facade_support::ModelToolReturn::text(
                            input.context.tool_name,
                            "model projection",
                        ))
                    })
                })],
                runtime_event: Some(Arc::new(move |event| {
                    let committed_results = Arc::clone(&committed_results);
                    Box::pin(async move {
                        if let lash_core::plugin::PluginLifecycleEvent::TurnFinalized(turn) = event
                        {
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
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(EchoTool);
    let mut runtime =
        runtime_with_plugins_and_tools(&backend, vec![plugin], tools, transport).await;

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("projection-tool-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .drive_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "run the tool".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    assert!(
        active_conversation_messages(&turn.state)
            .iter()
            .any(|message| {
                message.parts.iter().any(|part| {
                    part.content().contains("model projection")
                        && matches!(part.kind(), PartKind::ToolResult)
                })
            })
    );
    let committed = committed_results.lock().await;
    assert_eq!(
        committed.as_slice(),
        &[serde_json::json!({ "payload": "raw:sample" })]
    );
    assert_eq!(turn.tool_calls.len(), 1);
    assert_eq!(
        turn.tool_calls[0].provider_call_id.as_deref(),
        Some("tool-1")
    );
    assert_eq!(
        turn.tool_calls[0].output.value_for_projection(),
        serde_json::json!({ "payload": "raw:sample" })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn completed_turns_are_persisted_for_custom_runtime_store() {
    let double = kernel_double(SEED + 1, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![LlmStreamEvent::Delta {
            block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
            text: "Stored answer".to_string(),
        }],
        response: Ok(LlmResponse {
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

    let store = double_unbound_recording_store(&double).await;
    lash_core::testing::runtime_helpers::create_runtime_fixture_session(
        store.as_ref(),
        &SessionId::from("root"),
        &standard_test_policy(),
    )
    .await
    .expect("create the runtime fixture session");
    let plugins = plugin_session_with_tools(&SessionId::from("root"), Arc::new(EmptyTools));
    let runtime_host = test_host_config(&backend);
    let runtime_services = lash_core::facade_support::PersistentRuntimeServices::new(
        Arc::clone(&plugins),
        session_view(store.clone(), "root"),
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let mut runtime = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        runtime_host,
        runtime_services,
        RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )),
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");
    let realized_meta = session_view(store.clone(), "root")
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

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("custom-store-projection-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let _turn = runtime
        .drive_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "where did this go?".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    let read_model = durable_window(store.clone(), "root")
        .await
        .window
        .read_model();
    let messages = read_model.messages.as_slice();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, MessageRole::User);
    assert_eq!(messages[0].parts[0].content(), "where did this go?");
    assert_eq!(messages[1].role, MessageRole::Assistant);
    assert_eq!(messages[1].parts[0].content(), "Stored answer");
}

#[tokio::test(flavor = "multi_thread")]
async fn preopened_store_binds_without_remapping_initial_frame() {
    let double = kernel_double(SEED + 2, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let store = double_unbound_recording_store(&double).await;
    let policy = standard_test_policy();
    lash_core::store::SessionCatalogStore::admit_session(
        store.as_ref(),
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("preopened-session"),
            relation: lash_core::SessionRelation::Root,
            config: policy.clone().into(),
            head: lash_core::SessionCreationHead::CommittedByCreator,
        },
    )
    .await
    .expect("preopen store binding");
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("preopened-session"),
        policy: policy.clone(),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ))
    };
    state.ensure_agent_frame_initialized();
    let provisional_frame = state
        .current_frame_node_id
        .clone()
        .expect("provisional initial frame");
    let runtime_host = test_host_config(&backend);
    let runtime_services = lash_core::facade_support::PersistentRuntimeServices::new(
        plugin_session_with_tools(&SessionId::from("preopened-session"), Arc::new(EmptyTools)),
        session_view(store, "preopened-session"),
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let runtime = LashRuntime::from_persistent_embedded_state(
        policy,
        runtime_host,
        runtime_services,
        state,
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("preopened persistent runtime");
    let bound = runtime.export_persistence_state();
    let frame = bound.current_agent_frame().expect("bound initial frame");
    let lash_core::SessionNodePayload::FrameOpen { frame_key, .. } = &bound
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
        lash_core::facade_support::frame_node_id(
            &SessionId::from("preopened-session"),
            frame_key.as_str()
        ),
        "frame identity is stable before and after store binding"
    );
    assert!(matches!(
        bound.turn_scope("first-turn"),
        lash_core::ExecutionScope::Turn {
            ref session_id,
            ref turn_id,
        } if session_id == "preopened-session" && turn_id == "first-turn"
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn park_returns_error_when_final_commit_fails() {
    let double = kernel_double(SEED + 3, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let store = double_unbound_recording_store(&double).await;
    lash_core::testing::runtime_helpers::create_runtime_fixture_session(
        store.as_ref(),
        &SessionId::from("park-session"),
        &standard_test_policy(),
    )
    .await
    .expect("create the runtime fixture session");
    let plugins = plugin_session_with_tools(&SessionId::from("park-session"), Arc::new(EmptyTools));
    let runtime_host = test_host_config(&backend);
    let runtime_services = lash_core::facade_support::PersistentRuntimeServices::new(
        plugins,
        session_view(store.clone(), "park-session"),
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let runtime = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        runtime_host,
        runtime_services,
        RuntimeSessionState {
            session_id: SessionId::from("park-session"),
            policy: standard_test_policy(),
            ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
            ))
        },
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");

    store.fail_next_runtime_commit(lash_core::StoreError::Backend(
        "park-session final commit refused".to_string(),
    ));

    let err = match Box::pin(runtime.park()).await {
        Ok(_) => panic!("park should fail when final persistence fails"),
        Err(refused) => *refused.error,
    };

    let message = err.to_string();
    assert!(message.contains("failed to persist runtime state"));
    assert!(message.contains("park-session final commit refused"));
}

#[tokio::test(flavor = "multi_thread")]
async fn storeless_append_rejects_inactive_ancestor_before_mutation() {
    let double = kernel_double(SEED + 5, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let store = double_unbound_recording_store(&double).await;
    let protocol_dirty = Arc::new(AtomicBool::new(false));
    let restore_called = Arc::new(AtomicBool::new(false));
    let plugin_host =
        lash_core::testing::test_plugin_host(vec![Arc::new(AppendRollbackProtocolFactory {
            store,
            protocol_dirty: Arc::clone(&protocol_dirty),
            restore_called: Arc::clone(&restore_called),
            fail_restore: Arc::new(AtomicBool::new(false)),
            advance_store_head: false,
        })]);
    let plugins = plugin_host
        .build_session(PluginSessionRequest::creation("root", Default::default()))
        .expect("plugins");
    let runtime_host = test_host_config(&backend);
    let runtime_services = lash_core::testing::runtime_internals::RuntimeServices::new(
        plugins,
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let mut runtime = LashRuntime::from_embedded_state(
        standard_test_policy(),
        runtime_host,
        runtime_services,
        RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )),
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("storeless runtime");
    protocol_dirty.store(false, Ordering::SeqCst);
    restore_called.store(false, Ordering::SeqCst);
    let before = runtime.state().session_graph.clone();

    let result = Box::pin(runtime.append_storeless_session_nodes(
        lash_core::AppendSessionNodesRequest {
            operation_id: "storeless-stale-ancestor".to_string(),
            nodes: vec![lash_core::SessionAppendNode::plugin(
                "storeless-stale-ancestor",
                serde_json::json!({"value": 1}),
            )],
            requires_ancestor_node_id: Some("not-on-active-path".into()),
        },
    ))
    .await
    .expect("storeless ancestor fence is a typed result");

    assert!(matches!(
        result,
        lash_core::AppendSessionNodesOutcome::StaleBranch { ref required_node_id }
            if required_node_id == "not-on-active-path"
    ));
    assert!(!protocol_dirty.load(Ordering::SeqCst));
    assert!(!restore_called.load(Ordering::SeqCst));
    assert_eq!(
        serde_json::to_value(&runtime.state().session_graph).expect("encode storeless graph"),
        serde_json::to_value(&before).expect("encode original storeless graph")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn completed_turns_are_persisted_in_session_graph() {
    let double = kernel_double(SEED + 9, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta {
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "Stored answer".to_string(),
            },
            LlmStreamEvent::Usage(LlmUsage {
                input_tokens: 12,
                output_tokens: 4,
                cache_read_input_tokens: 1,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 2,
            }),
        ],
        response: Ok(LlmResponse {
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
            terminal_reason: lash_core::LlmTerminalReason::Stop,
            ..LlmResponse::default()
        }),
    }]);

    let store = double_unbound_recording_store(&double).await;
    lash_core::testing::runtime_helpers::create_runtime_fixture_session(
        store.as_ref(),
        &SessionId::from("root"),
        &standard_test_policy(),
    )
    .await
    .expect("create the runtime fixture session");
    let base_provider: Arc<dyn lash_core::ToolProvider> = Arc::new(EmptyTools);
    let base_provider_factory = Arc::clone(&base_provider);
    let plugin_host =
        lash_core::testing::test_plugin_host(vec![Arc::new(StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("base_tools"),
            lash_core::facade_support::PluginSpec::new()
                .with_tool_provider(Arc::clone(&base_provider_factory)),
        ))]);
    let plugins = plugin_host
        .build_session(PluginSessionRequest::creation("root", Default::default()))
        .expect("plugins");
    let runtime_host = test_host_config(&backend);
    let runtime_services = lash_core::facade_support::PersistentRuntimeServices::new(
        Arc::clone(&plugins),
        session_view(store.clone(), "root"),
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let mut runtime = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        runtime_host,
        runtime_services,
        RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )),
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");
    set_runtime_provider(&mut runtime, transport.clone().into_handle());

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("parked-custom-store-projection-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let _turn = runtime
        .drive_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "where did this go?".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    let read = durable_window(store.clone(), "root").await;
    let graph = read.window;
    let read_model = graph.read_model();
    let messages = read_model.messages.as_slice();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].parts[0].content(), "where did this go?");
    assert_eq!(messages[1].parts[0].content(), "Stored answer");
    let _checkpoint = read.checkpoint.expect("checkpoint");
    // The turn's usage is the owner's accounting, beside the window rather
    // than in it (ADR 0125).
    let ledger = settled_runtime_usage(&runtime).await.rows;
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].source, "turn");
    assert_eq!(
        Some(&ledger[0].model_key),
        standard_test_policy().model_key()
    );
    assert_eq!(
        Some(ledger[0].requested_model.as_str()),
        standard_test_policy().wire_model()
    );
    assert_eq!(ledger[0].usage.input_tokens, 12);
    assert_eq!(ledger[0].usage.output_tokens, 4);
    assert_eq!(ledger[0].usage.cache_read_input_tokens, 1);
    assert_eq!(ledger[0].usage.reasoning_output_tokens, 2);
}
