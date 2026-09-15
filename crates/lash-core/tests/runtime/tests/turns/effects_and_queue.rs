use super::*;

#[tokio::test]
pub(super) async fn long_turn_keeps_claims_live_across_session_lease_renewals() {
    // A tiny TTL keeps the test sub-second: the session execution lease renews
    // every `renew_interval` and keeps its generation live, so the queued-work
    // claim pinned to that generation survives the stalled provider call by
    // construction.
    let lease_ttl = std::time::Duration::from_millis(120);
    let provider_stall = std::time::Duration::from_millis(500);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stall_calls = Arc::clone(&calls);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_request| {
            let stall_calls = Arc::clone(&stall_calls);
            async move {
                // Call 0 leaves the turn at a checkpoint that claims the wake;
                // the claimed wake is injected into call 1, and stalling there
                // pushes the live claim past its TTL before the next checkpoint.
                if stall_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                    tokio::time::sleep(provider_stall).await;
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "stalled turn response".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();

    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let mut config = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_lease_timings(
        lash_core::facade_support::LeaseTimings::from_ttl(lease_ttl).expect("valid lease timings"),
    );
    config.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(transport.clone().into_handle()),
    );
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        lash_core::facade_support::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;

    // The wake batch is the queued work the turn claims mid-flight at an
    // active-turn checkpoint.
    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    let target_scope = lash_core::SessionScope::new("root");
    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                "stalled-turn-wake",
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::session(target_scope.clone()),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(target_scope.session_id.clone())),
        )
        .await
        .expect("register wake process");
    let wake = append_process_wake_to_queue(
        registry.as_ref(),
        store.as_ref(),
        &ProcessId::from("stalled-turn-wake"),
        lash_core::ProcessEventAppendRequest::new(
            "process.wake",
            json!({
                "text": "queued work claimed mid turn",
                "value": {
                    "status": "queued work claimed mid turn"
                }
            }),
        ),
    )
    .await;

    // Correct behavior: the turn's claim stays live under its session-lease
    // generation and commits, so the wake is completed exactly once. The second
    // checkpoint re-runs `claim_ready_queued_work` under the same live
    // generation and cannot re-steal the turn's own rows.
    let turn = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        runtime.run_turn_assembled(
            TurnInput::text("long running user turn"),
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("long-turn-queued-work-claim"),
            ),
        ),
    )
    .await
    .expect("stalled turn should finish")
    .expect("stalled turn must commit without losing its queued-work claim");

    assert_eq!(turn.assistant_output.safe_text, "stalled turn response");
    assert!(
        lash_core::store::QueuedWorkStore::list_queued_work(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("queued work after stalled turn")
        .is_empty(),
        "wake `{}` should be completed exactly once by the committing turn",
        wake.wake_id
    );
    assert!(
        runtime
            .stream_next_queued_work(TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("after-long-turn-queued-work-claim")
                ),
            ))
            .await
            .expect("post-turn queue check should succeed")
            .ran()
            .is_none(),
        "the committed wake `{}` must not replay after the turn",
        wake.wake_id
    );
}

// Boundary: command ordering tests stay in `turns.rs` when they assert public
// queued-work scheduler behavior across `stream_next_queued_work` calls,
// provider execution, and the API distinction between "ran a turn" and
// command-only `None`. Runtime Scenarios own the store-level command-before
// turn-work gate and command-only drain invariants.
#[tokio::test]
pub(super) async fn fig1123_queued_frame_switch_finishes_follow_on_before_next_queued_turn() {
    let store = Arc::new(RecordingStore::default());
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
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
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
        test_host_config(),
        runtime_store,
    )
    .await;
    let first = enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "first queued turn",
    )
    .await;

    let first_result = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("queued-frame-chain"),
            ),
        ))
        .await
        .expect("queued frame chain succeeds")
        .ran()
        .expect("queued frame chain returns its terminal turn");

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
        .state
        .current_frame_node_id
        .as_deref()
        .expect("follow-on frame is active");
    assert_eq!(requests_after_follow[1].scope.agent_frame_id, follow_frame);

    let second_result = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("second-queued-after-frame-chain"),
            ),
        ))
        .await
        .expect("second queued turn succeeds")
        .ran()
        .expect("second queued turn runs after the frame chain");

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

