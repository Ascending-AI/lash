use super::*;
use lash_core::testing::TestTurnDrive as _;

const SEED: u64 = 0x5_f450;

#[tokio::test]
pub(super) async fn plugin_before_turn_can_abort_and_inject_messages() {
    let double = kernel_double(SEED + 1, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: Some(Arc::new(|_| {
                    Box::pin(async {
                        Ok(vec![
                            lash_core::facade_support::TurnPluginDirective::EnqueueMessages(
                                lash_core::facade_support::EnqueueMessagesDirective {
                                    messages: vec![lash_core::PluginMessage::text(
                                        lash_core::MessageRole::System,
                                        "plugin preface",
                                    )],
                                },
                            ),
                            lash_core::facade_support::TurnPluginDirective::AbortTurn(
                                lash_core::facade_support::AbortTurnDirective {
                                    code: "blocked".to_string(),
                                    message: "plugin stopped the turn".to_string(),
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
    let transport = mock_provider(Vec::new());
    let mut runtime = runtime_with_plugins(&backend, vec![plugin], transport).await;

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("plugin-extension-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .drive_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    assert!(matches!(&turn.outcome, TurnOutcome::Stopped(_)));
    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Stopped(TurnStop::PluginAbort)
    ));
    assert!(
        turn.errors
            .iter()
            .any(|issue| issue.kind == lash_core::TurnFailureKind::Plugin)
    );
    assert!(
        active_conversation_messages(&turn.state)
            .iter()
            .any(|message| {
                message
                    .parts
                    .iter()
                    .any(|part| part.content().contains("plugin preface"))
            })
    );
}

#[tokio::test]
pub(super) async fn normal_turn_stores_effective_user_text_in_state() {
    let double = kernel_double(SEED + 2, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "Done".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let mut runtime = runtime_with_plugins(&backend, Vec::new(), transport).await;

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("skill-command-visibility-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .drive_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "/yolopush\n\n<skill>\nbody\n</skill>".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    let read_model = turn.state.read_model();
    let read_model = read_model.expect("accepted turn frame scope resolves");
    let user_message = read_model
        .messages
        .iter()
        .find(|message| message.role == MessageRole::User)
        .expect("user message");
    assert_eq!(
        user_message
            .parts
            .first()
            .map(|part| part.content())
            .as_deref(),
        Some("/yolopush\n\n<skill>\nbody\n</skill>")
    );
    // The committed turn input carries typed provenance so a host that rendered
    // its own row for this turn recognizes this copy without parsing the
    // runtime-minted message id (FIG-972). The direct path has no durable turn
    // input behind it, so `input_id` is absent.
    assert_eq!(
        user_message.origin,
        Some(lash_core::MessageOrigin::TurnInput {
            turn_id: TurnId::from("skill-command-visibility-turn"),
            input_id: None,
        })
    );
}

#[tokio::test]
pub(super) async fn retryable_llm_failures_exhaust_and_fail_turn() {
    let double = kernel_double(SEED + 3, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Err(lash_core::llm::transport::LlmTransportError::new(
                "provider unavailable",
            )
            .with_retry_verdict(
                lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
            )
            .with_code(FailureCode::provider("http_500"))),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Err(lash_core::llm::transport::LlmTransportError::new(
                "provider unavailable",
            )
            .with_retry_verdict(
                lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
            )
            .with_code(FailureCode::provider("http_500"))),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Err(lash_core::llm::transport::LlmTransportError::new(
                "provider unavailable",
            )
            .with_retry_verdict(
                lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
            )
            .with_code(FailureCode::provider("http_500"))),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Err(lash_core::llm::transport::LlmTransportError::new(
                "provider unavailable",
            )
            .with_retry_verdict(
                lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
            )
            .with_code(FailureCode::provider("http_500"))),
        },
    ]);
    let mut runtime = runtime_with_plugins(&backend, Vec::new(), transport).await;
    runtime.host.core.clock = Arc::new(CancelWatchTestClock(lash_core::testing::TestClock::new(
        1_700_000_000_123,
    )));

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("retryable-error-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .drive_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    assert!(matches!(&turn.outcome, TurnOutcome::Stopped(_)));
    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Stopped(TurnStop::ProviderError)
    ));
    assert!(
        turn.errors
            .iter()
            .any(|issue| issue.kind == lash_core::TurnFailureKind::LlmProvider)
    );
    assert!(
        turn.errors
            .iter()
            .any(|issue| issue.message.contains("provider unavailable"))
    );
    // The transport's typed retryable signal survives into the host-facing
    // issue instead of living only in trace records.
    assert!(turn.errors.iter().any(
        |issue| issue.kind == lash_core::TurnFailureKind::LlmProvider
            && issue.retryable == Some(true)
    ));
    assert_eq!(turn.llm_calls.len(), 1);
    assert_eq!(turn.llm_calls[0].attempts.len(), 4);
}

#[tokio::test]
pub(super) async fn provider_failure_surfaces_typed_kind_and_retryability_on_turn_issue() {
    let double = kernel_double(SEED + 4, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    // A 400 classifies as a non-retryable Validation failure, so the turn
    // fails on the first attempt with fully typed failure signals.
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Err(
            lash_core::llm::transport::LlmTransportError::new("bad request")
                .with_http_status(400)
                .with_code(FailureCode::provider("400")),
        ),
    }]);
    let mut runtime = runtime_with_plugins(&backend, Vec::new(), transport).await;

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("typed-provider-failure-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .drive_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Stopped(TurnStop::ProviderError)
    ));
    let issue = turn
        .errors
        .iter()
        .find(|issue| issue.kind == lash_core::TurnFailureKind::LlmProvider)
        .expect("llm_provider issue");
    assert_eq!(issue.retryable, Some(false));
    assert_eq!(
        issue.provider_failure_kind,
        Some(lash_core::ProviderFailureKind::Validation)
    );
    assert_eq!(issue.code, Some(lash_core::FailureCode::provider("400")));
    assert_eq!(turn.llm_calls.len(), 1);
    assert_eq!(turn.llm_calls[0].attempts.len(), 1);
}

#[tokio::test]
pub(super) async fn assembled_turn_reports_turn_timing_from_injected_clock() {
    let double = kernel_double(SEED + 5, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "Done".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let mut runtime = runtime_with_plugins(&backend, Vec::new(), transport).await;
    runtime.host.core.clock = Arc::new(ManualClock::new(4_242));

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("turn-timing-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .drive_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    // `started_at_ms` is read from the injected wall clock, so a
    // deterministic clock yields a deterministic timestamp (the OS clock
    // would report the current epoch here).
    assert_eq!(turn.execution.started_at_ms, 4_242);
}

#[tokio::test]
pub(super) async fn queued_checkpoint_input_commits_before_continuing_standard_turn() {
    let double = kernel_double(SEED + 6, lash_restate_test::ServerConfig::default()).await;
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "First answer.".to_string(),
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
                    text: "Second answer.".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let (mut runtime, store) =
        standard_runtime_with_transport_and_double_queue_store(&double, transport).await;
    enqueue_turn_input_for_checkpoint(
        store.as_ref(),
        &SessionId::from("root"),
        &TurnId::from("queued-checkpoint-turn"),
        None,
        TurnInput::text("one more thing"),
    )
    .await;

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("queued-checkpoint-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .drive_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
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
                message.role == MessageRole::Assistant
                    && message
                        .parts
                        .iter()
                        .any(|part| part.content().contains("Second answer."))
            })
    );
    let admitted = active_conversation_messages(&turn.state)
        .into_iter()
        .filter(|message| {
            message.role == MessageRole::User
                && message
                    .parts
                    .iter()
                    .any(|part| part.content() == "one more thing")
        })
        .collect::<Vec<_>>();
    assert_eq!(admitted.len(), 1);
    // A normal user message that records which turn absorbed it, not a plugin or
    // process injection (FIG-972). The turn that absorbs it is the follow-on
    // physical turn (FIG-3157): the input was claimed at the terminal
    // checkpoint of `queued-checkpoint-turn`, which finished on its own
    // committed answer, so the claim drives the next turn of the same run.
    assert!(matches!(
        admitted[0].origin.as_ref(),
        Some(lash_core::MessageOrigin::TurnInput { turn_id, input_id })
            if turn_id == "queued-checkpoint-turn:agent-frame:1" && input_id.is_some()
    ));
}

#[tokio::test]
pub(super) async fn queued_checkpoint_input_preserves_images() {
    let double = kernel_double(SEED + 7, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_calls = Arc::clone(&calls);
    let transport = TestProvider::builder()
        .kind("mock")
        .complete(move |request| {
            let captured_requests = Arc::clone(&captured_requests);
            let captured_calls = Arc::clone(&captured_calls);
            async move {
                captured_requests.lock_recover().push(request);
                let call = captured_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let text = if call == 0 {
                    "First answer."
                } else {
                    "Second answer."
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
    let store = double_unbound_recording_store(&double).await;
    let mut runtime = TestRuntime::new(&backend, transport)
        .plugins(Vec::new())
        .host(test_host_config(&backend))
        .store(store.clone())
        .attachment_acceptance(
            lash_core::attachments::attachment_test_capability().attachment_acceptance,
        )
        .build()
        .await;
    enqueue_turn_input_for_checkpoint(
        store.as_ref(),
        &SessionId::from("root"),
        &TurnId::from("image-attachment-turn"),
        None,
        TurnInput::text("see image").with_attachment(lash_core::AttachmentSource::inline(
            lash_core::MediaType::parse("image/png").unwrap(),
            vec![1, 2, 3],
        )),
    )
    .await;

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("image-attachment-turn"),
        ))
        .await
        .expect("open the turn's handler");
    runtime
        .drive_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    let requests = requests.lock_recover().clone();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().any(|message| {
        message.role == lash_core::llm::types::LlmRole::User
            && message.blocks.iter().any(|block| {
                matches!(
                    block,
                    lash_core::llm::types::LlmContentBlock::Attachment { .. }
                )
            })
    }));
}

/// Boundary: active-turn checkpoint input tests stay in `turns.rs` when they
/// assert model prompt replay, plugin checkpoint hooks, injected-input stream
/// events, image materialization, or persisted conversation projection. Runtime
/// Scenarios own the host-level active-input redrive/cancel/queue invariants.
#[tokio::test]
pub(super) async fn checkpoint_hook_can_inject_messages() {
    let double = kernel_double(SEED + 8, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: Some(Arc::new(|ctx| {
                    Box::pin(async move {
                        if ctx.checkpoint == lash_core::CheckpointKind::BeforeCompletion {
                            Ok(vec![
                                lash_core::facade_support::TurnPluginDirective::EnqueueMessages(
                                    lash_core::facade_support::EnqueueMessagesDirective {
                                        messages: vec![lash_core::PluginMessage::text(
                                            lash_core::MessageRole::System,
                                            "checkpoint injected",
                                        )],
                                    },
                                ),
                            ])
                        } else {
                            Ok(Vec::new())
                        }
                    })
                })),
                presentation_steps: vec![],
                runtime_event: None,
                external_registrar: None,
            }))
        }),
    });
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "First answer.".to_string(),
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
                    text: "Second answer.".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let mut runtime = runtime_with_plugins(&backend, vec![plugin], transport).await;

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("plugin-action-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .drive_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
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
                message.role == MessageRole::System
                    && message
                        .parts
                        .iter()
                        .any(|part| part.content() == "checkpoint injected")
            })
    );
}

