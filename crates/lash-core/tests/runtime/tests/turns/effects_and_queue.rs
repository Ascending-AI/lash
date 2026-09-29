use super::*;
use lash_core::testing::TestTurnDrive as _;

const SEED: u64 = 0x5_a300;

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn fig1123_queued_frame_switch_finishes_follow_on_before_next_queued_turn() {
    let double = kernel_double(SEED + 1, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let store = double_unbound_recording_store(&double).await;
    let captured_store = Arc::clone(&store);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_call_index = Arc::clone(&call_index);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |request| {
            let store = Arc::clone(&captured_store);
            let requests = Arc::clone(&captured_requests);
            let call_index = Arc::clone(&captured_call_index);
            async move {
                requests.lock_recover().push(request);
                match call_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 => {
                        enqueue_idle_turn_input(
                            store.as_ref(),
                            &SessionId::from("root"),
                            "second queued turn",
                        )
                        .await;
                        Ok(LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "switch-call".to_string(),
                                tool_name: "terminal_tool_0".to_string(),
                                input_json: serde_json::json!({}).to_string(),
                                replay: Some(lash_sansio::llm::types::ProviderReplayMeta {
                                    opaque: Some("prior-frame-opaque".to_string()),
                                    ..Default::default()
                                }),
                            }],
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        })
                    }
                    1 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "follow-on complete".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    2 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "second queued complete".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    index => panic!("unexpected provider call {index}"),
                }
            }
        })
        .build();
    let runtime_store: Arc<dyn lash_core::store::RuntimeStore> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(TerminalControlTool {
            controls: vec![lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("queued-follow-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("run follow-on task".to_string()),
            }],
        }),
        transport,
        test_host_config(&backend),
        runtime_store,
    )
    .await;
    let first = enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "first queued turn",
    )
    .await;
    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from("root").clone(),
            TurnId::from("queued-frame-chain").clone(),
        ))
        .await
        .expect("open the scope's handler");
    let first_result = runtime
        .drive_next_root(
            "queued-frame-chain",
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("queued frame chain succeeds")
        .expect("queued frame root admitted")
        .into_final_turn()
        .expect("queued frame chain returns its terminal turn");
    handler.close().await.expect("close the scope's handler");

    assert_eq!(
        first_result.assistant_output.safe_text,
        "follow-on complete"
    );
    let pending_after_follow = lash_core::store::TurnInputStore::list_pending_turn_inputs(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("pending inputs after frame follow");
    assert_eq!(pending_after_follow.len(), 1);
    assert_ne!(pending_after_follow[0].input.input_id, first.input_id);
    let requests_after_follow = requests.lock_recover().clone();
    assert_eq!(requests_after_follow.len(), 2);
    assert!(request_contains_text(
        &requests_after_follow[1],
        "run follow-on task"
    ));
    assert!(!request_contains_text(
        &requests_after_follow[1],
        "first queued turn"
    ));
    assert!(!request_contains_text(
        &requests_after_follow[1],
        "second queued turn"
    ));
    assert!(
        !serde_json::to_string(&requests_after_follow[1])
            .expect("request JSON")
            .contains("prior-frame-opaque"),
        "opaque replay from the prior frame must not enter the follow-on request"
    );
    let follow_frame = runtime
        .state()
        .current_frame_node_id
        .as_deref()
        .expect("follow-on frame is active");
    assert_eq!(requests_after_follow[1].scope.agent_frame_id, follow_frame);
    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from("root").clone(),
            TurnId::from("second-queued-after-frame-chain").clone(),
        ))
        .await
        .expect("open the scope's handler");
    let second_result = runtime
        .drive_next_root(
            "second-queued-after-frame-chain",
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("second queued turn succeeds")
        .expect("second queued root admitted")
        .into_final_turn()
        .expect("second queued turn runs after the frame chain");
    handler.close().await.expect("close the scope's handler");

    assert_eq!(
        second_result.assistant_output.safe_text,
        "second queued complete"
    );
    assert!(
        lash_core::store::TurnInputStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("pending inputs after second turn")
        .is_empty()
    );
    let requests = requests.lock_recover();
    assert_eq!(requests.len(), 3);
    assert!(request_contains_text(&requests[2], "second queued turn"));
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn mid_chain_cancellation_commits_one_cancelled_terminal_and_settles_handoff() {
    let double = kernel_double(SEED + 2, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    const SESSION_ID: &str = "mid-chain-cancellation";

    let store = double_unbound_recording_store(&double).await;
    let cancel = CancellationToken::new();
    struct CancelAfterSwitch(CancellationToken);
    impl lash_core::runtime::RuntimeTurnPhaseProbe for CancelAfterSwitch {
        fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
            if phase == lash_core::runtime::RuntimeTurnPhase::PostCommitDelivery {
                self.0.cancel();
            }
        }

        fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
    }
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| async move {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "switch-call".to_string(),
                    tool_name: "terminal_tool_0".to_string(),
                    input_json: "{}".to_string(),
                    replay: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build();
    let runtime_store: Arc<dyn lash_core::store::RuntimeStore> = store.clone();
    let mut runtime = TestRuntime::new(&backend, transport)
        .tools(Arc::new(TerminalControlTool {
            controls: vec![lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("cancelled-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("cancel before running".to_string()),
            }],
        }))
        .host(test_host_config(&backend))
        .store(runtime_store)
        .with_session_id(SESSION_ID)
        .build()
        .await;
    runtime.set_turn_phase_probe(Arc::new(CancelAfterSwitch(cancel.clone())));
    enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from(SESSION_ID),
        "start cancellable switch",
    )
    .await;
    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from(SESSION_ID).clone(),
            TurnId::from("mid-chain-cancel").clone(),
        ))
        .await
        .expect("open the scope's handler");
    let terminal = runtime
        .drive_one_admitted_queued_root(TurnOptions::new(cancel, handler.scoped()))
        .await
        .expect("cancelled chain assembles")
        .ran()
        .expect("cancelled terminal turn");
    handler.close().await.expect("close the scope's handler");
    assert!(matches!(
        terminal.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    assert!(
        lash_core::store::QueuedWorkStore::list_queued_work(
            store.as_ref(),
            &SessionId::from(SESSION_ID)
        )
        .await
        .expect("queue after cancellation")
        .is_empty()
    );
    assert!(
        lash_core::store::SessionCommitStore::load_session_head_meta(
            store.as_ref(),
            &lash_core::SessionId::from(SESSION_ID)
        )
        .await
        .expect("load the head")
        .expect("head")
        .pending_follow_on
        .is_none(),
        "a cancelled follow-on is its own terminal and clears the fact"
    );
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn claimed_normalization_failure_commits_and_settles_input() {
    let double = kernel_double(SEED + 3, lash_restate_test::ServerConfig::default()).await;
    #[derive(Debug)]
    struct DenyClaimedAttachments;

    impl lash_core::testing::runtime_internals::AttachmentSourcePolicy for DenyClaimedAttachments {
        fn authorize(
            &self,
            producer: &lash_core::testing::runtime_internals::AttachmentProducer,
            _source: &lash_core::AttachmentSource,
        ) -> Result<(), lash_core::test_support::AttachmentSourcePolicyError> {
            Err(lash_core::test_support::AttachmentSourcePolicyError {
                producer: producer.clone(),
                reason: "claimed attachment denied for test".to_string(),
            })
        }
    }

    let (mut runtime, store) =
        standard_runtime_with_transport_and_double_queue_store(&double, mock_provider(Vec::new()))
            .await;
    runtime.host.core.attachment_source_policy = Arc::new(DenyClaimedAttachments);
    let inbound = lash_core::store::TurnInputStore::enqueue_pending_turn_input(
        store.as_ref(),
        lash_core::PendingTurnInputDraft::new(
            "root",
            lash_core::TurnInputIngress::NextTurn,
            TurnInput::items([InputItem::attachment(
                lash_core::AttachmentSource::external_url(
                    lash_core::MediaType::parse("application/pdf").unwrap(),
                    "https://example.test/denied.pdf",
                ),
            )]),
        ),
    )
    .await
    .expect("enqueue invalid input");
    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from("root").clone(),
            TurnId::from("invalid-claimed-input").clone(),
        ))
        .await
        .expect("open the scope's handler");
    let terminal = runtime
        .drive_one_admitted_queued_root(TurnOptions::new(
            CancellationToken::new(),
            handler.scoped(),
        ))
        .await
        .expect("invalid input assembles")
        .ran()
        .expect("invalid terminal turn");
    handler.close().await.expect("close the scope's handler");
    assert!(matches!(
        terminal.outcome,
        TurnOutcome::Stopped(TurnStop::InvalidInput)
    ));
    let inputs = lash_core::store::TurnInputStore::list_pending_turn_inputs(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("list completed invalid input");
    assert!(
        inputs
            .iter()
            .all(|input| input.input.input_id != inbound.input_id)
    );
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn claimed_plugin_abort_commits_and_settles_input() {
    let double = kernel_double(SEED + 4, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: Some(Arc::new(|_| {
                    Box::pin(async {
                        Ok(vec![
                            lash_core::facade_support::TurnPluginDirective::AbortTurn(
                                lash_core::facade_support::AbortTurnDirective {
                                    code: "blocked".to_string(),
                                    message: "plugin stopped claimed turn".to_string(),
                                },
                            ),
                        ])
                    })
                })),
                checkpoint: None,
                presentation_steps: vec![],
                runtime_event: None,
                external_registrar: None,
            }))
        }),
    });
    let store = double_unbound_recording_store(&double).await;
    let runtime_store: Arc<dyn lash_core::store::RuntimeStore> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![plugin],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(&backend),
        runtime_store,
    )
    .await;
    let inbound =
        enqueue_idle_turn_input(store.as_ref(), &SessionId::from("root"), "abort this input").await;
    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from("root").clone(),
            TurnId::from("claimed-plugin-abort").clone(),
        ))
        .await
        .expect("open the scope's handler");
    let terminal = runtime
        .drive_one_admitted_queued_root(TurnOptions::new(
            CancellationToken::new(),
            handler.scoped(),
        ))
        .await
        .expect("plugin abort assembles")
        .ran()
        .expect("plugin abort terminal turn");
    handler.close().await.expect("close the scope's handler");
    assert!(matches!(
        terminal.outcome,
        TurnOutcome::Stopped(TurnStop::PluginAbort)
    ));
    let inputs = lash_core::store::TurnInputStore::list_pending_turn_inputs(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("list completed aborted input");
    assert!(
        inputs
            .iter()
            .all(|input| input.input.input_id != inbound.input_id)
    );
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn stream_turn_tool_put_is_bound_to_the_turn_id() {
    let double = kernel_double(SEED + 5, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    const TURN_ID: &str = "attachment-owner-stream-turn";
    let store = double_unbound_recording_store(&double).await;
    let runtime_store: Arc<dyn lash_core::RuntimeStore> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(AttachmentPutTool),
        attachment_put_transport(),
        test_host_config(&backend),
        runtime_store,
    )
    .await;
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root").clone(),
            TurnId::from(TURN_ID).clone(),
        ))
        .await
        .expect("open the scope's handler");
    runtime
        .drive_turn(
            TurnInput::text("store an attachment"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("stream turn succeeds");
    handler.close().await.expect("close the scope's handler");

    assert_turn_owned_attachment(store.as_ref(), &TurnId::from(TURN_ID));
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn admitted_drive_reuses_graph_across_follow_on_and_rechecks_next_drive() {
    let double = kernel_double(SEED + 13, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_call_index = Arc::clone(&call_index);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let call_index = Arc::clone(&captured_call_index);
            async move {
                match call_index.fetch_add(1, Ordering::SeqCst) {
                    0 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "resident-switch".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    1 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "retained lease follow-on".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    2 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "reacquired lease turn".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    index => panic!("unexpected provider call {index}"),
                }
            }
        })
        .build();
    let store = double_unbound_recording_store(&double).await;
    let runtime_store: Arc<dyn lash_core::store::RuntimeStore> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(TerminalControlTool {
            controls: vec![lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("resident-follow-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("continue in the admitted drive".to_string()),
            }],
        }),
        transport,
        test_host_config(&backend),
        runtime_store,
    )
    .await;
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root").clone(),
            TurnId::from("resident-chain").clone(),
        ))
        .await
        .expect("open the scope's handler");
    let run = runtime
        .drive_turn_frames(
            TurnInput::text("start the admitted drive chain"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("admitted drive chain succeeds");
    handler.close().await.expect("close the scope's handler");
    assert_eq!(run.turns.len(), 2);
    // ADR 0069: one acceptance admitted this run, and it admitted exactly the
    // physical turn it was accepted for. The follow-on frames of the same run
    // were never separately admitted, so they must not restate the identity.
    assert!(
        run.turns[0].turn_input_acceptance.is_some(),
        "the admitted turn carries the acceptance it was admitted under"
    );
    assert!(
        run.turns[1].turn_input_acceptance.is_none(),
        "a follow-on frame of the same run was not separately admitted"
    );
    assert_eq!(
        store.load_session_count(),
        0,
        "the admitted root and its follow-on must not hydrate an unchanged graph"
    );
    // The admission's pending-follow-on probe answers from the head, the
    // root checks its epoch against it, and the follow-on rechecks it.
    assert_eq!(
        store.load_session_head_meta_count(),
        3,
        "the admitted drive probes its follow-on, checks its epoch and rechecks head freshness \
         for the follow-on"
    );
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root").clone(),
            TurnId::from("reacquired-turn").clone(),
        ))
        .await
        .expect("open the scope's handler");
    for node in &run.turns[0].state.session_graph.nodes {
        assert!(
            run.turns[1]
                .state
                .session_graph
                .find_node(&node.node_id)
                .is_some(),
            "the skip path lost committed graph node {}",
            node.node_id
        );
    }

    runtime
        .drive_turn(
            TurnInput::text("turn in the next drive"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("turn in the next drive succeeds");
    handler.close().await.expect("close the scope's handler");
    assert_eq!(
        store.load_session_count(),
        0,
        "reacquiring an unchanged durable head must not hydrate its graph"
    );
    assert_eq!(
        store.load_session_head_meta_count(),
        5,
        "the next admitted drive probes its follow-on and checks the durable head once more"
    );
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn frame_switch_limit_commits_terminal_error_and_settles_claim() {
    let double = kernel_double(SEED + 15, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let switch_count = lash_core::runtime::logical_turn::MAX_AGENT_FRAME_SWITCHES;
    let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_call_index = Arc::clone(&call_index);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let call_index = Arc::clone(&captured_call_index);
            async move {
                let index = call_index.fetch_add(1, Ordering::SeqCst);
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::ToolCall {
                        call_id: format!("switch-{index}"),
                        tool_name: format!("terminal_tool_{index}"),
                        input_json: "{}".to_string(),
                        replay: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let controls = (0..switch_count)
        .map(|index| lash_core::ToolControl::SwitchAgentFrame {
            frame_key: lash_core::FrameKey::from_caller_material(&format!("bounded-frame-{index}"))
                .expect("non-empty caller material"),
            initial_nodes: Vec::new(),
            task: Some(format!("continue bounded chain {index}")),
        })
        .collect();
    let store = double_unbound_recording_store(&double).await;
    let runtime_store: Arc<dyn lash_core::store::RuntimeStore> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(TerminalControlTool { controls }),
        transport,
        test_host_config(&backend),
        runtime_store,
    )
    .await;
    let inbound = enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "start bounded chain",
    )
    .await;
    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from("root").clone(),
            TurnId::from("bounded-frame-chain").clone(),
        ))
        .await
        .expect("open the scope's handler");
    let terminal = runtime
        .drive_one_admitted_queued_root(TurnOptions::new(
            CancellationToken::new(),
            handler.scoped(),
        ))
        .await
        .expect("bounded chain terminalizes")
        .ran()
        .expect("bounded chain returns terminal turn");
    handler.close().await.expect("close the scope's handler");
    assert!(matches!(
        terminal.outcome,
        TurnOutcome::Stopped(TurnStop::RuntimeError)
    ));
    assert!(
        terminal
            .errors
            .iter()
            .any(|issue| { issue.message.contains("exceeded the limit of") })
    );
    assert_eq!(call_index.load(Ordering::SeqCst), switch_count);
    assert!(
        lash_core::store::QueuedWorkStore::list_queued_work(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("queue after bounded chain")
        .is_empty()
    );
    let inputs = lash_core::store::TurnInputStore::list_pending_turn_inputs(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("inputs after bounded chain");
    assert!(
        inputs
            .iter()
            .all(|input| input.input.input_id != inbound.input_id)
    );
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn frame_switch_limit_capture_abort_abandons_prompt_claim_before_returning_diagnostic()
 {
    let double = kernel_double(SEED + 16, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let switch_count = lash_core::runtime::logical_turn::MAX_AGENT_FRAME_SWITCHES;
    let executor = Arc::new(FailingCaptureExecutor {
        dirty: AtomicBool::new(false),
        fail_capture: AtomicBool::new(false),
        snapshot: std::sync::Mutex::new(Vec::new()),
        restored: std::sync::Mutex::new(Vec::new()),
    });
    let protocol: Arc<dyn lash_core::plugin::ProtocolSessionPlugin> =
        Arc::new(RestoreExecutorFromRuntimeState {
            executor: Arc::clone(&executor),
        });
    let code_executor: Arc<dyn lash_core::plugin::CodeExecutorPlugin> = executor.clone();
    let protocol_factory = lash_core::testing::test_standard_protocol_factory_with_runtime_state(
        protocol,
        Some(code_executor),
    );
    let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_call_index = Arc::clone(&call_index);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let call_index = Arc::clone(&captured_call_index);
            async move {
                let index = call_index.fetch_add(1, Ordering::SeqCst);
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::ToolCall {
                        call_id: format!("switch-{index}"),
                        tool_name: format!("terminal_tool_{index}"),
                        input_json: "{}".to_string(),
                        replay: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let controls = (0..switch_count)
        .map(|index| lash_core::ToolControl::SwitchAgentFrame {
            frame_key: lash_core::FrameKey::from_caller_material(&format!(
                "capture-abort-frame-{index}"
            ))
            .expect("non-empty caller material"),
            initial_nodes: Vec::new(),
            task: Some(format!("continue capture-abort chain {index}")),
        })
        .collect();
    let store = double_unbound_recording_store(&double).await;
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![protocol_factory],
        Arc::new(TerminalControlTool { controls }),
        transport,
        test_host_config(&backend),
        store.clone() as Arc<dyn lash_core::RuntimeStore>,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(FailCaptureAfterCommittedTurns {
        executor,
        committed_turns: AtomicUsize::new(0),
        fail_after: switch_count,
    }));
    let inbound = enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "start capture-abort chain",
    )
    .await;
    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from("root").clone(),
            TurnId::from("bounded-frame-capture-abort").clone(),
        ))
        .await
        .expect("open the scope's handler");
    let committed = runtime
        .drive_one_admitted_queued_root(TurnOptions::new(
            CancellationToken::new(),
            handler.scoped(),
        ))
        .await
        .expect("the engine returns the last committed frame")
        .ran()
        .expect("the frame switch committed before capture failed");
    handler.close().await.expect("close the scope's handler");
    assert!(matches!(
        committed.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    let pending = lash_core::store::SessionCommitStore::load_session_head_meta(
        store.as_ref(),
        &lash_core::SessionId::from("root"),
    )
    .await
    .expect("load the head")
    .expect("head")
    .pending_follow_on
    .expect("the failed follow-on remains owed");
    assert_eq!(pending.physical_index(), switch_count as u64);
    assert!(
        lash_core::store::QueuedWorkStore::list_queued_work(
            store.as_ref(),
            &SessionId::from("root"),
        )
        .await
        .expect("list queued work")
        .is_empty(),
        "a frame handoff is never a queue row"
    );
    assert_eq!(
        pending.root_turn_id().as_str(),
        inbound.input_id.as_str(),
        "only the admitted input's follow-on remains owed"
    );
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn leading_session_command_drains_before_queued_turn() {
    let double = kernel_double(
        SEED + 17,
        lash_restate_test::ServerConfig::default().time(lash_restate_test::TimeMode::Manual),
    )
    .await;
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "queued answer".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let clock = double.test_clock();
    let (mut runtime, store) =
        standard_runtime_with_transport_and_double_queue_store(&double, transport).await;
    let command = enqueue_session_command(
        store.as_ref(),
        &SessionId::from("root"),
        "refresh before turn",
    )
    .await;
    clock.advance(1);
    let turn = enqueue_idle_turn_input(store.as_ref(), &SessionId::from("root"), "user turn").await;
    let turn_events = RecordingTurnEvents::default();
    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from("root").clone(),
            TurnId::from("command-before-turn-drain").clone(),
        ))
        .await
        .expect("open the scope's handler");
    let drained = runtime
        .drive_one_admitted_queued_root(
            TurnOptions::new(CancellationToken::new(), handler.scoped())
                .with_turn_events(&turn_events),
        )
        .await
        .expect("queued drain succeeds")
        .ran()
        .expect("queued turn runs after command");
    handler.close().await.expect("close the scope's handler");

    assert_eq!(drained.assistant_output.safe_text, "queued answer");
    assert!(
        lash_core::store::QueuedWorkStore::list_queued_work(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("list queue after command plus turn")
        .is_empty(),
        "command `{}` and turn input `{}` should both be completed",
        command.batch_id,
        turn.input_id
    );
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn idle_ordering_read_is_independent_of_pending_command_depth() {
    let double = kernel_double(
        SEED + 18,
        lash_restate_test::ServerConfig::default().time(lash_restate_test::TimeMode::Manual),
    )
    .await;
    for backlog_depth in [1, 256] {
        let transport = mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: format!("answer after {backlog_depth} commands"),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        }]);
        let clock = double.test_clock();
        let (mut runtime, store) =
            standard_runtime_with_transport_and_double_queue_store(&double, transport).await;
        for index in 0..backlog_depth {
            enqueue_session_command(
                store.as_ref(),
                &SessionId::from("root"),
                &format!("depth-invariance command {index}"),
            )
            .await;
        }
        clock.advance(1);
        enqueue_idle_turn_input(
            store.as_ref(),
            &SessionId::from("root"),
            "user turn after commands",
        )
        .await;
        let handler = double
            .open_handler(AdmittedScope::queue_drain(
                SessionId::from("root").clone(),
                TurnId::from(format!("depth-invariance-{backlog_depth}")).clone(),
            ))
            .await
            .expect("open the scope's handler");
        let drained = runtime
            .drive_one_admitted_queued_root(TurnOptions::new(
                CancellationToken::new(),
                handler.scoped(),
            ))
            .await
            .expect("depth-invariance drain succeeds")
            .ran()
            .expect("queued turn runs after commands");
        handler.close().await.expect("close the scope's handler");

        assert_eq!(
            drained.assistant_output.safe_text,
            format!("answer after {backlog_depth} commands")
        );
        assert_eq!(
            store.list_queued_work_count(),
            1,
            "engine admission reads the queue once regardless of command depth {backlog_depth}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn later_session_command_drains_before_earlier_queued_turn() {
    let double = kernel_double(
        SEED + 19,
        lash_restate_test::ServerConfig::default().time(lash_restate_test::TimeMode::Manual),
    )
    .await;
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "first turn answer".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let clock = double.test_clock();
    let (mut runtime, store) =
        standard_runtime_with_transport_and_double_queue_store(&double, transport).await;
    let turn =
        enqueue_idle_turn_input(store.as_ref(), &SessionId::from("root"), "first user turn").await;
    clock.advance(1);
    let command = enqueue_session_command(
        store.as_ref(),
        &SessionId::from("root"),
        "refresh admitted after input",
    )
    .await;
    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from("root").clone(),
            TurnId::from("later-command-before-turn-drain").clone(),
        ))
        .await
        .expect("open the scope's handler");
    let drained = runtime
        .drive_one_admitted_queued_root(TurnOptions::new(
            CancellationToken::new(),
            handler.scoped(),
        ))
        .await
        .expect("queued turn drain succeeds")
        .ran()
        .expect("first queued turn runs");
    handler.close().await.expect("close the scope's handler");

    assert_eq!(drained.assistant_output.safe_text, "first turn answer");
    assert!(
        lash_core::store::QueuedWorkStore::list_queued_work(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("list queue after first turn")
        .is_empty(),
        "later command `{}` must drain before turn `{}` runs",
        command.batch_id,
        turn.input_id
    );
    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from("root").clone(),
            TurnId::from("later-command-drain").clone(),
        ))
        .await
        .expect("open the scope's handler");
    let command_only = runtime
        .drive_one_admitted_queued_root(TurnOptions::new(
            CancellationToken::new(),
            handler.scoped(),
        ))
        .await
        .expect("later command drain succeeds")
        .ran();
    handler.close().await.expect("close the scope's handler");
    assert!(command_only.is_none());
    assert!(
        lash_core::store::QueuedWorkStore::list_queued_work(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("list queue after later command")
        .is_empty()
    );
}

// Boundary: Runtime Scenarios own the idle queue claim and completion
// invariant. This full runtime test stays here because it verifies the
// app-facing queued-work turn event, prompt projection, and blank-history
// suppression produced by `drive_next_queued_root`.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn pending_process_wake_drains_into_idle_queued_turn_as_turn_event() {
    let double = kernel_double(SEED + 20, lash_restate_test::ServerConfig::default()).await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |req| {
            let captured_requests = Arc::clone(&captured_requests);
            async move {
                captured_requests.lock_recover().push(req);
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "saw event".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let (mut runtime, store) =
        standard_runtime_with_transport_and_double_queue_store(&double, transport).await;
    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    let target_scope = lash_core::SessionScope::new("root");
    let process_caused_by = lash_core::CausalRef::SessionNode {
        session_id: SessionId::from("root"),
        node_id: "trigger:button".to_string(),
    };
    let registered = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::ProcessProvenance::session(target_scope.clone())
                    .with_caused_by(Some(process_caused_by.clone())),
                lash_core::Lifetime::Detached,
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(target_scope.session_id.clone())),
        )
        .await
        .expect("register wake process");
    let wake = append_process_wake_to_queue(
        registry.as_ref(),
        store.as_ref(),
        &registered.id,
        lash_core::ProcessEventAppendRequest::new(
            "process.wake",
            json!({
                "text": "deploy complete",
                "value": {
                    "status": "deploy complete"
                }
            }),
        ),
    )
    .await;

    let turn_events = RecordingTurnEvents::default();
    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from("root").clone(),
            TurnId::from("queued-work-started-turn").clone(),
        ))
        .await
        .expect("open the scope's handler");
    runtime
        .drive_one_admitted_queued_root(
            TurnOptions::new(CancellationToken::new(), handler.scoped())
                .with_turn_events(&turn_events),
        )
        .await
        .expect("turn")
        .ran()
        .expect("queued turn");
    handler.close().await.expect("close the scope's handler");

    let events = turn_events.snapshot();
    let lash_core::TurnEvent::TurnStarted { turn_id } = &events
        .first()
        .expect("queued turn emitted no activity")
        .event
    else {
        panic!("queued turn must begin with TurnStarted");
    };
    assert_eq!(turn_id, "drive-run:queued-work-started-turn#0");
    let queued_started = events
        .iter()
        .position(|activity| {
            matches!(
                &activity.event,
                lash_core::TurnEvent::QueuedWorkStarted { .. }
            )
        })
        .expect("queued work started event");
    assert_eq!(queued_started, 1, "claim facts must follow turn identity");
    let model_started = events
        .iter()
        .position(|activity| {
            matches!(
                &activity.event,
                lash_core::TurnEvent::ModelRequestStarted { .. }
            )
        })
        .expect("model request started event");
    assert!(
        queued_started < model_started,
        "queued work should be announced before model output starts"
    );
    let lash_core::TurnEvent::QueuedWorkStarted {
        boundary,
        batch_ids,
        causes,
    } = &events[queued_started].event
    else {
        panic!("expected queued work started event");
    };
    assert_eq!(
        *boundary,
        lash_core::testing::runtime_internals::AdmissionBoundary::Idle
    );
    assert_eq!(batch_ids.len(), 1);
    assert!(causes.iter().any(|cause| {
        cause.event_type == "process.wake"
            && cause.id == wake.wake_id
            && cause.text.contains("deploy complete")
    }));

    let requests = {
        let guard = requests.lock_recover();
        guard.clone()
    };
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    let message_text = |message: &lash_core::llm::types::LlmMessage| {
        message
            .blocks
            .iter()
            .filter_map(|block| match block {
                lash_core::llm::types::LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let turn_event_user_messages = request
        .messages
        .iter()
        .filter(|message| {
            message.role == lash_core::llm::types::LlmRole::User
                && message_text(message).contains("=== TURN EVENTS ===")
        })
        .collect::<Vec<_>>();
    assert_eq!(turn_event_user_messages.len(), 1);
    let turn_event_text = message_text(turn_event_user_messages[0]);
    assert!(turn_event_text.contains("Background process wake"));
    assert!(turn_event_text.contains("deploy complete"));
    assert!(request.messages.iter().all(|message| {
        message.role != lash_core::llm::types::LlmRole::System
            || !message_text(message).contains("deploy complete")
    }));
    assert!(request.messages.iter().all(|message| {
        message.role != lash_core::llm::types::LlmRole::User || !message.is_blank()
    }));
    assert!(
        active_conversation_messages(runtime.state())
            .iter()
            .all(|message| {
                !(message.role == lash_core::MessageRole::User
                    && message
                        .parts
                        .iter()
                        .all(|part| part.content().trim().is_empty()))
            }),
        "empty wake turns must not synthesize blank user history"
    );
    assert!(
        lash_core::store::QueuedWorkStore::list_queued_work(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("queued work after commit")
        .is_empty()
    );
}

#[derive(Clone)]
pub(super) struct CountingEchoTool {
    pub(super) executions: Arc<AtomicUsize>,
}

#[derive(Clone)]
pub(super) struct CancellationGatedTurnEvents {
    events: RecordingTurnEvents,
    cancellation: CancellationToken,
    entered: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

impl CancellationGatedTurnEvents {
    pub(super) fn new(
        cancellation: CancellationToken,
    ) -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        (
            Self {
                events: RecordingTurnEvents::default(),
                cancellation,
                entered: Arc::new(Mutex::new(Some(entered_tx))),
            },
            entered_rx,
        )
    }

    pub(super) fn snapshot(&self) -> Vec<TurnActivity> {
        self.events.snapshot()
    }
}

#[async_trait::async_trait]
impl lash_core::facade_support::TurnActivitySink for CancellationGatedTurnEvents {
    async fn emit(&self, activity: TurnActivity) {
        if matches!(
            &activity.event,
            TurnEvent::AssistantProseDelta { text, .. }
                if text.as_ref() == "drained before effect abort"
        ) {
            if let Some(entered) = self.entered.lock_recover().take() {
                let _ = entered.send(());
            }
            self.cancellation.cancelled().await;
        }
        self.events.events.lock_recover().push(activity);
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for CountingEchoTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        EchoTool.tool_manifests()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        EchoTool.resolve_contract(name)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        EchoTool.execute(call).await
    }
}
