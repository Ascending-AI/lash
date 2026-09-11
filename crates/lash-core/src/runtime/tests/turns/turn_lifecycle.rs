use super::*;

#[test]
pub(super) fn cancel_watch_test_clock_wall_clock_faces_agree() {
    let clock = CancelWatchTestClock(crate::testing::TestClock::new(1_700_000_000_123));
    let clock: &dyn crate::Clock = &clock;
    let milliseconds = clock.timestamp_ms();
    let datetime = clock.timestamp_datetime();
    let text = chrono::DateTime::parse_from_rfc3339(&clock.timestamp_rfc3339())
        .expect("clock emits RFC 3339");
    assert_eq!(datetime.timestamp_millis() as u64, milliseconds);
    assert_eq!(text.timestamp_millis() as u64, milliseconds);
}

#[derive(Debug)]
pub(super) struct ManualClock {
    epoch_ms: std::sync::atomic::AtomicU64,
}

impl ManualClock {
    pub(super) fn new(epoch_ms: u64) -> Self {
        Self {
            epoch_ms: std::sync::atomic::AtomicU64::new(epoch_ms),
        }
    }

    pub(super) fn advance_ms(&self, delta_ms: u64) {
        self.epoch_ms
            .fetch_add(delta_ms, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl crate::Clock for ManualClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let timestamp_ms = self.epoch_ms.load(std::sync::atomic::Ordering::SeqCst);
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(timestamp_ms),
        )
    }

    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

#[test]
pub(super) fn manual_clock_wall_clock_faces_agree() {
    let clock = ManualClock::new(1_700_000_000_123);
    let clock: &dyn crate::Clock = &clock;
    let milliseconds = clock.timestamp_ms();
    let datetime = clock.timestamp_datetime();
    let text = chrono::DateTime::parse_from_rfc3339(&clock.timestamp_rfc3339())
        .expect("clock emits RFC 3339");
    assert_eq!(datetime.timestamp_millis() as u64, milliseconds);
    assert_eq!(text.timestamp_millis() as u64, milliseconds);
}

#[tokio::test]
pub(super) async fn dropping_suspended_host_delivery_keeps_committed_state_adopted() {
    let post_commit_entered = Arc::new(AtomicBool::new(false));
    let plugin: Arc<dyn crate::PluginFactory> = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                tool_result_projector: None,
                runtime_event: None,
                external_registrar: Some(Arc::new(|reg| {
                    reg.turn().after(Arc::new(|_| {
                        Box::pin(async {
                            Ok(vec![crate::AfterTurnPluginDirective::from(
                                crate::PluginDirective::emit_runtime_events(vec![
                                    crate::PluginRuntimeEvent::Custom {
                                        name: "post_commit_suspend".to_string(),
                                        payload: serde_json::json!({"test": true}),
                                    },
                                ]),
                            )])
                        })
                    }));
                    Ok(())
                })),
            }))
        }),
    });
    let store = Arc::new(RecordingStore::default());
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![plugin],
        Arc::new(EmptyTools),
        mock_provider(vec![
            MockCall {
                stream_events: Vec::new(),
                response: Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "committed before delivery".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                }),
            },
            MockCall {
                stream_events: Vec::new(),
                response: Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "resident commit survived drop".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                }),
            },
        ]),
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(RecordPostCommitDelivery {
        entered: Arc::clone(&post_commit_entered),
    }));
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel(1);
    let release = Arc::new(tokio::sync::Notify::new());
    let sink = SuspendingPostCommitSink {
        adopted: Arc::clone(&post_commit_entered),
        entered: entered_tx,
        release: Arc::clone(&release),
    };

    let mut turn = Box::pin(
        runtime.stream_turn(
            TurnInput::text("commit before delivering"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("commit-before-delivery"),
                ),
            )
            .with_events(&sink),
        ),
    );
    tokio::select! {
        entered = entered_rx.recv() => assert!(entered.is_some(), "sink entered"),
        result = turn.as_mut() => panic!("turn must suspend in host delivery: {result:?}"),
    }
    let durable = crate::store::SessionCommitStore::load_session(store.as_ref())
        .await
        .expect("load committed head")
        .expect("committed session");
    assert_eq!(durable.head_revision, 1);
    drop(turn);
    assert_eq!(runtime.state.turn_index, 1);
    assert_eq!(
        *runtime.resident_session.validity(),
        ResidentSessionState::Valid
    );
    let recovered = runtime
        .run_turn_assembled(
            TurnInput::text("continue after dropped host delivery"),
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("after-dropped-host-delivery"),
            ),
        )
        .await
        .expect("the adopted resident state remains usable");
    assert_eq!(
        recovered.assistant_output.safe_text,
        "resident commit survived drop"
    );
}