#[tokio::test]
pub(super) async fn fig1123_committed_frame_handoff_survives_before_inline_claim_and_pump_recovers_it()
 {
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
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
                            call_id: "switch-call".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    1 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "recovered follow-on".to_string(),
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
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(TerminalControlTool {
            controls: vec![lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("recovery-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("recover this handoff".to_string()),
            }],
        }),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    let inbound =
        enqueue_idle_turn_input(store.as_ref(), &SessionId::from("root"), "start switch").await;
    store.fail_next_exact_queue_claim();

    let first = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("handoff-crash-window"),
            ),
        ))
        .await
        .expect("the committed frame switch remains a successful public call")
        .ran()
        .expect("the committed frame switch is returned");
    assert!(matches!(
        first.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    assert!(first.errors.iter().any(|issue| {
        issue.code.as_deref() == Some("store_commit_failed") && issue.retryable == Some(false)
    }));
    assert!(matches!(
        runtime.resident_session.validity(),
        ResidentSessionState::Invalidated { .. }
    ));

    let inputs = lash_core::store::TurnInputStore::list_pending_turn_inputs(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("list inbound input after switch commit");
    assert!(
        inputs
            .iter()
            .all(|input| input.input.input_id != inbound.input_id)
    );
    let queued = lash_core::store::QueuedWorkStore::list_pending_queued_work(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("list committed handoff");
    assert_eq!(queued.len(), 1);
    let expected_frame_id = lash_core::session_graph::frame_node_id(
        &SessionId::from("root"),
        lash_core::FrameKey::from_caller_material("recovery-frame")
            .expect("non-empty caller material")
            .as_str(),
    );
    assert!(matches!(
        &queued[0].items[0].payload,
        lash_core::testing::runtime_internals::QueuedWorkPayload::AgentFrameTask { frame_id, task, .. }
            if frame_id == &expected_frame_id && task == "recover this handoff"
    ));

    let recovered = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("handoff-pump-recovery"),
            ),
        ))
        .await
        .expect("pump recovery succeeds")
        .ran()
        .expect("pump runs durable handoff");
    assert_eq!(recovered.assistant_output.safe_text, "recovered follow-on");
    assert!(
        lash_core::store::QueuedWorkStore::list_queued_work(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("queue after recovery")
        .is_empty()
    );
}

#[tokio::test]
pub(super) async fn mid_chain_cancellation_commits_one_cancelled_terminal_and_settles_handoff() {
    const SESSION_ID: &str = "mid-chain-cancellation";

    let store = Arc::new(RecordingStore::default());
    let captured_store = Arc::clone(&store);
    let cancel = CancellationToken::new();
    let cancel_after_switch = cancel.clone();
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let store = Arc::clone(&captured_store);
            let cancel = cancel_after_switch.clone();
            async move {
                store.set_claim_after_lease_validation_hook(Arc::new(move || cancel.cancel()));
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
            }
        })
        .build();
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let mut runtime = TestRuntime::new(transport)
        .tools(Arc::new(TerminalControlTool {
            controls: vec![lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("cancelled-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("cancel before running".to_string()),
            }],
        }))
        .host(test_host_config())
        .store(runtime_store)
        .with_session_id(SESSION_ID)
        .build()
        .await;
    enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from(SESSION_ID),
        "start cancellable switch",
    )
    .await;

    let terminal = runtime
        .stream_next_queued_work(TurnOptions::new(
            cancel,
            named_turn_scope(
                &SessionId::from(SESSION_ID),
                &TurnId::from("mid-chain-cancel"),
            ),
        ))
        .await
        .expect("cancelled chain assembles")
        .ran()
        .expect("cancelled terminal turn");
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
}

#[tokio::test]
pub(super) async fn claimed_normalization_failure_commits_and_settles_input() {
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
        standard_runtime_with_transport_and_queue_store(mock_provider(Vec::new())).await;
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

    let terminal = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("invalid-claimed-input"),
            ),
        ))
        .await
        .expect("invalid input assembles")
        .ran()
        .expect("invalid terminal turn");
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