#[tokio::test]
pub(super) async fn checkpoint_plugin_abort_leaves_active_input_pending_without_application_evidence()
 {
    let double = kernel_double(SEED + 9, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: Some(Arc::new(|_| {
                    Box::pin(async {
                        Ok(vec![
                            lash_core::facade_support::TurnPluginDirective::AbortTurn(
                                lash_core::facade_support::AbortTurnDirective {
                                    code: "checkpoint_rejected".to_string(),
                                    message: "reject checkpoint delivery".to_string(),
                                },
                            ),
                        ])
                    })
                })),
                presentation_steps: vec![],
                runtime_event: None,
                external_registrar: None,
            }))
        }),
    });
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "first".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let store = double_unbound_recording_store(&double).await;
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![plugin],
        Arc::new(EmptyTools),
        transport,
        test_host_config(&backend),
        runtime_store,
    )
    .await;
    let admitted = enqueue_turn_input_for_checkpoint(
        store.as_ref(),
        &SessionId::from("root"),
        &TurnId::from("checkpoint-plugin-abort-turn"),
        Some("host:checkpoint-plugin-abort".to_string()),
        TurnInput::text("must remain pending"),
    )
    .await;
    let turn_events = RecordingTurnEvents::default();

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("checkpoint-plugin-abort-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .drive_turn(
            TurnInput::text("hello"),
            TurnOptions::new(CancellationToken::new(), handler.scoped())
                .with_turn_events(&turn_events),
        )
        .await
        .expect("plugin-aborted turn assembles");
    handler.close().await.expect("close the turn's handler");

    assert!(
        matches!(turn.outcome, TurnOutcome::Stopped(_)),
        "checkpoint rejection must stop the turn: {:?}",
        turn.outcome
    );
    // The turn's own input is an acceptance too (ADR 0069), so it is applied
    // and reported; what must not appear is application evidence for the input
    // this checkpoint failed to admit.
    assert!(
        turn_events
            .snapshot()
            .iter()
            .all(|activity| match &activity.event {
                lash_core::TurnEvent::QueuedInputAccepted { applications } => applications
                    .iter()
                    .all(|application| application.input_id != admitted.input_id),
                _ => true,
            }),
        "a rejected checkpoint must not emit live application evidence"
    );
    assert!(
        lash_core::store::IngressStore::list_turn_input_applications(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("list rejected checkpoint applications")
        .iter()
        .all(|application| application.input_id != admitted.input_id),
        "a rejected checkpoint must not persist application evidence"
    );
    assert!(
        lash_core::store::IngressStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("list pending input after rejected checkpoint")
        .iter()
        .any(|input| input.input.input_id == admitted.input_id),
        "a rejected checkpoint input must remain claimable"
    );
    assert!(
        active_conversation_messages(&turn.state)
            .iter()
            .all(|message| {
                message
                    .parts
                    .iter()
                    .all(|part| part.content() != "must remain pending")
            }),
        "a rejected checkpoint input must not enter canonical history"
    );
}