#[tokio::test]
pub(super) async fn post_commit_restore_failure_is_a_diagnostic_and_forces_reload() {
    let protocol = Arc::new(FailNextProtocolRestore {
        fail_next: AtomicBool::new(false),
        restore_count: AtomicUsize::new(0),
    });
    let protocol_factory =
        crate::testing::test_standard_protocol_factory_with_runtime_state(protocol.clone(), None);
    let store = Arc::new(RecordingStore::default());
    let call_index = Arc::new(AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let call_index = Arc::clone(&call_index);
            async move {
                Ok(match call_index.fetch_add(1, Ordering::SeqCst) {
                    0 => LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "switch".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    },
                    1 => LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "resident state reloaded".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    },
                    index => panic!("unexpected provider call {index}"),
                })
            }
        })
        .build();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![protocol_factory],
        Arc::new(TerminalControlTool {
            controls: vec![crate::ToolControl::SwitchAgentFrame {
                frame_key: crate::FrameKey::from_caller_material("restore-failure-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("continue after restore".to_string()),
            }],
        }),
        transport,
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;
    protocol.fail_next.store(true, Ordering::SeqCst);

    let committed = runtime
        .run_turn_assembled(
            TurnInput::text("switch frames"),
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("post-commit-restore-failure"),
            ),
        )
        .await
        .expect("a published commit must not become a whole-turn error");
    assert!(matches!(
        committed.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    assert!(committed.errors.iter().any(|issue| {
        issue.code.as_deref() == Some("protocol_restore_session") && issue.retryable == Some(false)
    }));
    assert!(matches!(
        runtime.resident_session.validity(),
        ResidentSessionState::Invalidated { .. }
    ));
    let durable = crate::store::SessionCommitStore::load_session(store.as_ref())
        .await
        .expect("load committed frame switch")
        .expect("committed session");
    assert_eq!(durable.head_revision, 1);

    let ((refusal, reload_error, exported), capture) = super::trace_capture::capturing(|| async {
        let refusal = runtime
            .tool_state()
            .expect_err("a synchronous accessor refuses invalidated resident state");
        protocol.fail_next.store(true, Ordering::SeqCst);
        let reload_error = runtime
            .export_persisted_state()
            .await
            .expect_err("the injected protocol restore fault denies reload");
        let exported = runtime
            .export_persisted_state()
            .await
            .expect("persisted export retries reload from the durable head");
        (refusal, reload_error, exported)
    })
    .await;
    assert!(refusal.to_string().contains("durable reload is required"));
    assert_eq!(
        reload_error.code,
        crate::RuntimeErrorCode::ResidentSessionReloadFailed
    );
    assert_eq!(exported.head_revision, 1);
    assert_eq!(
        *runtime.resident_session.validity(),
        ResidentSessionState::Valid
    );
    assert_eq!(protocol.restore_count.load(Ordering::SeqCst), 4);

    let refusal_event = capture.exactly_one("resident_session_state.sync_refusal");
    assert_eq!(refusal_event.field("consumer"), "tool_state");
    assert_eq!(refusal_event.field("consulted_validity"), "false");
    assert_eq!(refusal_event.field("outcome"), "refused");
    assert_eq!(
        refusal_event.field("error_classification"),
        "resident_session_reload_failed"
    );
    let reload_decisions = capture.named("resident_session_state.reload_decision");
    assert_eq!(
        reload_decisions.len(),
        2,
        "one decision event is required for each reload attempt"
    );
    let denied = &reload_decisions[0];
    assert_eq!(denied.field("consulted_validity"), "false");
    assert_eq!(denied.field("durable_source"), "history_store");
    assert_eq!(
        denied.field("durable_head_freshness"),
        "reloaded_from_store"
    );
    assert_eq!(denied.field("resident_head_revision"), "1");
    assert_eq!(denied.field("durable_head_revision"), "1");
    assert_eq!(
        denied.field("failing_restore_stage"),
        "protocol_session_restore"
    );
    assert_eq!(denied.field("outcome"), "denied");
    assert_eq!(
        denied.field("error_classification"),
        "resident_session_reload_failed"
    );
    let restored = &reload_decisions[1];
    assert_eq!(restored.field("failing_restore_stage"), "none");
    assert_eq!(restored.field("outcome"), "restored");
    assert_eq!(restored.field("error_classification"), "none");
    assert_eq!(
        refusal_event.field("decision_id"),
        denied.field("decision_id"),
        "synchronous refusal must reference the reload decision identity"
    );
    assert_eq!(
        denied.field("decision_id"),
        restored.field("decision_id"),
        "retrying one invalidation incident preserves its decision identity"
    );

    let recovered = runtime
        .run_turn_assembled(
            TurnInput::text("use the reloaded state"),
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("after-post-commit-restore-failure"),
            ),
        )
        .await
        .expect("next use reloads durable resident state");
    assert_eq!(
        recovered.assistant_output.safe_text,
        "resident state reloaded"
    );
    assert_eq!(
        *runtime.resident_session.validity(),
        ResidentSessionState::Valid
    );
    assert_eq!(protocol.restore_count.load(Ordering::SeqCst), 4);
}

#[tokio::test]
pub(super) async fn double_invalidation_preserves_first_decision_id() {
    let mut runtime =
        runtime_with_plugins_and_tools(Vec::new(), Arc::new(EmptyTools), mock_provider(Vec::new()))
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

#[tokio::test]
pub(super) async fn successful_reload_clears_invalidated_state_to_valid() {
    let store = Arc::new(RecordingStore::default());
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;
    assert_eq!(
        *runtime.resident_session.validity(),
        ResidentSessionState::Valid
    );

    runtime.invalidate_resident_session_state();
    assert!(matches!(
        runtime.resident_session.validity(),
        ResidentSessionState::Invalidated { .. }
    ));

    runtime
        .reload_invalidated_resident_session_state()
        .await
        .expect("successful reload from store/snapshot");

    assert_eq!(
        *runtime.resident_session.validity(),
        ResidentSessionState::Valid,
        "successful reload must clear invalidated state back to Valid"
    );
}

#[tokio::test]
pub(super) async fn final_commit_refusals_reach_the_runtime_host_mapper() {
    let cases = [
        (
            crate::StoreError::HeadRevisionConflict {
                expected: 7,
                actual: 8,
            },
            crate::RuntimeErrorCode::StoreCommitSuperseded,
            None,
        ),
        (
            crate::StoreError::SessionDeleted {
                session_id: SessionId::from("deleted-during-final-commit"),
            },
            crate::RuntimeErrorCode::SessionDeleted,
            Some("deleted-during-final-commit"),
        ),
    ];

    for (case_index, (store_error, expected_code, expected_deleted_session_id)) in
        cases.into_iter().enumerate()
    {
        let transport = TestProvider::builder()
            .kind("mock")
            .complete(|_| async {
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "answered before the final commit refusal".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            })
            .build();
        let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
        store.fail_next_runtime_commit(store_error);

        let error = runtime
            .run_turn_assembled(
                crate::TurnInput::text("reach the production final commit caller"),
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from(format!("host-commit-refusal-{case_index}")),
                ),
            )
            .await
            .expect_err("the injected final commit refusal must reject the turn");

        assert_eq!(error.code, expected_code);
        assert_eq!(error.deleted_session_id(), expected_deleted_session_id);
    }
}