#[tokio::test]
pub(super) async fn claimed_plugin_abort_commits_and_settles_input() {
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
                tool_result_projector: None,
                runtime_event: None,
                external_registrar: None,
            }))
        }),
    });
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![plugin],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(),
        runtime_store,
    )
    .await;
    let inbound =
        enqueue_idle_turn_input(store.as_ref(), &SessionId::from("root"), "abort this input").await;

    let terminal = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("claimed-plugin-abort"),
            ),
        ))
        .await
        .expect("plugin abort assembles")
        .ran()
        .expect("plugin abort terminal turn");
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

#[tokio::test]
pub(super) async fn stream_turn_tool_put_is_bound_to_the_turn_id() {
    const TURN_ID: &str = "attachment-owner-stream-turn";
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(AttachmentPutTool),
        attachment_put_transport(),
        test_host_config(),
        runtime_store,
    )
    .await;

    runtime
        .stream_turn(
            TurnInput::text("store an attachment"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(&SessionId::from("root"), &TurnId::from(TURN_ID)),
            ),
        )
        .await
        .expect("stream turn succeeds");

    assert_turn_owned_attachment(store.as_ref(), &TurnId::from(TURN_ID));
}

#[tokio::test]
pub(super) async fn stream_prepared_turn_tool_put_is_bound_to_the_turn_id() {
    const TURN_ID: &str = "attachment-owner-prepared-turn";
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(AttachmentPutTool),
        attachment_put_transport(),
        test_host_config(),
        runtime_store,
    )
    .await;
    let messages = lash_core::facade_support::MessageSequence::from_owned(vec![Message {
        id: "prepared-attachment-user".to_string(),
        role: MessageRole::User,
        parts: vec![Part::text(
            "prepared-attachment-user.p0".to_string(),
            "store an attachment".to_string(),
            None,
        )]
        .into(),
        origin: None,
    }]);

    runtime
        .stream_prepared_turn(
            messages,
            None,
            None,
            None,
            lash_core::TurnContext::default(),
            Vec::new(),
            TurnId::from(TURN_ID.to_string()),
            1,
            &NoopEventSink,
            &NoopTurnActivitySink,
            named_turn_scope(&SessionId::from("root"), &TurnId::from(TURN_ID)),
            CancellationToken::new(),
            None,
            None,
        )
        .await
        .expect("prepared stream turn succeeds");

    assert_turn_owned_attachment(store.as_ref(), &TurnId::from(TURN_ID));
}

#[tokio::test]
pub(super) async fn stream_prepared_turn_follows_agent_frame_switch() {
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
                            call_id: "prepared-switch".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    1 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "prepared follow-on complete".to_string(),
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
    let mut runtime = runtime_with_plugins_and_tools(
        Vec::new(),
        Arc::new(TerminalControlTool {
            controls: vec![lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("prepared-follow-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("finish prepared follow-on".to_string()),
            }],
        }),
        transport,
    )
    .await;
    let messages = lash_core::facade_support::MessageSequence::from_owned(vec![Message {
        id: "prepared-user".to_string(),
        role: MessageRole::User,
        parts: vec![Part::text(
            "prepared-user.p0".to_string(),
            "prepared input".to_string(),
            None,
        )]
        .into(),
        origin: None,
    }]);

    let terminal = runtime
        .stream_prepared_turn(
            messages,
            None,
            None,
            None,
            lash_core::TurnContext::default(),
            Vec::new(),
            TurnId::from("prepared-chain".to_string()),
            1,
            &NoopEventSink,
            &NoopTurnActivitySink,
            named_turn_scope(&SessionId::from("root"), &TurnId::from("prepared-chain")),
            CancellationToken::new(),
            None,
            None,
        )
        .await
        .expect("prepared logical turn succeeds");
    assert_eq!(
        terminal.assistant_output.safe_text,
        "prepared follow-on complete"
    );
    assert_eq!(call_index.load(Ordering::SeqCst), 2);
}