#[tokio::test]
pub(super) async fn checkpoint_attachment_failure_leaves_active_input_pending_without_application_evidence()
 {
    let double = kernel_double(SEED + 10, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    #[derive(Debug)]
    struct DenyHostCheckpointAttachments;

    impl lash_core::testing::runtime_internals::AttachmentSourcePolicy
        for DenyHostCheckpointAttachments
    {
        fn authorize(
            &self,
            producer: &lash_core::testing::runtime_internals::AttachmentProducer,
            _source: &lash_core::AttachmentSource,
        ) -> Result<(), lash_core::test_support::AttachmentSourcePolicyError> {
            if matches!(
                producer,
                lash_core::testing::runtime_internals::AttachmentProducer::Host
            ) {
                return Err(lash_core::test_support::AttachmentSourcePolicyError {
                    producer: producer.clone(),
                    reason: "checkpoint attachment denied for test".to_string(),
                });
            }
            Ok(())
        }
    }

    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: Some(Arc::new(|_| {
                    Box::pin(async {
                        let mut message = lash_core::PluginMessage::text(
                            lash_core::MessageRole::System,
                            "plugin upload",
                        );
                        message.parts.push(lash_core::Part::attachment_part(
                            String::new(),
                            String::new(),
                            Some(lash_core::session_model::message::PartAttachment {
                                source: lash_core::AttachmentSource::external_url(
                                    lash_core::MediaType::parse("application/pdf")
                                        .expect("valid test media type"),
                                    "https://example.test/checkpoint.pdf",
                                ),
                            }),
                        ));
                        Ok(vec![
                            lash_core::facade_support::TurnPluginDirective::EnqueueMessages(
                                lash_core::facade_support::EnqueueMessagesDirective {
                                    messages: vec![message],
                                },
                            ),
                        ])
                    })
                })),
                presentation_steps: vec![],
                runtime_event: None,
                external_registrar: None,
            }))
        }),
    });
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "first".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let store = double_unbound_recording_store(&double).await;
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![plugin],
        Arc::new(EmptyTools),
        transport,
        test_host_config(&backend),
        runtime_store,
    )
    .await;
    runtime.host.core.attachment_source_policy = Arc::new(DenyHostCheckpointAttachments);
    let admitted = enqueue_turn_input_for_checkpoint(
        store.as_ref(),
        &SessionId::from("root"),
        &TurnId::from("checkpoint-attachment-failure-turn"),
        Some("host:checkpoint-attachment-failure".to_string()),
        TurnInput::text("must remain pending after attachment failure"),
    )
    .await;
    let turn_events = RecordingTurnEvents::default();

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("checkpoint-attachment-failure-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .drive_turn(
            TurnInput::text("hello"),
            TurnOptions::new(CancellationToken::new(), handler.scoped())
                .with_turn_events(&turn_events),
        )
        .await
        .expect("attachment-failed turn assembles");
    handler.close().await.expect("close the turn's handler");

    assert!(
        matches!(turn.outcome, TurnOutcome::Stopped(_)),
        "checkpoint attachment failure must stop the turn: {:?}",
        turn.outcome
    );
    // The turn's own input is an acceptance too (ADR 0069), so it is applied
    // and reported; what must not appear is application evidence for the input
    // this checkpoint failed to admit.
    assert!(
        turn_events
            .snapshot()
            .iter()
            .all(|activity| match &activity.event {
                lash_core::TurnEvent::QueuedInputAccepted { applications } => applications
                    .iter()
                    .all(|application| application.input_id != admitted.input_id),
                _ => true,
            }),
        "a failed checkpoint attachment must not emit live application evidence"
    );
    assert!(
        lash_core::store::IngressStore::list_turn_input_applications(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("list attachment-failed checkpoint applications")
        .iter()
        .all(|application| application.input_id != admitted.input_id),
        "a failed checkpoint attachment must not persist application evidence"
    );
    assert!(
        lash_core::store::IngressStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("list pending input after attachment failure")
        .iter()
        .any(|input| input.input.input_id == admitted.input_id),
        "an attachment-failed checkpoint input must remain claimable"
    );
    assert!(
        active_conversation_messages(&turn.state)
            .iter()
            .all(|message| {
                message
                    .parts
                    .iter()
                    .all(|part| part.content() != "must remain pending after attachment failure")
            }),
        "an attachment-failed checkpoint input must not enter canonical history"
    );
}

#[tokio::test]
pub(super) async fn queued_checkpoint_input_accepts_and_persists_one_normal_user_message() {
    let double = kernel_double(SEED + 11, lash_restate_test::ServerConfig::default()).await;
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "first".to_string(),
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
                    text: "answer".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let (mut runtime, store) =
        standard_runtime_with_transport_and_double_queue_store(&double, transport).await;
    enqueue_turn_input_for_checkpoint(
        store.as_ref(),
        &SessionId::from("root"),
        &TurnId::from("injection-accepted-turn"),
        Some("host:follow-up-id".to_string()),
        TurnInput::text("follow up"),
    )
    .await;
    let sink = RecordingSink::default();
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("injection-accepted-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let assembled = runtime
        .drive_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            TurnOptions::new(CancellationToken::new(), handler.scoped()).with_events(&sink),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    let mut saw_injected_accept = false;
    for event in sink.snapshot() {
        if let lash_core::facade_support::SessionStreamEvent::InjectedTurnInputAccepted {
            inputs,
            ..
        } = event
        {
            saw_injected_accept = inputs.iter().any(|input| {
                input.id.as_deref() == Some("follow-up-id")
                    && input.message.role == lash_core::MessageRole::User
                    && input
                        .message
                        .parts
                        .iter()
                        .any(|part| part.content() == "follow up")
            });
        }
    }
    assert!(
        saw_injected_accept,
        "expected injected turn input accepted event"
    );

    let projected = active_conversation_messages(&assembled.state);
    let follow_up_count = projected
        .iter()
        .filter(|message| {
            message.role == lash_core::MessageRole::User
                && message
                    .parts
                    .iter()
                    .any(|part| part.content() == "follow up")
        })
        .count();
    assert_eq!(
        follow_up_count, 1,
        "injected active-turn input must persist exactly once in history"
    );
    let follow_up = projected
        .iter()
        .find(|message| {
            message.role == lash_core::MessageRole::User
                && message
                    .parts
                    .iter()
                    .any(|part| part.content() == "follow up")
        })
        .expect("committed injected input");
    // The injected input keeps the normal user-message representation — no
    // plugin or process origin — and records which turn absorbed it and which
    // durable input it came from (FIG-972).
    let lash_core::MessageOrigin::TurnInput { turn_id, input_id } = follow_up
        .origin
        .as_ref()
        .expect("committed injected input carries turn-input provenance")
    else {
        panic!("injected input must use the normal user-message representation");
    };
    // FIG-3157: claimed at the terminal checkpoint, absorbed by the follow-on
    // physical turn rather than by the turn that had already finished.
    assert_eq!(turn_id, "injection-accepted-turn:agent-frame:1");
    let input_id = input_id
        .as_deref()
        .expect("queued ingress records the durable input id");
    assert_eq!(
        follow_up.id,
        lash_core::runtime::ingress_message_id(input_id)
    );
    let opening = projected
        .iter()
        .find(|message| {
            message.role == lash_core::MessageRole::User
                && message.parts.iter().any(|part| part.content() == "hello")
        })
        .expect("committed opening input");
    // The opening input entered the same way (ADR 0069): its own acceptance,
    // its own durable id, distinct from the one injected at the checkpoint.
    let lash_core::MessageOrigin::TurnInput {
        turn_id: opening_turn_id,
        input_id: opening_input_id,
    } = opening
        .origin
        .as_ref()
        .expect("committed opening input carries turn-input provenance")
    else {
        panic!("the opening input must use the normal user-message representation");
    };
    assert_eq!(opening_turn_id, "injection-accepted-turn");
    let opening_input_id = opening_input_id
        .as_deref()
        .expect("a direct turn is admitted durably before it drives");
    assert_ne!(opening_input_id, input_id);
    assert_eq!(
        opening.id,
        lash_core::runtime::ingress_message_id(opening_input_id)
    );
}

pub(super) async fn commit_checkpoint_injected_turn_for_redrive(
    double: &lash_restate_test::RestateTestBackend,
    store: Arc<RecordingStore>,
    controller: Arc<dyn lash_core::testing::EffectLayer>,
    turn_id: &TurnId,
) -> (
    lash_core::TurnInput,
    lash_core::facade_support::TurnInputAcceptanceReceipt,
) {
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "first answer".to_string(),
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
                    text: "answer after injection".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let backend = double.lash_backend();
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store.clone();
    let mut runtime = Box::pin(runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        journal_replay_host(&backend, Arc::clone(&controller)),
        runtime_store,
    ))
    .await;
    enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "queued before opening",
    )
    .await;
    enqueue_turn_input_for_checkpoint(
        store.as_ref(),
        &SessionId::from("root"),
        turn_id,
        Some(format!("host:{turn_id}:injection")),
        TurnInput::text("mid-turn injection"),
    )
    .await;
    let input = TurnInput::text("opening input");
    let handler = double
        .open_handler(lash_core::AdmittedScope::turn("root", turn_id))
        .await
        .expect("open the scope's handler");
    let scope = lash_core::testing::LayeredEffectHost::layer_scoped(
        handler.scoped(),
        Arc::clone(&controller),
    )
    .expect("layer the handler's scope");
    // FIG-3157: work admitted at the terminal checkpoint drives a
    // follow-on physical turn, so the run holds two turns. The acceptance
    // belongs to the admitted turn, which is the run's first one; the run
    // carries the same identity for callers that do not index turns.
    let committed = runtime
        .drive_turn_frames(
            input.clone(),
            TurnOptions::new(CancellationToken::new(), scope),
        )
        .await
        .expect("commit the checkpoint-injected turn");
    handler.close().await.expect("close the scope's handler");
    let acceptance = committed
        .acceptance
        .expect("the committed direct turn exposes its acceptance");
    (input, acceptance)
}