/// FIG-1573: a turn that ends without committing must not leave an input
/// pinned to it - no crash required.
///
/// The host routed an input into the running turn, so the row is
/// `pending_active` and scoped to that turn's id. The commit-time re-defer
/// (`RuntimeCommit::deferring_interrupted_turn_inputs`) is the only writer that
/// moves such a row back to `deferred_next_turn`, and this turn never reaches
/// its commit: the store fences it, exactly as a claim fenced at a checkpoint
/// does in the field. The teardown owes the row the same repair, and this test
/// reads the durable row directly so it proves the teardown trigger and not the
/// drain-time backstop.
#[tokio::test]
pub(super) async fn fig1573_input_pinned_to_a_turn_that_cannot_commit_is_re_deferred_at_teardown() {
    let session_id = "root";
    let live_turn_id = "fig1573-live-turn";
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::RuntimePersistence> = store.clone();
    let transport = TestProvider::builder()
        .kind("mock")
        .complete(|_| async {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "answered".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
        Arc::clone(&runtime_store),
    )
    .await;

    crate::TurnInputStore::enqueue_pending_turn_input(
        store.as_ref(),
        crate::PendingTurnInputDraft::new(
            session_id,
            crate::TurnInputIngress::active_turn(
                live_turn_id.to_string(),
                crate::TurnInputCheckpointBoundary::AfterWork,
            ),
            crate::TurnInput::text("routed into the live turn"),
        ),
    )
    .await
    .expect("enqueue an input scoped to the live turn");

    store.fail_next_runtime_commit(crate::StoreError::SessionExecutionLeaseExpired {
        session_id: SessionId::from(session_id.to_string()),
    });
    let error = runtime
        .run_turn_assembled(
            crate::TurnInput::text("run the turn that will be fenced at commit"),
            CancellationToken::new(),
            named_turn_scope(&SessionId::from(session_id), &TurnId::from(live_turn_id)),
        )
        .await
        .expect_err("a fenced commit must fail the turn");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::SessionExecutionLeaseLost
    );

    let pending = crate::TurnInputStore::list_pending_turn_inputs(
        store.as_ref(),
        &SessionId::from(session_id),
    )
    .await
    .expect("list pending turn inputs");
    assert_eq!(
        pending.len(),
        2,
        "the routed input is still queued, and so is the fenced turn's own acceptance (ADR 0069)"
    );
    for row in &pending {
        assert_eq!(
            row.state,
            crate::TurnInputState::DeferredNextTurn,
            "the teardown of a turn that cannot commit must re-defer every input it held"
        );
        assert_eq!(
            row.ingress,
            crate::TurnInputIngress::NextTurn,
            "the repaired rows must be addressable by the next turn, not by the dead turn id"
        );
    }
}

#[tokio::test]
pub(super) async fn dirty_execution_state_capture_failure_aborts_commit_and_cold_reopens_prior_state()
 {
    let executor = Arc::new(FailingCaptureExecutor {
        dirty: AtomicBool::new(true),
        fail_capture: AtomicBool::new(false),
        snapshot: std::sync::Mutex::new(b"committed-before-failure".to_vec()),
        restored: std::sync::Mutex::new(Vec::new()),
    });
    let protocol: Arc<dyn crate::plugin::ProtocolSessionPlugin> =
        Arc::new(RestoreExecutorFromRuntimeState {
            executor: Arc::clone(&executor),
        });
    let code_executor: Arc<dyn crate::plugin::CodeExecutorPlugin> = executor.clone();
    let protocol_factory = crate::testing::test_standard_protocol_factory_with_runtime_state(
        protocol,
        Some(code_executor),
    );
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::RuntimePersistence> = store.clone();
    let provider_executor = Arc::clone(&executor);
    let provider_call = Arc::new(AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let executor = Arc::clone(&provider_executor);
            let call = provider_call.fetch_add(1, Ordering::SeqCst);
            async move {
                let text = match call {
                    0 => "first committed turn",
                    1 => {
                        *executor.snapshot.lock_recover() = b"dirty-after-baseline".to_vec();
                        executor.dirty.store(true, Ordering::SeqCst);
                        executor.fail_capture.store(true, Ordering::SeqCst);
                        "must not commit"
                    }
                    index => panic!("unexpected provider call {index}"),
                };
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: text.to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![protocol_factory],
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
        Arc::clone(&runtime_store),
    )
    .await;

    runtime
        .run_turn_assembled(
            TurnInput::text("commit the baseline"),
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("execution-state-baseline"),
            ),
        )
        .await
        .expect("baseline turn commits its execution state");
    executor.dirty.store(false, Ordering::SeqCst);

    let error = runtime
        .run_turn_assembled(
            TurnInput::text("capture must fail"),
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("execution-state-capture-failure"),
            ),
        )
        .await
        .expect_err("dirty capture failure must abort before the turn commit");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::ExecutionStateCaptureFailed
    );
    assert!(
        error
            .message
            .contains("failed to snapshot dirty execution state")
    );
    let durable = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load baseline state")
        .expect("baseline state exists");
    assert_eq!(durable.head_revision, 1);
    assert_eq!(
        durable.execution_state_snapshot(),
        Some(b"committed-before-failure".as_slice())
    );

    drop(runtime);
    executor.fail_capture.store(false, Ordering::SeqCst);
    executor.dirty.store(false, Ordering::SeqCst);
    let reopen_protocol: Arc<dyn crate::plugin::ProtocolSessionPlugin> =
        Arc::new(RestoreExecutorFromRuntimeState {
            executor: Arc::clone(&executor),
        });
    let reopen_executor: Arc<dyn crate::plugin::CodeExecutorPlugin> = executor.clone();
    let reopen_factory = crate::testing::test_standard_protocol_factory_with_runtime_state(
        reopen_protocol,
        Some(reopen_executor),
    );
    let plugins = crate::PluginHost::new(vec![reopen_factory])
        .rematerialize_session(
            "root",
            durable.plugin_state().expect("durable plugin state"),
            crate::plugin::RecordedSessionConfig::new(durable.protocol_turn_options.clone()),
        )
        .expect("reopen plugins");
    let _reopened = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::PersistentRuntimeServices::new(plugins, runtime_store),
        durable,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("cold reopen restores the last committed execution state");
    assert_eq!(
        executor.restored.lock_recover().last().map(Vec::as_slice),
        Some(b"committed-before-failure".as_slice())
    );
}