#[tokio::test]
pub(super) async fn process_scoped_agent_frame_follow_on_uses_distinct_cancel_peek_keys() {
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
                            call_id: "process-follow-on-switch".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    1 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "process follow-on complete".to_string(),
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
    let recorder = super::effect::RecordingEffectController::default()
        .with_controller_owned_replay()
        .with_strict_replay_by_address()
        .with_local_llm_execution();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(TerminalControlTool {
            controls: vec![lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("process-follow-on-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("finish the process-backed follow-on".to_string()),
            }],
        }),
        transport,
        lash_core::facade_support::EmbeddedRuntimeHost::new(
            super::effect::runtime_host_config_with_native_controller(Arc::new(recorder.clone())),
        ),
        Arc::new(RecordingStore::default()),
    )
    .await;
    let process_id = ProcessId::from("process:subagent:physical-follow-on");
    let root_turn_id = TurnId::from(process_id.as_str());
    let follow_on_turn_id = TurnId::from(format!("{root_turn_id}:agent-frame:1"));

    let run = runtime
        .stream_turn_with_agent_frames(
            TurnInput::text("start the process-backed frame chain"),
            TurnOptions::new(
                CancellationToken::new(),
                lash_core::ScopedEffectController::shared(
                    Arc::new(recorder.clone()),
                    lash_core::ExecutionScope::process(&process_id),
                )
                .expect("process scope"),
            ),
        )
        .await
        .expect("process-backed agent-frame run succeeds");

    assert_eq!(run.turns.len(), 2);
    assert_eq!(
        run.final_turn()
            .expect("final process-backed frame")
            .assistant_output
            .safe_text,
        "process follow-on complete"
    );
    assert_eq!(call_index.load(Ordering::SeqCst), 2);
    assert!(
        recorder.has_kind_for_turn(lash_core::RuntimeEffectKind::PeekAwaitEvent, &root_turn_id)
    );
    assert!(recorder.has_kind_for_turn(
        lash_core::RuntimeEffectKind::PeekAwaitEvent,
        &follow_on_turn_id
    ));
}

#[tokio::test]
pub(super) async fn turn_finalized_borrowed_append_lane_loss_keeps_typed_issue() {
    let call_index = Arc::new(AtomicUsize::new(0));
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
                            call_id: "finalized-lapsed-switch".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    1 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "final turn still commits".to_string(),
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
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store;
    let host_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let mut config = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
    config.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(transport.clone().into_handle()),
    );
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![turn_finalized_borrowed_append_plugin()],
        Arc::new(TerminalControlTool {
            controls: vec![lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material(
                    "finalized-lapsed-follow-frame",
                )
                .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("exercise the retained finalize observer".to_string()),
            }],
        }),
        transport,
        lash_core::facade_support::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(ExpireLeaseAtSecondTurnFinalizedHook::new(
        Arc::clone(&clock),
    )));

    let run = runtime
        .stream_turn_with_agent_frames(
            TurnInput::text("start finalized borrowed append probe"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("finalized-lapsed-borrow"),
                ),
            ),
        )
        .await
        .expect("the final current-head commit survives the observer's borrowed-lane failure");

    assert_eq!(run.turns.len(), 2);
    let issue = run.turns[1]
        .errors
        .iter()
        .find(|issue| {
            issue.code.as_deref()
                == Some(lash_core::RuntimeErrorCode::SessionExecutionLeaseLost.as_str())
        })
        .unwrap_or_else(|| {
            panic!(
                "TurnFinalized must preserve typed lane loss: {:?}",
                run.turns[1].errors
            )
        });
    assert_eq!(issue.kind, "runtime");
    assert_eq!(issue.retryable, Some(false));
    assert_eq!(call_index.load(Ordering::SeqCst), 2);
}