pub(super) async fn redrive_checkpoint_injected_turn(
    double: &lash_restate_test::RestateTestBackend,
    store: Arc<dyn lash_core::RuntimePersistence>,
    controller: Arc<dyn lash_core::testing::EffectLayer>,
    turn_id: &TurnId,
    input: TurnInput,
) -> Result<lash_core::facade_support::AssembledTurn, lash_core::RuntimeError> {
    let backend = double.lash_backend();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        journal_replay_host(&backend, Arc::clone(&controller)),
        store,
    )
    .await;
    let handler = double
        .open_handler(lash_core::AdmittedScope::turn("root", turn_id))
        .await
        .expect("open the scope's handler");
    let scope = lash_core::testing::LayeredEffectHost::layer_scoped(handler.scoped(), controller)
        .expect("layer the handler's scope");
    // FIG-3157: the run holds the admitted turn plus the follow-on turn the
    // terminal-checkpoint admission drives. The acceptance identity belongs to
    // the admitted turn, so that is the one returned here.
    let run = runtime
        .drive_turn_frames(input, TurnOptions::new(CancellationToken::new(), scope))
        .await?;
    handler.close().await.expect("close the scope's handler");
    Ok(run
        .turns
        .into_iter()
        .next()
        .expect("a redriven run assembles its admitted turn"))
}

#[tokio::test]
pub(super) async fn checkpoint_injected_turn_redrive_replays_the_original_commit_identity() {
    let double = kernel_double(SEED + 17, lash_restate_test::ServerConfig::default()).await;
    let turn_id = &TurnId::from("checkpoint-injected-redrive");
    let store = double_unbound_recording_store(&double).await;
    let controller: Arc<dyn lash_core::testing::EffectLayer> =
        Arc::new(JournalReplayEffectController::default());
    let (input, first_acceptance) = commit_checkpoint_injected_turn_for_redrive(
        &double,
        Arc::clone(&store),
        Arc::clone(&controller),
        turn_id,
    )
    .await;
    let first_applications = lash_core::store::IngressStore::list_turn_input_applications(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("read first turn applications");
    assert_eq!(first_applications.len(), 2);
    // The root's admission composes next-turn rows only (FIG-3927 §2.2): the
    // mid-turn injection is addressed to the turn named by the opening
    // acceptance's source key, which the root composed as a member, so that
    // turn never runs and never reaches a checkpoint. The root's terminal
    // write ends that turn too (FIG-3946): it re-opens the injection as
    // next-turn input at its own position, for the session's next root.
    let injection = lash_core::store::IngressStore::list_pending_turn_inputs(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("list pending turn inputs")
    .into_iter()
    .find(|read| {
        read.input.source_key.as_deref() == Some(format!("host:{turn_id}:injection").as_str())
    })
    .expect("the mid-turn injection is still pending");
    assert_eq!(
        injection.status,
        lash_core::PendingTurnInputReadStatus::Open,
        "the ended root released the injection: it is bound to no root"
    );
    assert_eq!(
        injection.input.state.kind(),
        lash_core::TurnInputStateKind::DeferredNextTurn,
        "the injection no longer names the turn that never ran: {:?}",
        injection.input.state
    );
    assert!(
        first_applications
            .iter()
            .all(|application| application.input_id != injection.input.input_id),
        "the ended root did not apply the injection"
    );
    assert_eq!(
        first_applications
            .iter()
            .filter(|application| application.checkpoint.is_some())
            .count(),
        0,
        "no input was admitted at a checkpoint"
    );

    let replay_store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(JournalRedriveStore {
        inner: Arc::clone(&store),
    });
    let replayed = Box::pin(redrive_checkpoint_injected_turn(
        &double,
        replay_store,
        Arc::clone(&controller),
        turn_id,
        input,
    ))
    .await
    .expect("a checkpoint-injected turn redrive must replay the original commit receipt");

    assert_eq!(
        replayed.turn_input_acceptance.as_ref(),
        Some(&first_acceptance),
        "redrive must retain the journaled acceptance identity"
    );
    assert_eq!(
        lash_core::store::IngressStore::list_turn_input_applications(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("read applications after redrive"),
        first_applications,
        "redrive must preserve the original initial/checkpoint application split"
    );

    // The session's next root admits the re-opened injection as its input.
    let next_turn = TurnId::from("checkpoint-injected-next-root");
    Box::pin(drive_root_after_checkpoint_injection(
        &double,
        Arc::clone(&store),
        &next_turn,
    ))
    .await;
    let applications = lash_core::store::IngressStore::list_turn_input_applications(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("read applications after the next root");
    let applied = applications
        .iter()
        .find(|application| application.input_id == injection.input.input_id)
        .expect("the next root applies the injection");
    assert_eq!(
        applied.checkpoint, None,
        "the next root admits the injection as next-turn input"
    );
    assert_ne!(
        applied.turn_id, *turn_id,
        "the injection is applied by a turn that ran, not the one that never did"
    );
    assert!(
        lash_core::store::IngressStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root"),
        )
        .await
        .expect("list pending turn inputs after the next root")
        .iter()
        .all(|read| read.input.input_id != injection.input.input_id),
        "the next root settled the injection"
    );
}

async fn drive_root_after_checkpoint_injection(
    double: &lash_restate_test::RestateTestBackend,
    store: Arc<RecordingStore>,
    turn_id: &TurnId,
) {
    let controller: Arc<dyn lash_core::testing::EffectLayer> =
        Arc::new(JournalReplayEffectController::default());
    let backend = double.lash_backend();
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store;
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "answer to the injection".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        }]),
        journal_replay_host(&backend, Arc::clone(&controller)),
        runtime_store,
    )
    .await;
    let handler = double
        .open_handler(lash_core::AdmittedScope::turn("root", turn_id))
        .await
        .expect("open the next root's handler");
    let scope = lash_core::testing::LayeredEffectHost::layer_scoped(handler.scoped(), controller)
        .expect("layer the next root's scope");
    runtime
        .drive_turn_frames(
            TurnInput::text("after the injection"),
            TurnOptions::new(CancellationToken::new(), scope),
        )
        .await
        .expect("the next root commits");
    handler
        .close()
        .await
        .expect("close the next root's handler");
}