#[tokio::test]
pub(super) async fn caller_supplied_key_colliding_with_existing_frame_preserves_execution_state() {
    let executor = Arc::new(FailingCaptureExecutor {
        dirty: AtomicBool::new(true),
        fail_capture: AtomicBool::new(false),
        snapshot: std::sync::Mutex::new(b"live-frame-execution-state".to_vec()),
        restored: std::sync::Mutex::new(Vec::new()),
    });
    let protocol: Arc<dyn crate::plugin::ProtocolSessionPlugin> =
        Arc::new(SwitchBeforeLlmProtocol {
            executor: Some(Arc::clone(&executor)),
            frame_key_material: "caller-named-existing-frame".to_string(),
            switch_next: AtomicBool::new(true),
        });
    let code_executor: Arc<dyn crate::plugin::CodeExecutorPlugin> = executor.clone();
    let protocol_factory = crate::testing::test_standard_protocol_factory_with_runtime_state(
        protocol,
        Some(code_executor),
    );
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::RuntimePersistence> = store.clone();
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|_| async move {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "follow-on must fail before commit".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![protocol_factory],
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
        Arc::clone(&runtime_store),
    )
    .await;
    let colliding_frame_key = crate::FrameKey::from_caller_material("caller-named-existing-frame")
        .expect("non-empty caller material");
    let opened = runtime
        .open_agent_frame(crate::OpenAgentFrameRequest::new(
            colliding_frame_key,
            crate::AgentFrameReason::initial(),
        ))
        .await
        .expect("pre-open caller-named frame");
    assert!(opened.opened, "caller-named collision target must exist");
    runtime.set_turn_phase_probe(Arc::new(FailCaptureAfterFirstCommittedTurn {
        executor: Arc::clone(&executor),
        committed_turns: AtomicUsize::new(0),
    }));
    let initial_frame_node_id = runtime
        .state
        .current_frame_node_id
        .clone()
        .expect("runtime initializes the current frame");

    let switched = runtime
        .run_turn_assembled(
            TurnInput::text("redrive an already materialized frame switch"),
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("already-current-frame-switch"),
            ),
        )
        .await
        .expect("an already-current frame switch remains an idempotent no-op");
    assert!(
        matches!(switched.outcome, TurnOutcome::AgentFrameSwitch { .. }),
        "unexpected no-op switch outcome: {switched:?}"
    );
    assert_eq!(
        runtime.state.current_frame_node_id.as_deref(),
        Some(initial_frame_node_id.as_str())
    );

    let durable = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load no-op switch state")
        .expect("no-op switch state is durable");
    assert_eq!(
        durable.execution_state_snapshot(),
        Some(b"live-frame-execution-state".as_slice())
    );
    drop(runtime);

    let reopened_executor = Arc::new(FailingCaptureExecutor {
        dirty: AtomicBool::new(false),
        fail_capture: AtomicBool::new(false),
        snapshot: std::sync::Mutex::new(Vec::new()),
        restored: std::sync::Mutex::new(Vec::new()),
    });
    let reopen_protocol: Arc<dyn crate::plugin::ProtocolSessionPlugin> =
        Arc::new(SwitchBeforeLlmProtocol {
            executor: Some(Arc::clone(&reopened_executor)),
            frame_key_material: "caller-named-existing-frame".to_string(),
            switch_next: AtomicBool::new(true),
        });
    let reopen_code_executor: Arc<dyn crate::plugin::CodeExecutorPlugin> =
        reopened_executor.clone();
    let reopen_factory = crate::testing::test_standard_protocol_factory_with_runtime_state(
        reopen_protocol,
        Some(reopen_code_executor),
    );
    let plugins = crate::PluginHost::new(vec![reopen_factory])
        .rematerialize_session(
            "root",
            durable.plugin_state().expect("durable plugin state"),
            crate::plugin::RecordedSessionConfig::new(durable.protocol_turn_options.clone()),
        )
        .expect("cold-reopen plugins");
    let _reopened = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::PersistentRuntimeServices::new(plugins, runtime_store),
        durable,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("cold reopen restores the still-live frame execution state");
    assert_eq!(
        reopened_executor
            .restored
            .lock_recover()
            .last()
            .map(Vec::as_slice),
        Some(b"live-frame-execution-state".as_slice())
    );
}

#[tokio::test]
pub(super) async fn materialized_frame_switch_clears_checkpoint_and_resets_resident_executor() {
    let executor = Arc::new(FailingCaptureExecutor {
        dirty: AtomicBool::new(true),
        fail_capture: AtomicBool::new(false),
        snapshot: std::sync::Mutex::new(b"abandoned-frame-execution-state".to_vec()),
        restored: std::sync::Mutex::new(Vec::new()),
    });
    let protocol: Arc<dyn crate::plugin::ProtocolSessionPlugin> =
        Arc::new(ResetExecutorOnSwitchProtocol {
            executor: Arc::clone(&executor),
            frame_key_material: "materialized-next-frame".to_string(),
            switch_next: AtomicBool::new(true),
        });
    let code_executor: Arc<dyn crate::plugin::CodeExecutorPlugin> = executor.clone();
    let protocol_factory = crate::testing::test_standard_protocol_factory_with_runtime_state(
        protocol,
        Some(code_executor),
    );
    let store = Arc::new(RecordingStore::default());
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|_| async move {
            panic!("a protocol-directed frame switch must finish before provider execution")
        })
        .build();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![protocol_factory],
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(FailCaptureAfterFirstCommittedTurn {
        executor: Arc::clone(&executor),
        committed_turns: AtomicUsize::new(0),
    }));
    executor.restored.lock_recover().clear();

    let switched = runtime
        .run_turn_assembled(
            TurnInput::text("switch to a distinct frame"),
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("materialized-frame-switch"),
            ),
        )
        .await
        .expect("materialized frame switch commits");
    assert!(matches!(
        switched.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));

    let durable = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load materialized switch state")
        .expect("materialized switch state is durable");
    assert!(
        durable.execution_state_ref().is_none()
            && durable.execution_state_snapshot().is_none()
            && executor.restored.lock_recover().last().map(Vec::as_slice)
                == Some(b"fresh-frame-execution-state".as_slice()),
        "one committed switch must durably clear the checkpoint and reset the resident executor"
    );
}

#[tokio::test]
pub(super) async fn capture_abort_releases_lease_and_claim_for_prompt_peer_reclaim() {
    let executor = Arc::new(FailingCaptureExecutor {
        dirty: AtomicBool::new(false),
        fail_capture: AtomicBool::new(false),
        snapshot: std::sync::Mutex::new(Vec::new()),
        restored: std::sync::Mutex::new(Vec::new()),
    });
    let protocol: Arc<dyn crate::plugin::ProtocolSessionPlugin> =
        Arc::new(RestoreExecutorFromRuntimeState {
            executor: Arc::clone(&executor),
        });
    let code_executor: Arc<dyn crate::plugin::CodeExecutorPlugin> = executor.clone();
    let protocol_factory = crate::testing::test_standard_protocol_factory_with_runtime_state(
        protocol,
        Some(code_executor),
    );
    let store = Arc::new(RecordingStore::default());
    let failing_transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| async move {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "must not commit".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build();
    let mut first = runtime_with_plugins_and_tools_and_host_and_store(
        vec![Arc::clone(&protocol_factory)],
        Arc::new(EmptyTools),
        failing_transport,
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;
    first.set_turn_phase_probe(Arc::new(FailCaptureAfterEffectLoop {
        executor: Arc::clone(&executor),
    }));
    enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "peer must reclaim this input",
    )
    .await;

    let error = first
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("capture-abort-owner"),
            ),
        ))
        .await
        .expect_err("dirty capture aborts before commit");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::ExecutionStateCaptureFailed
    );

    executor.fail_capture.store(false, Ordering::SeqCst);
    executor.dirty.store(false, Ordering::SeqCst);
    let mut peer = runtime_with_plugins_and_tools_and_host_and_store(
        vec![protocol_factory],
        Arc::new(EmptyTools),
        mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "peer reclaimed".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        }]),
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;

    let reclaimed = peer
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("capture-abort-peer"),
            ),
        ))
        .await
        .expect("peer reclaim must not wait for the lease TTL")
        .ran()
        .expect("peer immediately receives the abandoned input");
    assert_eq!(reclaimed.assistant_output.safe_text, "peer reclaimed");
}