#[tokio::test]
pub(super) async fn retained_turn_graph_service_does_not_extend_the_execution_lane() {
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|_| async {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "retained-service-switch".to_string(),
                    tool_name: "terminal_tool_0".to_string(),
                    input_json: "{}".to_string(),
                    replay: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build();
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let retained = Arc::new(std::sync::Mutex::new(None));
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![retain_turn_persisted_graph_service_plugin(Arc::clone(
            &retained,
        ))],
        Arc::new(TerminalControlTool {
            controls: vec![lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material(
                    "retained-service-follow-frame",
                )
                .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("leave this follow-on queued".to_string()),
            }],
        }),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "stash the graph service",
    )
    .await;

    let output = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(&SessionId::from("root"), &TurnId::from("retained-service")),
        ))
        .await
        .expect("queued switch succeeds")
        .ran()
        .expect("queued switch returns a turn");
    assert!(matches!(
        output.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));

    let graph = retained
        .lock_recover()
        .clone()
        .expect("TurnPersisted retained its graph service");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if lash_core::store::SessionExecutionLeaseStore::get_session_execution_lease(
                store.as_ref(),
                &SessionId::from("root"),
            )
            .await
            .expect("read released lane")
            .lease
            .is_none()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the unique turn-driver guard releases while the service is retained");

    let error = graph
        .append_session_nodes(
            &SessionId::from("root"),
            lash_core::AppendSessionNodesRequest {
                operation_id: "stale-retained-service".to_string(),
                nodes: vec![lash_core::SessionAppendNode::plugin(
                    "test.stale-retained-service",
                    serde_json::json!({"attempted": true}),
                )],
                requires_ancestor_node_id: None,
            },
        )
        .await
        .expect_err("a retained service can only present its stale borrowed fence");
    assert!(matches!(
        error,
        lash_core::PluginError::SessionExecutionLeaseLost { ref session_id }
            if session_id == "root"
    ));
}