#[tokio::test]
pub(super) async fn accepted_input_withdrawn_before_its_drive_cedes() {
    let double = kernel_double(SEED + 18, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let turn_id = &TurnId::from("accepted-input-withdrawn");
    let store = double_unbound_recording_store(&double).await;
    let controller: Arc<dyn lash_core::testing::EffectLayer> =
        Arc::new(JournalReplayEffectController::default());
    let withdrawing: Arc<dyn lash_core::RuntimePersistence> = Arc::new(WithdrawBeforeDriveStore {
        inner: Arc::clone(&store),
    });
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        journal_replay_host(&backend, Arc::clone(&controller)),
        withdrawing,
    )
    .await;
    let handler = double
        .open_handler(lash_core::AdmittedScope::turn("root", turn_id))
        .await
        .expect("open the scope's handler");
    let scope = lash_core::testing::LayeredEffectHost::layer_scoped(handler.scoped(), controller)
        .expect("layer the handler's scope");

    let error = runtime
        .drive_turn_frames(
            TurnInput::text("withdrawn out from under the acceptance"),
            TurnOptions::new(CancellationToken::new(), scope),
        )
        .await
        .expect_err("a drive that finds its accepted row withdrawn cedes");
    handler.close().await.expect("close the scope's handler");

    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::AcceptedTurnInputCeded,
        "{error:?}"
    );
    let pending = lash_core::store::IngressStore::list_pending_turn_inputs(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("read pending inputs");
    assert!(
        pending.is_empty(),
        "the withdrawn row is not re-admitted: {pending:?}"
    );
    assert!(
        lash_core::store::IngressStore::list_turn_input_applications(
            store.as_ref(),
            &SessionId::from("root"),
        )
        .await
        .expect("read applications")
        .is_empty(),
        "the ceding turn commits nothing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn active_input_after_last_call_is_first_admitted_on_next_turn() {
    let double = kernel_double(SEED + 12, lash_restate_test::ServerConfig::default()).await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_calls = Arc::clone(&calls);
    let transport = TestProvider::builder()
        .kind("after-last-call-ingress")
        .complete(move |request| {
            let captured_requests = Arc::clone(&captured_requests);
            let captured_calls = Arc::clone(&captured_calls);
            async move {
                captured_requests
                    .lock_recover()
                    .push(request.messages.clone());
                let text = match captured_calls.fetch_add(1, Ordering::SeqCst) {
                    0 => "first turn complete",
                    1 => "deferred input complete",
                    other => panic!("unexpected provider call {other}"),
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
    let (mut runtime, store) =
        standard_runtime_with_transport_and_double_queue_store(&double, transport).await;
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    runtime.set_turn_phase_probe(Arc::new(PauseAtPreparedTurn {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    }));

    let task_double = double.clone();
    let first_turn = lash_core::task::spawn(async move {
        let handler = task_double
            .open_handler(AdmittedScope::turn(
                SessionId::from("root"),
                TurnId::from("after-last-call-turn"),
            ))
            .await
            .expect("open the turn's handler");
        runtime
            .drive_turn(
                TurnInput::text("first turn input"),
                lash_core::facade_support::TurnOptions::new(
                    CancellationToken::new(),
                    handler.scoped(),
                ),
            )
            .await
            .expect("first turn");
        handler.close().await.expect("close the turn's handler");
        runtime
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("turn reaches finalization after its last call");
    enqueue_turn_input_for_checkpoint(
        store.as_ref(),
        &SessionId::from("root"),
        &TurnId::from("after-last-call-turn"),
        Some("host:late-active".to_string()),
        TurnInput::text("late active input"),
    )
    .await;
    release.store(true, Ordering::SeqCst);
    let mut runtime = first_turn.await.expect("first turn task");

    let pending = lash_core::store::IngressStore::list_pending_turn_inputs(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("deferred late input");
    assert_eq!(pending.len(), 1);
    assert!(matches!(
        pending[0].input.ingress(),
        lash_core::TurnInputIngress::NextTurn
    ));
    assert_eq!(
        pending[0].input.state,
        lash_core::TurnInputState::DeferredNextTurn
    );

    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from("root"),
            "late-active-next-turn",
        ))
        .await
        .expect("open the drain's handler");
    runtime
        .drive_one_admitted_queued_root(TurnOptions::new(
            CancellationToken::new(),
            handler.scoped(),
        ))
        .await
        .expect("drain deferred input")
        .ran()
        .expect("deferred input starts a turn");
    handler.close().await.expect("close the drain's handler");

    let requests = requests.lock_recover();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        serde_json::to_string(&requests[1]).expect("serialize next-turn first-call messages"),
        r#"[{"role":"User","starts_user_segment":true,"blocks":[{"Text":{"text":"first turn input","response_meta":null,"cache_breakpoint":false}}]},{"role":"Assistant","blocks":[{"Text":{"text":"first turn complete","response_meta":null,"cache_breakpoint":false}}]},{"role":"User","starts_user_segment":true,"blocks":[{"Text":{"text":"late active input","response_meta":null,"cache_breakpoint":false}}]}]"#
    );
}

// Boundary: Runtime Scenarios own command-only queue completion at the store
// layer. This full runtime test stays here to assert the public scheduler API:
// command-only work returns `None` rather than fabricating a turn.
#[tokio::test]
pub(super) async fn command_only_queued_work_drain_completes_without_turn() {
    let double = kernel_double(SEED + 13, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) =
        standard_runtime_with_transport_and_double_queue_store(&double, mock_provider(Vec::new()))
            .await;
    let command =
        enqueue_session_command(store.as_ref(), &SessionId::from("root"), "test refresh").await;

    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from("root"),
            "command-only-queue-drain",
        ))
        .await
        .expect("open the drain's handler");
    let drained = runtime
        .drive_one_admitted_queued_root(TurnOptions::new(
            CancellationToken::new(),
            handler.scoped(),
        ))
        .await
        .expect("command-only drain succeeds")
        .ran();
    handler.close().await.expect("close the drain's handler");

    assert!(drained.is_none());
    assert!(
        lash_core::store::IngressStore::list_queued_work(store.as_ref(), &SessionId::from("root"))
            .await
            .expect("list queue after command-only drain")
            .is_empty(),
        "command batch `{}` should be completed",
        command.batch_id
    );
}

// The process-wake and active-checkpoint tests exercise the engine drive and
// its provider-visible turn, while the selected-batch invariant remains in
// runtime persistence conformance.
#[tokio::test]
pub(super) async fn next_turn_input_turn_claims_process_wake_at_active_checkpoint() {
    let double = kernel_double(SEED + 14, lash_restate_test::ServerConfig::default()).await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_calls = Arc::clone(&calls);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |req| {
            let captured_requests = Arc::clone(&captured_requests);
            let captured_calls = Arc::clone(&captured_calls);
            async move {
                captured_requests.lock_recover().push(req);
                let call = captured_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let text = if call == 0 {
                    "turn input response"
                } else {
                    "wake checkpoint response"
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
    let (mut runtime, store) =
        standard_runtime_with_transport_and_double_queue_store(&double, transport).await;
    let queued_input = enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "queued user input",
    )
    .await;
    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    let target_scope = lash_core::SessionScope::new("root");
    let registered = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::ProcessProvenance::session(target_scope.clone()),
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
                "text": "wake should wait",
                "value": {
                    "status": "wake should wait"
                }
            }),
        ),
    )
    .await;

    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from("root"),
            "next-input-before-wake-drain",
        ))
        .await
        .expect("open the drain's handler");
    let drained = runtime
        .drive_one_admitted_queued_root(TurnOptions::new(
            CancellationToken::new(),
            handler.scoped(),
        ))
        .await
        .expect("queued drain succeeds")
        .ran()
        .expect("pending turn input drains first");
    handler.close().await.expect("close the drain's handler");

    assert_eq!(
        drained.assistant_output.safe_text,
        "wake checkpoint response"
    );
    assert!(
        lash_core::store::IngressStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("pending inputs after drain")
        .is_empty(),
        "turn input `{}` should be completed",
        queued_input.input_id
    );
    assert!(
        lash_core::store::IngressStore::list_queued_work(store.as_ref(), &SessionId::from("root"))
            .await
            .expect("queued work after pending input drain")
            .is_empty(),
        "process wake `{}` should be claimed at the user-input turn checkpoint",
        wake.wake_id
    );

    let requests = requests.lock_recover().clone();
    assert_eq!(requests.len(), 2);
    assert!(request_contains_text(&requests[0], "queued user input"));
    assert!(!request_contains_text(&requests[0], "wake should wait"));
    assert!(request_contains_text(&requests[1], "queued user input"));
    assert!(request_contains_text(&requests[1], "wake should wait"));
}