#[tokio::test]
pub(super) async fn follow_on_capture_failure_returns_the_committed_frame_and_handoff_is_retry_safe()
 {
    let executor = Arc::new(FailingCaptureExecutor {
        dirty: AtomicBool::new(false),
        fail_capture: AtomicBool::new(false),
        snapshot: std::sync::Mutex::new(Vec::new()),
        restored: std::sync::Mutex::new(Vec::new()),
    });
    let protocol: Arc<dyn crate::plugin::ProtocolSessionPlugin> =
        Arc::new(RestoreExecutorFromRuntimeState {
            executor: Arc::clone(&executor),
        });
    let code_executor: Arc<dyn crate::plugin::CodeExecutorPlugin> = executor.clone();
    let protocol_factory = crate::testing::test_standard_protocol_factory_with_runtime_state(
        protocol,
        Some(code_executor),
    );
    let store = Arc::new(RecordingStore::default());
    let call_index = Arc::new(AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let call_index = Arc::clone(&call_index);
            async move {
                Ok(match call_index.fetch_add(1, Ordering::SeqCst) {
                    0 => LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "switch".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    },
                    1 => LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "recovered follow-on".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    },
                    index => panic!("unexpected provider call {index}"),
                })
            }
        })
        .build();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![protocol_factory],
        Arc::new(TerminalControlTool {
            controls: vec![crate::ToolControl::SwitchAgentFrame {
                frame_key: crate::FrameKey::from_caller_material("capture-failure-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("recover committed handoff".to_string()),
            }],
        }),
        transport,
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(FailCaptureAfterFirstCommittedTurn {
        executor: Arc::clone(&executor),
        committed_turns: AtomicUsize::new(0),
    }));
    enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "switch then fail capture",
    )
    .await;

    let committed = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("follow-on-capture-failure"),
            ),
        ))
        .await
        .expect("a follow-on pre-commit failure must not erase the committed frame")
        .ran()
        .expect("the committed frame is returned");
    assert!(matches!(
        committed.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    assert!(committed.errors.iter().any(|issue| {
        issue.code.as_deref() == Some("execution_state_capture_failed")
            && issue.retryable == Some(false)
    }));
    let durable = crate::store::SessionCommitStore::load_session(store.as_ref())
        .await
        .expect("load committed frame")
        .expect("committed frame exists");
    assert_eq!(durable.head_revision, 1);

    executor.fail_capture.store(false, Ordering::SeqCst);
    executor.dirty.store(false, Ordering::SeqCst);
    let recovered = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("retry-safe-committed-handoff"),
            ),
        ))
        .await
        .expect("retrying the logical queue call is safe")
        .ran()
        .expect("the durable handoff is reclaimed");
    assert_eq!(recovered.assistant_output.safe_text, "recovered follow-on");
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), &SessionId::from("root"))
            .await
            .expect("queue after recovered handoff")
            .is_empty()
    );
}

#[derive(Debug)]
pub(super) struct StepExpiryClock {
    epoch_ms: u64,
    live_timestamp_calls: std::sync::atomic::AtomicU64,
    timestamp_calls: std::sync::atomic::AtomicU64,
    armed: AtomicBool,
}

impl StepExpiryClock {
    pub(super) fn new(epoch_ms: u64) -> Self {
        Self {
            epoch_ms,
            live_timestamp_calls: std::sync::atomic::AtomicU64::new(u64::MAX),
            timestamp_calls: std::sync::atomic::AtomicU64::new(0),
            armed: AtomicBool::new(false),
        }
    }

    pub(super) fn expire_after_timestamp_calls(&self, live_calls: u64) {
        self.timestamp_calls
            .store(0, std::sync::atomic::Ordering::SeqCst);
        self.live_timestamp_calls
            .store(live_calls, std::sync::atomic::Ordering::SeqCst);
        self.armed.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl crate::Clock for StepExpiryClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let timestamp_ms = if !self.armed.load(Ordering::SeqCst) {
            self.epoch_ms
        } else {
            let call = self
                .timestamp_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call
                < self
                    .live_timestamp_calls
                    .load(std::sync::atomic::Ordering::SeqCst)
            {
                self.epoch_ms
            } else {
                self.epoch_ms
                    .saturating_add(crate::LeaseTimings::default().ttl_ms())
            }
        };
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(timestamp_ms),
        )
    }

    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

#[test]
pub(super) fn step_expiry_clock_wall_clock_faces_agree() {
    let clock = StepExpiryClock::new(1_700_000_000_123);
    let clock: &dyn crate::Clock = &clock;
    let milliseconds = clock.timestamp_ms();
    let datetime = clock.timestamp_datetime();
    let text = chrono::DateTime::parse_from_rfc3339(&clock.timestamp_rfc3339())
        .expect("clock emits RFC 3339");
    assert_eq!(datetime.timestamp_millis() as u64, milliseconds);
    assert_eq!(text.timestamp_millis() as u64, milliseconds);
}

pub(super) struct FrameRotatingDynamicTool {
    rotated: Arc<AtomicBool>,
}