#[tokio::test]
pub(super) async fn durable_queued_lapsed_lane_stays_loud_at_agent_frame_handoff() {
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
                            call_id: "queued-lapsed-switch".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    1 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "must not silently reacquire".to_string(),
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
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let host_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let borrowed_append_attempted = Arc::new(AtomicBool::new(false));
    let borrowed_append_error = Arc::new(std::sync::Mutex::new(None));
    let mut config = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
    config.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(transport.clone().into_handle()),
    );
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![turn_persisted_borrowed_append_plugin(
            Arc::clone(&borrowed_append_attempted),
            Arc::clone(&borrowed_append_error),
        )],
        Arc::new(TerminalControlTool {
            controls: vec![lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("queued-lapsed-follow-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("must retain the loud lease failure".to_string()),
            }],
        }),
        transport,
        lash_core::facade_support::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(ExpireLeaseAfterRetainedCommit::new(Arc::clone(
        &clock,
    ))));
    enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "start lapsed queued handoff",
    )
    .await;

    let output = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("queued-lapsed-handoff"),
            ),
        ))
        .await
        .expect("the committed switch is returned with a loud follow-on failure")
        .ran()
        .expect("queued turn should run");

    assert!(matches!(
        output.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    let issue = output
        .errors
        .iter()
        .find(|issue| {
            issue.code.as_deref()
                == Some(lash_core::RuntimeErrorCode::SessionExecutionLeaseLost.as_str())
        })
        .unwrap_or_else(|| {
            panic!(
                "the durable handoff reports the lapsed session lane: {:?}",
                output.errors
            )
        });
    assert_eq!(issue.retryable, Some(false));
    assert_eq!(
        *borrowed_append_error.lock_recover(),
        Some(std::mem::discriminant(
            &lash_core::PluginError::SessionExecutionLeaseLost {
                session_id: SessionId::from("root"),
            }
        )),
        "the plugin must receive the typed borrowed-lane failure"
    );
    assert!(
        borrowed_append_attempted.load(Ordering::SeqCst),
        "the lapsed retained lane must be presented by the borrowed nested commit"
    );
    assert_eq!(
        call_index.load(Ordering::SeqCst),
        1,
        "a lapsed retained lane must not be silently reacquired for the follow-on turn"
    );
    let pending = lash_core::store::QueuedWorkStore::list_queued_work(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("list committed handoff batch");
    assert!(
        pending
            .iter()
            .any(|batch| batch.items.iter().any(|item| matches!(
                item.payload,
                lash_core::testing::runtime_internals::QueuedWorkPayload::AgentFrameTask { .. }
            ))),
        "the loud claim failure must leave the committed handoff batch claimable"
    );
    let final_lease = lash_core::store::SessionExecutionLeaseStore::get_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("read final session lane state")
    .lease;
    assert!(
        final_lease.is_none(),
        "settling the loud durable failure must clear the expired owner row"
    );
}

#[tokio::test]
pub(super) async fn inprocess_lapsed_lane_stays_loud_after_agent_frame_handoff() {
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
                            call_id: "inprocess-lapsed-switch".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    1 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "must not reach the follow-on".to_string(),
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
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let host_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let borrowed_append_attempted = Arc::new(AtomicBool::new(false));
    let borrowed_append_error = Arc::new(std::sync::Mutex::new(None));
    let mut config = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
    config.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(transport.clone().into_handle()),
    );
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![turn_persisted_borrowed_append_plugin(
            Arc::clone(&borrowed_append_attempted),
            Arc::clone(&borrowed_append_error),
        )],
        Arc::new(TerminalControlTool {
            controls: vec![lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material(
                    "inprocess-lapsed-follow-frame",
                )
                .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("must retain the loud lease failure".to_string()),
            }],
        }),
        transport,
        lash_core::facade_support::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(ExpireLeaseAfterRetainedCommit::new(Arc::clone(
        &clock,
    ))));

    let run = runtime
        .stream_turn_with_agent_frames(
            TurnInput::text("start lapsed in-process handoff"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("inprocess-lapsed-handoff"),
                ),
            ),
        )
        .await
        .expect("the committed switch is returned with a loud follow-on failure");

    assert_eq!(run.turns.len(), 1);
    assert!(matches!(
        run.turns[0].outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    let issue = run.turns[0]
        .errors
        .iter()
        .find(|issue| {
            issue.code.as_deref()
                == Some(lash_core::RuntimeErrorCode::SessionExecutionLeaseLost.as_str())
        })
        .unwrap_or_else(|| {
            panic!(
                "the in-process follow-on reports the lapsed session lane: {:?}",
                run.turns[0].errors
            )
        });
    assert_eq!(issue.retryable, Some(false));
    assert_eq!(
        *borrowed_append_error.lock_recover(),
        Some(std::mem::discriminant(
            &lash_core::PluginError::SessionExecutionLeaseLost {
                session_id: SessionId::from("root"),
            }
        )),
        "the plugin must receive the typed borrowed-lane failure"
    );
    assert!(
        borrowed_append_attempted.load(Ordering::SeqCst),
        "the lapsed retained lane must be presented by the borrowed nested commit"
    );
    assert_eq!(
        call_index.load(Ordering::SeqCst),
        1,
        "the follow-on provider call must not start under an expired lane"
    );
    let final_lease = lash_core::store::SessionExecutionLeaseStore::get_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("read final session lane state")
    .lease;
    assert!(
        final_lease.is_none(),
        "settling the loud in-process failure must clear the expired owner row"
    );
}

#[tokio::test]
pub(super) async fn retained_lease_reuses_graph_and_reacquisition_reloads() {
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
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(TerminalControlTool {
            controls: vec![lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("resident-follow-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("continue on retained lease".to_string()),
            }],
        }),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;

    let run = runtime
        .stream_turn_with_agent_frames(
            TurnInput::text("start retained lease chain"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(&SessionId::from("root"), &TurnId::from("resident-chain")),
            ),
        )
        .await
        .expect("retained lease chain succeeds");
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
        "the initial head probe and retained-lease follow-on must not hydrate an unchanged graph"
    );
    assert_eq!(
        store.load_session_head_meta_count(),
        1,
        "the first physical turn must establish durable head freshness exactly once"
    );
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
        .stream_turn(
            TurnInput::text("turn after lease release"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(&SessionId::from("root"), &TurnId::from("reacquired-turn")),
            ),
        )
        .await
        .expect("turn after lease reacquisition succeeds");
    assert_eq!(
        store.load_session_count(),
        0,
        "reacquiring an unchanged durable head must not hydrate its graph"
    );
    assert_eq!(
        store.load_session_head_meta_count(),
        2,
        "a released and reacquired lease generation must force a durable head recheck"
    );
}