#[tokio::test]
pub(super) async fn wake_claimed_at_a_terminal_checkpoint_drives_a_follow_on_turn() {
    let double = kernel_double(SEED + 15, lash_restate_test::ServerConfig::default()).await;
    // FIG-3157: a terminal finish ends the turn. A wake claimed at the
    // `BeforeCompletion` checkpoint never extends it — the committed answer
    // stays the turn's answer, and the claim is carried into a follow-on
    // physical turn of the same logical run: no idle gap, no wait for the
    // user, and the session execution lease held across the seam so the
    // claim stays generation-valid (ADR 0029).
    const SESSION_ID: &str = "terminal-checkpoint-follow-on";

    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_calls = Arc::clone(&calls);
    // Filled after the runtime is built; the provider only reads it when a
    // call arrives, which is strictly later.
    let store_cell: Arc<Mutex<Option<Arc<RecordingStore>>>> = Arc::new(Mutex::new(None));
    let captured_store_cell = Arc::clone(&store_cell);
    // The lane identity a provider call observed: executor id and generation.
    type ObservedLease = Option<(String, u64)>;
    let observed_leases: Arc<Mutex<Vec<ObservedLease>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_observed_leases = Arc::clone(&observed_leases);
    // The wake arrives while the foreground turn runs: accepted after the
    // turn's input, so the turn lane admits the input first (ADR 0101 §5)
    // and the wake reaches the turn only at a checkpoint. Filled once the
    // process is registered; the provider appends it on its first call.
    type WakeSource = (
        Arc<dyn lash_core::ProcessRegistry>,
        Arc<RecordingStore>,
        ProcessId,
    );
    let wake_source: Arc<Mutex<Option<WakeSource>>> = Arc::new(Mutex::new(None));
    let captured_wake_source = Arc::clone(&wake_source);
    let appended_wake: Arc<Mutex<Option<lash_core::ProcessWakeDelivery>>> =
        Arc::new(Mutex::new(None));
    let captured_appended_wake = Arc::clone(&appended_wake);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |req| {
            let captured_requests = Arc::clone(&captured_requests);
            let captured_calls = Arc::clone(&captured_calls);
            let captured_store_cell = Arc::clone(&captured_store_cell);
            let captured_observed_leases = Arc::clone(&captured_observed_leases);
            let captured_wake_source = Arc::clone(&captured_wake_source);
            let captured_appended_wake = Arc::clone(&captured_appended_wake);
            async move {
                captured_requests.lock_recover().push(req);
                let source = captured_wake_source.lock_recover().take();
                if let Some((registry, store, process)) = source {
                    let wake = append_process_wake_to_queue(
                        registry.as_ref(),
                        store.as_ref(),
                        &process,
                        lash_core::ProcessEventAppendRequest::new(
                            "process.wake",
                            json!({
                                "text": "wake at the terminal boundary",
                                "value": {
                                    "status": "wake at the terminal boundary"
                                }
                            }),
                        ),
                    )
                    .await;
                    *captured_appended_wake.lock_recover() = Some(wake);
                }
                let store = captured_store_cell.lock_recover().clone();
                let observed = match store {
                    Some(store) => {
                        let epoch = lash_core::store::DriveEpochStore::drive_epoch(
                            store.as_ref(),
                            &SessionId::from(SESSION_ID),
                        )
                        .await
                        .expect("read the sealed drive epoch");
                        epoch
                            .admission
                            .map(|admission| (admission.as_str().to_owned(), epoch.epoch))
                    }
                    None => None,
                };
                captured_observed_leases.lock_recover().push(observed);
                let call = captured_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let text = if call == 0 {
                    "committed answer"
                } else {
                    "wake answer"
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
    let (mut runtime, store) = standard_runtime_with_transport_and_double_queue_store_for_session(
        &double,
        transport,
        &SessionId::from(SESSION_ID),
    )
    .await;
    *store_cell.lock_recover() = Some(Arc::clone(&store));
    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    let target_scope = lash_core::SessionScope::new(SESSION_ID);
    let registered = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::ProcessProvenance::session(target_scope.clone()),
                lash_core::Lifetime::Detached,
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(target_scope.session_id.clone())),
        )
        .await
        .expect("register wake process");
    *wake_source.lock_recover() = Some((
        Arc::clone(&registry),
        Arc::clone(&store),
        registered.id.clone(),
    ));

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from(SESSION_ID),
            TurnId::from("terminal-checkpoint-follow-on-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let run = runtime
        .drive_turn_frames(
            TurnInput::text("hello"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("the terminal-checkpoint wake drives its own follow-on turn");
    handler.close().await.expect("close the turn's handler");
    let wake = appended_wake
        .lock_recover()
        .clone()
        .expect("the wake arrived during the foreground turn");

    // One logical run, two physical turns, and the first one is a finish —
    // not a frame switch, and not a turn that was re-prompted into a second
    // terminal answer.
    assert_eq!(run.turns.len(), 2, "the run holds the follow-on turn");
    assert!(
        matches!(run.turns[0].outcome, TurnOutcome::Finished(_)),
        "the foreground turn finishes on its own answer: {:?}",
        run.turns[0].outcome
    );
    assert_eq!(run.turns[0].assistant_output.safe_text, "committed answer");
    assert_eq!(run.turns[1].assistant_output.safe_text, "wake answer");

    // The committed finish is the turn's answer and is rendered: one
    // assistant message per physical turn, neither replacing the other.
    let projected = active_conversation_messages(&run.turns[1].state);
    for answer in ["committed answer", "wake answer"] {
        assert_eq!(
            projected
                .iter()
                .filter(|message| message.role == MessageRole::Assistant
                    && message
                        .parts
                        .iter()
                        .any(|part| part.content().contains(answer)))
                .count(),
            1,
            "`{answer}` must be rendered exactly once"
        );
    }

    // The wake is the follow-on turn's input, not part of the turn that had
    // already committed its answer.
    let requests = requests.lock_recover().clone();
    assert_eq!(requests.len(), 2);
    assert!(!request_contains_text(
        &requests[0],
        "wake at the terminal boundary"
    ));
    assert!(request_contains_text(
        &requests[1],
        "wake at the terminal boundary"
    ));

    // No idle gap: the wake was drained inside this run, with no drain call
    // and no further user input.
    assert!(
        lash_core::store::IngressStore::list_queued_work(
            store.as_ref(),
            &SessionId::from(SESSION_ID)
        )
        .await
        .expect("queued work after the follow-on turn")
        .is_empty(),
        "wake `{}` should be completed by the follow-on turn",
        wake.wake_id
    );

    // The lease is the same lane at the same generation on both sides of the
    // terminal boundary: it was never released between the two turns.
    let observed_leases = observed_leases.lock_recover().clone();
    assert_eq!(observed_leases.len(), 2);
    let held_before = observed_leases[0]
        .as_ref()
        .expect("the foreground turn holds the session execution lease");
    let held_after = observed_leases[1]
        .as_ref()
        .expect("the follow-on turn still holds the session execution lease");
    assert_eq!(
        held_before, held_after,
        "the session execution lease must be held across the terminal boundary"
    );
}

/// Where the run that withheld a wake for its FIG-3157 follow-on stops short
/// of that follow-on's commit.
#[derive(Clone, Copy, Debug)]
enum WithheldFollowOnFailure {
    /// The follow-on turn's own commit is refused for good.
    FollowOnCommit,
    /// The commit that withheld the wake fails its post-commit delivery, so
    /// the follow-on never starts.
    PostCommitDelivery,
}

/// FIG-3157 under FIG-3927: a follow-on that will not commit leaves no
/// withheld row bound to its root.
///
/// The wake the terminal checkpoint withheld is bound to the root, and the
/// commit that withheld it wrote no terminal because it owed the follow-on.
/// When the run stops before that follow-on commits, the root ends at the
/// turn whose answer it committed: it has terminal evidence, it is no longer
/// the session's unfinished root, and the wake is open again at its own
/// position, so the session's next drive admits it in a root of its own and
/// completes it.
async fn a_follow_on_that_cannot_commit_leaves_no_withheld_row_bound(
    seed: u64,
    session_id: &'static str,
    failure: WithheldFollowOnFailure,
) {
    let double = kernel_double(seed, lash_restate_test::ServerConfig::default()).await;
    let root = TurnId::from(format!("{session_id}-turn"));
    let store_cell: Arc<Mutex<Option<Arc<RecordingStore>>>> = Arc::new(Mutex::new(None));
    let captured_store_cell = Arc::clone(&store_cell);
    type WakeSource = (
        Arc<dyn lash_core::ProcessRegistry>,
        Arc<RecordingStore>,
        ProcessId,
    );
    let wake_source: Arc<Mutex<Option<WakeSource>>> = Arc::new(Mutex::new(None));
    let captured_wake_source = Arc::clone(&wake_source);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_calls = Arc::clone(&calls);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let captured_store_cell = Arc::clone(&captured_store_cell);
            let captured_wake_source = Arc::clone(&captured_wake_source);
            let captured_calls = Arc::clone(&captured_calls);
            async move {
                let source = captured_wake_source.lock_recover().take();
                if let Some((registry, store, process)) = source {
                    append_process_wake_to_queue(
                        registry.as_ref(),
                        store.as_ref(),
                        &process,
                        lash_core::ProcessEventAppendRequest::new(
                            "process.wake",
                            json!({
                                "text": "withheld wake",
                                "value": { "status": "withheld wake" }
                            }),
                        ),
                    )
                    .await;
                }
                let call = captured_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call == 1
                    && matches!(failure, WithheldFollowOnFailure::FollowOnCommit)
                    && let Some(store) = captured_store_cell.lock_recover().clone()
                {
                    store.fail_next_runtime_commit(lash_core::StoreError::RecordEncodingFailed {
                        record_kind: "turn commit".to_string(),
                        message: "injected follow-on commit refusal".to_string(),
                    });
                }
                let text = if call == 0 {
                    "committed answer"
                } else {
                    "wake answer"
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
    let persisted_failed = Arc::new(AtomicBool::new(false));
    let plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>> = match failure {
        WithheldFollowOnFailure::FollowOnCommit => Vec::new(),
        WithheldFollowOnFailure::PostCommitDelivery => {
            let persisted_failed = Arc::clone(&persisted_failed);
            vec![Arc::new(RuntimeTestPluginFactory {
                build: Arc::new(move |_| {
                    let persisted_failed = Arc::clone(&persisted_failed);
                    Ok(Arc::new(RuntimeTestPlugin {
                        before_turn: None,
                        checkpoint: None,
                        presentation_steps: vec![],
                        runtime_event: Some(Arc::new(move |event| {
                            let persisted_failed = Arc::clone(&persisted_failed);
                            Box::pin(async move {
                                if matches!(
                                    event,
                                    lash_core::facade_support::PluginLifecycleEvent::TurnPersisted(
                                        _
                                    )
                                ) && !persisted_failed.swap(true, Ordering::SeqCst)
                                {
                                    return Err(lash_core::PluginError::Session(
                                        "injected post-commit delivery failure".to_string(),
                                    ));
                                }
                                Ok(())
                            })
                        })),
                        external_registrar: None,
                    }))
                }),
            })]
        }
    };
    let backend = double.lash_backend();
    let store = double_unbound_recording_store(&double).await;
    let mut runtime = TestRuntime::new(&backend, transport)
        .tools(Arc::new(EmptyTools))
        .plugins(plugins)
        .host(test_host_config(&backend))
        .store(store.clone())
        .with_session_id(session_id)
        .build()
        .await;
    *store_cell.lock_recover() = Some(Arc::clone(&store));
    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    let target_scope = lash_core::SessionScope::new(session_id);
    let registered = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::ProcessProvenance::session(target_scope.clone()),
                lash_core::Lifetime::Detached,
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(target_scope.session_id.clone())),
        )
        .await
        .expect("register wake process");
    *wake_source.lock_recover() = Some((
        Arc::clone(&registry),
        Arc::clone(&store),
        registered.id.clone(),
    ));

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from(session_id),
            root.clone(),
        ))
        .await
        .expect("open the turn's handler");
    let run = runtime
        .drive_turn_frames(
            TurnInput::text("hello"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("the committed turn is the run's answer");
    handler.close().await.expect("close the turn's handler");
    assert_eq!(
        run.turns.len(),
        1,
        "{failure:?}: only the answered turn committed"
    );
    assert_eq!(run.turns[0].assistant_output.safe_text, "committed answer");
    assert!(
        !run.turns[0].errors.is_empty(),
        "{failure:?}: the answered turn reports the follow-on's failure"
    );

    let session = SessionId::from(session_id);
    let terminal = lash_core::store::RootStore::root_terminal(store.as_ref(), &session, &root)
        .await
        .expect("read the root's terminal");
    assert!(
        terminal.is_some(),
        "{failure:?}: the root ends at the turn whose answer it committed"
    );
    assert!(
        lash_core::store::RootStore::unfinished_root(store.as_ref(), &session)
            .await
            .expect("read the unfinished root")
            .is_none(),
        "{failure:?}: no unfinished root holds the session"
    );
    let open = lash_core::store::IngressStore::list_open_queued_work(store.as_ref(), &session)
        .await
        .expect("list open queued work");
    assert_eq!(
        open.len(),
        1,
        "{failure:?}: the withheld wake is open again at its own position"
    );

    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            session.clone(),
            format!("{session_id}-redrive"),
        ))
        .await
        .expect("open the drain's handler");
    runtime
        .drive_one_admitted_queued_root(TurnOptions::new(
            CancellationToken::new(),
            handler.scoped(),
        ))
        .await
        .expect("the session's next drive admits the wake");
    handler.close().await.expect("close the drain's handler");
    assert!(
        lash_core::store::IngressStore::list_queued_work(store.as_ref(), &session)
            .await
            .expect("list queued work after the redrive")
            .is_empty(),
        "{failure:?}: the next drive completes the wake in a root of its own"
    );
}

#[tokio::test]
pub(super) async fn a_follow_on_whose_commit_is_refused_leaves_no_withheld_row_bound() {
    Box::pin(a_follow_on_that_cannot_commit_leaves_no_withheld_row_bound(
        SEED + 19,
        "withheld-follow-on-commit-refused",
        WithheldFollowOnFailure::FollowOnCommit,
    ))
    .await;
}

#[tokio::test]
pub(super) async fn a_withheld_commit_whose_delivery_fails_leaves_no_withheld_row_bound() {
    Box::pin(a_follow_on_that_cannot_commit_leaves_no_withheld_row_bound(
        SEED + 20,
        "withheld-follow-on-delivery-failed",
        WithheldFollowOnFailure::PostCommitDelivery,
    ))
    .await;
}

#[tokio::test]
pub(super) async fn process_wake_claimed_at_checkpoint_is_completed_when_turn_is_cancelled() {
    let double = kernel_double(SEED + 16, lash_restate_test::ServerConfig::default()).await;
    // Keep this cancellation rendezvous out of the shared `root` lane so unrelated libtest
    // cases cannot make its final commit contend with their turn.
    const SESSION_ID: &str = "process-wake-cancelled";

    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_calls = Arc::clone(&calls);
    let (wake_started_tx, wake_started_rx) = tokio::sync::oneshot::channel::<()>();
    let wake_started_tx = Arc::new(Mutex::new(Some(wake_started_tx)));
    let captured_wake_started_tx = Arc::clone(&wake_started_tx);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |req| {
            let captured_requests = Arc::clone(&captured_requests);
            let captured_calls = Arc::clone(&captured_calls);
            let captured_wake_started_tx = Arc::clone(&captured_wake_started_tx);
            async move {
                captured_requests.lock_recover().push(req);
                let call = captured_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call == 0 {
                    return Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "initial queued input response".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    });
                }
                if let Some(tx) = captured_wake_started_tx.lock_recover().take() {
                    let _ = tx.send(());
                }
                std::future::pending::<Result<LlmResponse, _>>().await
            }
        })
        .build();
    let (mut runtime, store) = standard_runtime_with_transport_and_double_queue_store_for_session(
        &double,
        transport,
        &SessionId::from(SESSION_ID),
    )
    .await;
    let queued_input = enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from(SESSION_ID),
        "cancel with wake pending",
    )
    .await;
    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    let target_scope = lash_core::SessionScope::new(SESSION_ID);
    let registered = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::ProcessProvenance::session(target_scope.clone()),
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
                "text": "wake cancelled in checkpoint",
                "value": {
                    "status": "wake cancelled in checkpoint"
                }
            }),
        ),
    )
    .await;
    let cancel = CancellationToken::new();
    let cancel_after_wake_started = cancel.clone();
    let canceller = lash_core::task::spawn(async move {
        wake_started_rx
            .await
            .expect("wake provider call should start");
        cancel_after_wake_started.cancel();
    });

    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from(SESSION_ID),
            "cancel-claimed-wake-drain",
        ))
        .await
        .expect("open the drain's handler");
    let drained = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        runtime.drive_one_admitted_queued_root(TurnOptions::new(cancel, handler.scoped())),
    )
    .await
    .expect("cancelled wake drain should finish")
    .expect("cancelled wake drain should not error")
    .ran()
    .expect("cancelled queued input turn should still assemble");
    handler.close().await.expect("close the drain's handler");
    canceller.await.expect("canceller task");

    assert!(matches!(
        drained.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    assert!(
        lash_core::store::IngressStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from(SESSION_ID)
        )
        .await
        .expect("pending inputs after cancellation")
        .is_empty(),
        "queued input `{}` should be completed by the cancelled turn",
        queued_input.input_id
    );
    assert!(
        lash_core::store::IngressStore::list_queued_work(
            store.as_ref(),
            &SessionId::from(SESSION_ID)
        )
        .await
        .expect("queued work after cancellation")
        .is_empty(),
        "claimed wake `{}` should be completed by the cancelled turn",
        wake.wake_id
    );
    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from(SESSION_ID),
            "after-cancel-claimed-wake-drain",
        ))
        .await
        .expect("open the drain's handler");
    assert!(
        runtime
            .drive_one_admitted_queued_root(TurnOptions::new(
                CancellationToken::new(),
                handler.scoped(),
            ))
            .await
            .expect("post-cancel drain should succeed")
            .ran()
            .is_none(),
        "neither the cancelled input nor the claimed wake should replay"
    );
    handler.close().await.expect("close the drain's handler");
    let requests = requests.lock_recover().clone();
    assert_eq!(requests.len(), 2);
    assert!(request_contains_text(
        &requests[0],
        "cancel with wake pending"
    ));
    assert!(!request_contains_text(
        &requests[0],
        "wake cancelled in checkpoint"
    ));
    assert!(request_contains_text(
        &requests[1],
        "cancel with wake pending"
    ));
    assert!(request_contains_text(
        &requests[1],
        "wake cancelled in checkpoint"
    ));
}

// Regression (ADR 0029): a long-running turn must keep the queued work it
// already admitted across a stall, no matter how short the lease TTL is.
// Queued-work batches are admitted at active-turn checkpoints to the turn's
// root; the binding carries no TTL of its own. So a turn that admits a batch
// at one checkpoint, stalls past the (tiny) lease TTL -- here a slow provider
// call, while the session lease keeps renewing on its background cadence and
// preserves its generation -- then crosses another checkpoint re-runs
// `admit_at_checkpoint` under the *same* live fence, which can never
// self-steal its own rows. At finalization the root still holds its rows and
// the commit succeeds. Before generation fencing this failed with
// `QueuedWorkClaimExpired` because the claim expired under the stalled owner.
//
// This test must FAIL if anyone reintroduces time- or renewal-based binding
// invalidation. The turn is driven with an in-process `TurnInput` (not an
// admitted pending input) so the queued-work binding is the one under
// scrutiny; the equally-unrenewed turn-input binding is covered by the
// conformance admission laws.