pub(super) fn rotating_tool_definition(name: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "Exercise live tool discovery across an AgentFrame rotation",
        crate::ToolDefinition::default_input_schema(),
        json!({ "type": "object", "additionalProperties": true }),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for FrameRotatingDynamicTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        let mut manifests = vec![
            rotating_tool_definition("rotate_surface").manifest(),
            rotating_tool_definition("curated_before_rotation").manifest(),
        ];
        if self.rotated.load(Ordering::SeqCst) {
            manifests.push(rotating_tool_definition("new_after_rotation").manifest());
            manifests.push(rotating_tool_definition("hidden_after_rotation").manifest());
        }
        manifests
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        self.tool_manifests()
            .into_iter()
            .any(|manifest| manifest.name == name)
            .then(|| Arc::new(rotating_tool_definition(name).contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        match call.name {
            "rotate_surface" => {
                self.rotated.store(true, Ordering::SeqCst);
                crate::ToolOutcome::ok(json!({ "rotated": true })).with_control(
                    crate::ToolControl::SwitchAgentFrame {
                        frame_key: crate::FrameKey::from_caller_material("live-surface-frame")
                            .expect("non-empty caller material"),
                        initial_nodes: Vec::new(),
                        task: Some("call the newly available tool".to_string()),
                    },
                )
            }
            "new_after_rotation" => crate::ToolOutcome::ok(json!({ "called": call.name }))
                .with_control(crate::ToolControl::Finish {
                    value: crate::ToolValue::untrusted_json(json!("new tool executed")),
                }),
            "curated_before_rotation" | "hidden_after_rotation" => {
                crate::ToolOutcome::ok(json!({ "called": call.name }))
            }
            name => crate::ToolOutcome::err_fmt(format_args!("unknown rotating tool `{name}`")),
        }
    }
}

#[tokio::test]
pub(super) async fn continue_as_frame_rotation_reconciles_newly_advertised_tool() {
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "rotate-call".to_string(),
                    tool_name: "rotate_surface".to_string(),
                    input_json: "{}".to_string(),
                    replay: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "new-tool-call".to_string(),
                    tool_name: "new_after_rotation".to_string(),
                    input_json: "{}".to_string(),
                    replay: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(FrameRotatingDynamicTool {
        rotated: Arc::new(AtomicBool::new(false)),
    });
    let mut factories = crate::testing::test_standard_protocol_factories();
    factories.push(Arc::new(StaticPluginFactory::new(
        "frame_rotating_tools",
        crate::PluginSpec::new().with_tool_provider(tools),
    )));
    let plugins = crate::PluginHost::new(factories)
        .build_session_with_parent(
            "root",
            Some(SessionId::from("parent")),
            crate::plugin::SessionCreationConfig {
                authority: crate::plugin::SessionAuthorityContext {
                    tool_access: crate::SessionToolAccess::ambient()
                        .with_hidden_tools(["hidden_after_rotation"])
                        .expect("valid hidden name"),
                    ..crate::plugin::SessionAuthorityContext::default()
                },
                ..Default::default()
            },
        )
        .expect("frame child plugins");
    let mut runtime = LashRuntime::from_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::RuntimeServices::new(plugins),
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded)),
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("frame child runtime");
    set_runtime_provider(&mut runtime, transport.into_handle());
    let mut curated = runtime.tool_state().expect("pre-rotation tool state");
    curated
        .set_membership(&crate::ToolId::from("tool:curated_before_rotation"), false)
        .expect("opt out before rotation");
    runtime
        .apply_tool_state(curated)
        .await
        .expect("apply pre-rotation curation");

    let run = runtime
        .stream_turn_with_agent_frames(
            TurnInput::text("rotate the frame"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("live-surface-frame-rotation"),
                ),
            ),
        )
        .await
        .expect("AgentFrame run");

    assert_eq!(run.frame_switch_count(), 1);
    let final_turn = run.final_turn().expect("final frame");
    assert!(
        matches!(
            &final_turn.outcome,
            TurnOutcome::Finished(TurnFinish::ToolValue { tool_name, value })
                if tool_name == "new_after_rotation" && *value == json!("new tool executed")
        ),
        "new tool must be callable in the follow frame: {:?}",
        final_turn.outcome
    );
    let catalog_names = runtime
        .active_tool_catalog_shared()
        .expect("post-rotation model catalog")
        .iter()
        .filter_map(|entry| entry["name"].as_str().map(ToOwned::to_owned))
        .collect::<Vec<_>>();
    assert!(catalog_names.contains(&"new_after_rotation".to_string()));
    assert!(!catalog_names.contains(&"hidden_after_rotation".to_string()));
    assert!(!catalog_names.contains(&"curated_before_rotation".to_string()));

    let registry = runtime
        .session
        .as_ref()
        .expect("frame child session")
        .plugins()
        .tool_registry();
    let post_rotation_state = registry.export_state();
    assert!(
        !post_rotation_state
            .get(&crate::ToolId::from("tool:curated_before_rotation"))
            .expect("curated entry survives rotation")
            .is_member()
    );
    assert!(
        post_rotation_state
            .get(&crate::ToolId::from("tool:hidden_after_rotation"))
            .expect("new hidden entry retains host curation")
            .is_member(),
        "frame authority must not rewrite ToolId-keyed host curation"
    );
}

pub(super) struct ExpireLeaseAtPreparedTurn {
    clock: Arc<ManualClock>,
    expired: AtomicBool,
}

impl ExpireLeaseAtPreparedTurn {
    pub(super) fn new(clock: Arc<ManualClock>) -> Self {
        Self {
            clock,
            expired: AtomicBool::new(false),
        }
    }
}

impl crate::runtime::RuntimeTurnPhaseProbe for ExpireLeaseAtPreparedTurn {
    fn begin(&self, phase: crate::runtime::RuntimeTurnPhase) {
        if phase == crate::runtime::RuntimeTurnPhase::PreparedTurn
            && !self.expired.swap(true, Ordering::SeqCst)
        {
            self.clock
                .advance_ms(crate::LeaseTimings::default().ttl_ms() + 1);
        }
    }

    fn end(&self, _phase: crate::runtime::RuntimeTurnPhase) {}
}

pub(super) struct ExpireLeaseAfterPromptBuild {
    clock: Arc<ManualClock>,
    expired: AtomicBool,
}

impl ExpireLeaseAfterPromptBuild {
    pub(super) fn new(clock: Arc<ManualClock>) -> Self {
        Self {
            clock,
            expired: AtomicBool::new(false),
        }
    }
}

impl crate::runtime::RuntimeTurnPhaseProbe for ExpireLeaseAfterPromptBuild {
    fn begin(&self, _phase: crate::runtime::RuntimeTurnPhase) {}

    fn end(&self, phase: crate::runtime::RuntimeTurnPhase) {
        if phase == crate::runtime::RuntimeTurnPhase::PromptBuild
            && !self.expired.swap(true, Ordering::SeqCst)
        {
            self.clock
                .advance_ms(crate::LeaseTimings::default().ttl_ms() + 1);
        }
    }
}

pub(super) struct ExpireLeaseAfterRetainedCommit {
    clock: Arc<ManualClock>,
    expired: AtomicBool,
}

pub(super) struct ExpireLeaseAtSecondTurnFinalizedHook {
    clock: Arc<ManualClock>,
    finalized_hooks: AtomicUsize,
}

impl ExpireLeaseAtSecondTurnFinalizedHook {
    pub(super) fn new(clock: Arc<ManualClock>) -> Self {
        Self {
            clock,
            finalized_hooks: AtomicUsize::new(0),
        }
    }
}