#[tokio::test]
pub(super) async fn lost_lease_and_reacquisition_force_graph_reloads() {
    let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_call_index = Arc::clone(&call_index);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let call_index = Arc::clone(&captured_call_index);
            async move {
                let index = call_index.fetch_add(1, Ordering::SeqCst);
                let response = match index {
                    0 => LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "lost-lease-switch".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    },
                    1 => LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "reacquired lease turn".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    },
                    index => panic!("unexpected provider call {index}"),
                };
                Ok(response)
            }
        })
        .build();
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let host_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let mut config = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
    config.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(transport.clone().into_handle()),
    );
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(TerminalControlTool {
            controls: vec![lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("lost-lease-follow-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("continue after retained commit".to_string()),
            }],
        }),
        transport,
        lash_core::facade_support::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(ExpireLeaseAfterRetainedCommit::new(Arc::clone(
        &clock,
    ))));

    let frame_run = runtime
        .stream_turn_with_agent_frames(
            TurnInput::text("lose the retained lease"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("lost-retained-lease"),
                ),
            ),
        )
        .await
        .expect("the committed frame must survive the follow-on lease loss");
    assert_eq!(frame_run.turns.len(), 1);
    assert!(matches!(
        frame_run.turns[0].outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    let issue = frame_run.turns[0]
        .errors
        .iter()
        .find(|issue| {
            issue.code.as_deref()
                == Some(lash_core::RuntimeErrorCode::SessionExecutionLeaseLost.as_str())
        })
        .expect("the committed frame reports the follow-on lease loss");
    assert_eq!(issue.retryable, Some(false));
    assert_eq!(
        store.load_session_count(),
        0,
        "the fenced handoff claim must reject the lost lease before a full reload"
    );
    assert_eq!(
        store.load_session_head_meta_count(),
        1,
        "the first turn must establish durable head freshness exactly once"
    );

    runtime
        .stream_turn(
            TurnInput::text("turn after lease loss"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("turn-after-lease-loss"),
                ),
            ),
        )
        .await
        .expect("turn after lease loss and reacquisition succeeds");
    assert_eq!(
        store.load_session_count(),
        1,
        "a lease acquired after loss must reload invalidated resident state exactly once"
    );
    assert_eq!(
        store.load_session_head_meta_count(),
        1,
        "the reload under the reacquired lease settles durable freshness itself \
         (FIG-1875); the rebuilt turn issues no additional bounded head probe"
    );
}

#[tokio::test]
pub(super) async fn frame_switch_limit_commits_terminal_error_and_settles_claim() {
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
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(TerminalControlTool { controls }),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    let inbound = enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "start bounded chain",
    )
    .await;

    let terminal = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("bounded-frame-chain"),
            ),
        ))
        .await
        .expect("bounded chain terminalizes")
        .ran()
        .expect("bounded chain returns terminal turn");
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

#[tokio::test]
pub(super) async fn frame_switch_limit_capture_abort_abandons_prompt_claim_before_returning_diagnostic()
 {
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
    let store = Arc::new(RecordingStore::default());
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![protocol_factory],
        Arc::new(TerminalControlTool { controls }),
        transport,
        test_host_config(),
        store.clone() as Arc<dyn lash_core::RuntimePersistence>,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(FailCaptureAfterCommittedTurns {
        executor,
        committed_turns: AtomicUsize::new(0),
        fail_after: switch_count,
    }));
    enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "start capture-abort chain",
    )
    .await;

    let committed = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("bounded-frame-capture-abort"),
            ),
        ))
        .await
        .expect("a failed terminal capture preserves the last committed frame")
        .ran()
        .expect("the last committed frame is returned");

    assert!(matches!(
        committed.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    assert!(committed.errors.iter().any(|issue| {
        issue.code.as_deref() == Some("execution_state_capture_failed")
            && issue.retryable == Some(false)
    }));
    assert_eq!(
        store.abandoned_claim_counts(),
        (1, 0),
        "the claimed handoff must pass through ordinary local-abort cleanup"
    );
    let queued = store.raw_queued_work_for_testing();
    assert_eq!(queued.len(), 1, "only the uncommitted handoff remains");
    assert!(
        queued[0].1.is_none() && !queued[0].3,
        "the remaining handoff must have no claim identity or token: {queued:?}"
    );
}

#[tokio::test]
pub(super) async fn leading_session_command_drains_before_queued_turn() {
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
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store_clock(transport, store_clock).await;
    let command = enqueue_session_command(
        store.as_ref(),
        &SessionId::from("root"),
        "refresh before turn",
    )
    .await;
    clock.advance_ms(1);
    let turn = enqueue_idle_turn_input(store.as_ref(), &SessionId::from("root"), "user turn").await;
    let turn_events = RecordingTurnEvents::default();

    let drained = runtime
        .stream_next_queued_work(
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("command-before-turn-drain"),
                ),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("queued drain succeeds")
        .ran()
        .expect("queued turn runs after command");

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

#[tokio::test]
pub(super) async fn idle_ordering_read_is_independent_of_pending_command_depth() {
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
        let clock = Arc::new(ManualClock::new(1_500));
        let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
        let (mut runtime, store) =
            standard_runtime_with_transport_and_queue_store_clock(transport, store_clock).await;
        for index in 0..backlog_depth {
            enqueue_session_command(
                store.as_ref(),
                &SessionId::from("root"),
                &format!("depth-invariance command {index}"),
            )
            .await;
        }
        clock.advance_ms(1);
        enqueue_idle_turn_input(
            store.as_ref(),
            &SessionId::from("root"),
            "user turn after commands",
        )
        .await;

        let drained = runtime
            .stream_next_queued_work(TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from(format!("depth-invariance-{backlog_depth}")),
                ),
            ))
            .await
            .expect("depth-invariance drain succeeds")
            .ran()
            .expect("queued turn runs after commands");

        assert_eq!(
            drained.assistant_output.safe_text,
            format!("answer after {backlog_depth} commands")
        );
        assert_eq!(
            store.list_pending_queued_work_count(),
            0,
            "idle ordering must not invoke the payload-hydrating full-list read at depth {backlog_depth}"
        );
    }
}