impl crate::runtime::RuntimeTurnPhaseProbe for ExpireLeaseAtSecondTurnFinalizedHook {
    fn begin(&self, _phase: crate::runtime::RuntimeTurnPhase) {}

    fn end(&self, _phase: crate::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, phase: &str) {
        if phase.starts_with("plugin_hook.turn_finalized.")
            && self.finalized_hooks.fetch_add(1, Ordering::SeqCst) == 1
        {
            self.clock
                .advance_ms(crate::LeaseTimings::default().ttl_ms() + 1);
        }
    }
}

pub(super) struct PauseAtPreparedTurn {
    pub(super) entered: Arc<AtomicBool>,
    pub(super) release: Arc<AtomicBool>,
}

impl crate::runtime::RuntimeTurnPhaseProbe for PauseAtPreparedTurn {
    fn begin(&self, phase: crate::runtime::RuntimeTurnPhase) {
        if phase != crate::runtime::RuntimeTurnPhase::PreparedTurn {
            return;
        }
        self.entered.store(true, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
    }

    fn end(&self, _phase: crate::runtime::RuntimeTurnPhase) {}
}

pub(super) struct PauseFirstProductCommitAttempt {
    pub(super) entered: Arc<AtomicBool>,
    pub(super) release: Arc<AtomicBool>,
    pub(super) attempts: AtomicUsize,
}

impl crate::runtime::RuntimeTurnPhaseProbe for PauseFirstProductCommitAttempt {
    fn begin(&self, _phase: crate::runtime::RuntimeTurnPhase) {}

    fn end(&self, _phase: crate::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, phase: &str) {
        if phase == "commit_admission.product_attempt"
            && self.attempts.fetch_add(1, Ordering::SeqCst) == 0
        {
            self.entered.store(true, Ordering::SeqCst);
            while !self.release.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        }
    }
}

pub(super) struct PauseAfterEffectLoop {
    pub(super) entered: Arc<AtomicBool>,
    pub(super) release: Arc<AtomicBool>,
}

pub(super) struct CasSurvivorIntentTools {
    pub(super) calls: Arc<AtomicUsize>,
}

pub(super) struct ParentEndFailureIntentTool;

pub(super) fn parent_end_failure_intent_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:parent_end_failure_intent",
        "parent_end_failure_intent",
        "Start a process whose parent-end action exercises cancelled-turn teardown.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for ParentEndFailureIntentTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![parent_end_failure_intent_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "parent_end_failure_intent")
            .then(|| Arc::new(parent_end_failure_intent_tool().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        panic!("the parent-end failure witness must use AttemptContext")
    }

    async fn execute_attempt(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolAttemptOutcome::done(
            crate::ToolOutcomeDone::ok(serde_json::json!({"started": true})),
            crate::ToolIntents::v1(vec![crate::ToolIntent::StartProcess(Box::new(
                crate::StartProcessIntent {
                    session_id: SessionId::from(call.context.session_id()),
                    request: crate::ProcessStartRequest::external(
                        "cancelled-turn-parent-end-child",
                        crate::ProcessOriginator::host_scoped("parent-end-failure-witness"),
                        serde_json::json!({"witness": true}),
                    ),
                    on_parent_end: crate::ProcessParentEndPolicy::Abandon,
                },
            ))]),
        )
    }
}

pub(super) fn cas_survivor_intent_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:cas_survivor_intent",
        "cas_survivor_intent",
        "Emit durable evidence before the enclosing turn competes on head CAS.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for CasSurvivorIntentTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![cas_survivor_intent_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "cas_survivor_intent").then(|| Arc::new(cas_survivor_intent_tool().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        panic!("the lease/CAS survivor law must use AttemptContext")
    }

    async fn execute_attempt(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        crate::ToolAttemptOutcome::done(
            crate::ToolOutcomeDone::ok(serde_json::json!({"intent": "committed"})),
            crate::ToolIntents::v1(vec![crate::ToolIntent::EmitProcessEvent(
                crate::EmitProcessEventIntent {
                    session_id: SessionId::from(call.context.session_id()),
                    process_id: ProcessId::from("cas-survivor-intent-target"),
                    event_type: "intent.survivor.committed".to_string(),
                    payload: serde_json::json!({"survives": true}),
                },
            )]),
        )
    }
}

impl crate::runtime::RuntimeTurnPhaseProbe for PauseAfterEffectLoop {
    fn begin(&self, _phase: crate::runtime::RuntimeTurnPhase) {}

    fn end(&self, phase: crate::runtime::RuntimeTurnPhase) {
        if phase != crate::runtime::RuntimeTurnPhase::EffectLoop {
            return;
        }
        self.entered.store(true, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
    }
}

impl ExpireLeaseAfterRetainedCommit {
    pub(super) fn new(clock: Arc<ManualClock>) -> Self {
        Self {
            clock,
            expired: AtomicBool::new(false),
        }
    }
}

impl crate::runtime::RuntimeTurnPhaseProbe for ExpireLeaseAfterRetainedCommit {
    fn begin(&self, phase: crate::runtime::RuntimeTurnPhase) {
        if phase == crate::runtime::RuntimeTurnPhase::PostCommitDelivery
            && !self.expired.swap(true, Ordering::SeqCst)
        {
            self.clock
                .advance_ms(crate::LeaseTimings::default().ttl_ms() + 1);
        }
    }

    fn end(&self, _phase: crate::runtime::RuntimeTurnPhase) {}
}

pub(super) async fn standard_runtime_with_transport_and_queue_store(
    transport: TestProvider,
) -> (LashRuntime, Arc<RecordingStore>) {
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    (runtime, store)
}

pub(super) async fn standard_runtime_with_transport_and_queue_store_for_session(
    transport: TestProvider,
    session_id: &SessionId,
) -> (LashRuntime, Arc<RecordingStore>) {
    let store = Arc::new(RecordingStore::default());
    let runtime = TestRuntime::new(transport)
        .tools(Arc::new(EmptyTools))
        .host(test_host_config())
        .store(store.clone())
        .with_session_id(session_id)
        .build()
        .await;
    (runtime, store)
}

pub(super) async fn standard_runtime_with_transport_and_queue_store_clock(
    transport: TestProvider,
    clock: Arc<dyn crate::Clock>,
) -> (LashRuntime, Arc<RecordingStore>) {
    let store = Arc::new(RecordingStore::with_clock(clock));
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    (runtime, store)
}

#[derive(Clone, Default)]
pub(super) struct JournalReplayEffectController {
    native: crate::NativeRuntimeEffectController,
    outcomes: Arc<Mutex<HashMap<String, crate::RuntimeEffectOutcome>>>,
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for JournalReplayEffectController {
    async fn prepare_completion_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<crate::CompletionKeyPreparation, crate::RuntimeError> {
        self.native
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
    ) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        self.native.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &crate::AwaitEventKey,
        resolution: crate::Resolution,
    ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
        self.native.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &crate::AwaitEventKey,
    ) -> Result<Option<crate::Resolution>, crate::RuntimeError> {
        self.native.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &crate::AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<crate::Resolution, crate::RuntimeError> {
        self.native.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
        self.native
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
        self.native
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for JournalReplayEffectController {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let effect_id = envelope
            .invocation
            .effect_id()
            .expect("journal replay effect has an id")
            .to_string();
        if let Some(outcome) = self.outcomes.lock_recover().get(&effect_id) {
            return Ok(outcome.clone());
        }
        let outcome = self.native.execute_effect(envelope, local_executor).await?;
        self.outcomes
            .lock_recover()
            .insert(effect_id, outcome.clone());
        Ok(outcome)
    }
}

pub(super) fn journal_replay_host(
    controller: Arc<dyn crate::RuntimeEffectController>,
) -> crate::EmbeddedRuntimeHost {
    let mut host = test_host_config();
    host.core.control.effect_host = Arc::new(crate::NativeEffectHost::new(controller));
    host
}

pub(super) struct JournalRedriveStore {
    pub(super) inner: Arc<RecordingStore>,
    pub(super) application_history_available: bool,
    pub(super) foreign_checkpoint_application: Option<(String, crate::TurnId)>,
}

#[async_trait::async_trait]
impl crate::store::RuntimePersistenceDecorator for JournalRedriveStore {
    fn inner(&self) -> &(dyn crate::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn load_session(
        &self,
    ) -> Result<Option<crate::store::PersistedSessionRead>, crate::StoreError> {
        // A workflow replay resumes from the invocation's pre-commit resident
        // state while the store already contains the first execution's commit.
        Ok(None)
    }

    async fn list_turn_input_applications(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::TurnInputApplication>, crate::StoreError> {
        if self.application_history_available {
            let mut applications = crate::store::TurnInputStore::list_turn_input_applications(
                self.inner.as_ref(),
                session_id,
            )
            .await?;
            if let Some((input_id, turn_id)) = &self.foreign_checkpoint_application {
                let application = applications
                    .iter_mut()
                    .find(|application| application.input_id == *input_id)
                    .expect("foreign checkpoint application input is retained");
                application.turn_id = turn_id.clone();
                application.checkpoint = Some(crate::CheckpointKind::BeforeCompletion);
            }
            return Ok(applications);
        }
        Err(crate::StoreError::Backend(
            "simulated unavailable turn-input application history".to_string(),
        ))
    }
}

pub(super) async fn append_process_wake_to_queue(
    registry: &dyn crate::ProcessRegistry,
    store: &RecordingStore,
    process_id: &ProcessId,
    request: crate::ProcessEventAppendRequest,
) -> crate::ProcessWakeDelivery {
    let appended = registry
        .append_event(process_id, request)
        .await
        .expect("append wake");
    let wake = appended.wake_delivery.expect("wake delivery");
    crate::store::QueuedWorkStore::enqueue_queued_work(
        store,
        crate::process_wake_batch_draft(wake.clone()),
    )
    .await
    .expect("enqueue wake");
    wake
}

pub(super) fn process_wake_event_type() -> crate::ProcessEventType {
    crate::ProcessEventType {
        name: "process.wake".to_string(),
        payload_schema: crate::LashSchema::any(),
        semantics: crate::ProcessEventSemanticsSpec {
            wake: Some(crate::ProcessWakeSpec {
                when: None,
                input: crate::ProcessValueSelector::Pointer("/text".to_string()),
            }),
            ..crate::ProcessEventSemanticsSpec::default()
        },
    }
}

pub(super) fn request_contains_text(request: &crate::llm::types::LlmRequest, needle: &str) -> bool {
    request.messages.iter().any(|message| {
        message.blocks.iter().any(|block| match block {
            crate::llm::types::LlmContentBlock::Text { text, .. } => text.contains(needle),
            _ => false,
        })
    })
}

pub(super) async fn enqueue_turn_input_for_checkpoint(
    store: &RecordingStore,
    session_id: &SessionId,
    turn_id: &TurnId,
    source_key: Option<String>,
    input: TurnInput,
) -> crate::PendingTurnInput {
    let mut draft = crate::PendingTurnInputDraft::new(
        session_id.to_string(),
        crate::TurnInputIngress::active_turn(
            turn_id.to_string(),
            crate::TurnInputCheckpointBoundary::AfterWork,
        ),
        input,
    );
    draft.source_key = source_key;
    crate::store::TurnInputStore::enqueue_pending_turn_input(store, draft)
        .await
        .expect("enqueue turn input")
}

pub(super) async fn enqueue_idle_turn_input(
    store: &RecordingStore,
    session_id: &SessionId,
    text: &str,
) -> crate::PendingTurnInput {
    crate::store::TurnInputStore::enqueue_pending_turn_input(
        store,
        crate::PendingTurnInputDraft::new(
            session_id.to_string(),
            crate::TurnInputIngress::NextTurn,
            TurnInput::text(text),
        ),
    )
    .await
    .expect("enqueue idle turn input")
}

pub(super) async fn enqueue_session_command(
    store: &RecordingStore,
    session_id: &SessionId,
    reason: &str,
) -> crate::QueuedWorkBatch {
    crate::store::QueuedWorkStore::enqueue_queued_work(
        store,
        crate::QueuedWorkBatchDraft::new(
            session_id.to_string(),
            crate::DeliveryPolicy::EarliestSafeBoundary,
            crate::SessionCommand::RefreshToolCatalog {
                reason: reason.to_string(),
            },
        ),
    )
    .await
    .expect("enqueue session command")
}

pub(super) async fn enqueue_config_patch_command(
    store: &RecordingStore,
    session_id: &SessionId,
    patch: crate::runtime::ApplyConfigPatch,
) -> crate::QueuedWorkBatch {
    crate::store::QueuedWorkStore::enqueue_queued_work(
        store,
        crate::QueuedWorkBatchDraft::new(
            session_id.to_string(),
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            crate::SessionCommand::ApplyConfigPatch {
                patch: Box::new(patch),
            },
        ),
    )
    .await
    .expect("enqueue config patch command")
}