#[tokio::test]
pub(super) async fn later_session_command_does_not_jump_earlier_queued_turn() {
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
    let clock = Arc::new(ManualClock::new(2_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store_clock(transport, store_clock).await;
    let turn =
        enqueue_idle_turn_input(store.as_ref(), &SessionId::from("root"), "first user turn").await;
    clock.advance_ms(1);
    let command = enqueue_session_command(
        store.as_ref(),
        &SessionId::from("root"),
        "refresh after turn",
    )
    .await;

    let drained = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("turn-before-command-drain"),
            ),
        ))
        .await
        .expect("queued turn drain succeeds")
        .ran()
        .expect("first queued turn runs");

    assert_eq!(drained.assistant_output.safe_text, "first turn answer");
    assert_eq!(
        lash_core::store::QueuedWorkStore::list_queued_work(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("list queue after first turn")
        .iter()
        .map(|batch| batch.batch_id.as_str())
        .collect::<Vec<_>>(),
        vec![command.batch_id.as_str()],
        "later command should remain after turn `{}` runs",
        turn.input_id
    );

    let command_only = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("later-command-drain"),
            ),
        ))
        .await
        .expect("later command drain succeeds")
        .ran();
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
// suppression produced by `stream_next_queued_work`.
#[tokio::test]
pub(super) async fn pending_process_wake_drains_into_idle_queued_turn_as_turn_event() {
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
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
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
    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                "wake-proc",
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::session(target_scope.clone())
                    .with_caused_by(Some(process_caused_by.clone())),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(target_scope.session_id.clone())),
        )
        .await
        .expect("register wake process");
    let wake = append_process_wake_to_queue(
        registry.as_ref(),
        store.as_ref(),
        &ProcessId::from("wake-proc"),
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
    runtime
        .stream_next_queued_work(
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("queued-work-started-turn"),
                ),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("turn")
        .ran()
        .expect("queued turn");

    let events = turn_events.snapshot();
    let lash_core::TurnEvent::TurnStarted { turn_id } = &events
        .first()
        .expect("queued turn emitted no activity")
        .event
    else {
        panic!("queued turn must begin with TurnStarted");
    };
    assert_eq!(turn_id, "queued-work-started-turn");
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
        lash_core::testing::runtime_internals::QueuedWorkClaimBoundary::Idle
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
        active_conversation_messages(&runtime.state)
            .iter()
            .all(|message| {
                !(message.role == lash_core::MessageRole::User
                    && message
                        .parts
                        .iter()
                        .all(|part| part.content.trim().is_empty()))
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
            TurnEvent::AssistantProseDelta { text }
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

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        EchoTool.execute(call).await
    }
}
