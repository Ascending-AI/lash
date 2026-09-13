use super::*;

#[tokio::test]
pub(super) async fn queued_config_patches_coalesce_into_one_head_commit() {
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store(mock_provider(Vec::new())).await;
    let models = ["queued-model-a", "queued-model-b", "queued-model-c"];
    for model in models {
        enqueue_config_patch_command(
            store.as_ref(),
            &SessionId::from("root"),
            crate::runtime::ApplyConfigPatch {
                model: Some(
                    crate::ModelSpec::builder(model)
                        .context_window_tokens(32_000)
                        .build()
                        .expect("model"),
                ),
                ..crate::runtime::ApplyConfigPatch::default()
            },
        )
        .await;
    }
    let commits_before = *store.runtime_commit_count.lock_recover();
    let owner = lease_owner("config-patch-coalescing");
    let lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
        &owner,
        "config-patch-coalescing-executor",
        crate::LeaseTimings::default().ttl_ms(),
    )
    .await
    .expect("claim session execution lease")
    .acquired()
    .expect("session execution lease");

    runtime
        .drain_next_session_command(&lease.fence())
        .await
        .expect("drain coalesced config patches")
        .expect("one receipt from the coalesced claim");

    assert_eq!(
        *store.runtime_commit_count.lock_recover(),
        commits_before + 1,
        "N config commands must share exactly one head commit"
    );
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), &SessionId::from("root"))
            .await
            .expect("list settled config commands")
            .is_empty(),
        "every independently accepted command must settle its own batch"
    );
    assert_eq!(runtime.session_policy().model.id, "queued-model-c");
}

#[tokio::test]
pub(super) async fn config_settlement_distinguishes_enqueue_rejection_from_durable_completion() {
    let mut runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    let original_model = runtime.session_policy().model.clone();
    let outcome = runtime
        .submit_apply_config_patch_with_idempotency_key(
            crate::runtime::ApplyConfigPatch {
                model: Some(
                    crate::ModelSpec::builder("must-not-publish")
                        .context_window_tokens(32_000)
                        .build()
                        .expect("model"),
                ),
                ..crate::runtime::ApplyConfigPatch::default()
            },
            "",
        )
        .await
        .expect("typed submission outcome");

    let crate::runtime::SessionCommandSettlement::Rejected(rejection) = outcome else {
        panic!("empty idempotency key must be rejected before durable acceptance");
    };
    assert_eq!(
        rejection.code,
        crate::RuntimeErrorCode::SessionCommandIdempotencyKey
    );
    assert_eq!(runtime.session_policy().model, original_model);

    let durable = runtime
        .submit_apply_config_patch_with_idempotency_key(
            crate::runtime::ApplyConfigPatch {
                model: Some(
                    crate::ModelSpec::builder("durable-inline")
                        .context_window_tokens(32_000)
                        .build()
                        .expect("model"),
                ),
                ..crate::runtime::ApplyConfigPatch::default()
            },
            "durable-inline",
        )
        .await
        .expect("durable settlement");
    assert!(matches!(
        durable,
        crate::runtime::SessionCommandSettlement::Durable(_)
    ));
    assert_eq!(runtime.session_policy().model.id, "durable-inline");
}

pub(super) fn turn_budget_config_mutator(
    turn_budget: crate::TurnBudget,
) -> Arc<dyn crate::PluginFactory> {
    Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(move |_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                tool_result_projector: None,
                runtime_event: None,
                external_registrar: Some(Arc::new(move |reg| {
                    reg.session()
                        .config_mutator(Arc::new(move |_ctx, mut policy| {
                            Box::pin(async move {
                                policy.turn_budget = turn_budget;
                                Ok(policy)
                            })
                        }));
                    Ok(())
                })),
            }))
        }),
    })
}

#[tokio::test]
pub(super) async fn plugin_turn_budget_mutation_survives_park_and_reload() {
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::RuntimePersistence> = store.clone();
    let persisted_budget = crate::TurnBudget::bounded(7);
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![turn_budget_config_mutator(persisted_budget)],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(),
        Arc::clone(&runtime_store),
    )
    .await;

    runtime
        .update_session_config(crate::SessionConfigPatch {
            model: Some(
                crate::ModelSpec::builder("turn-budget-mutation-trigger")
                    .context_window_tokens(32_000)
                    .build()
                    .expect("model"),
            ),
            ..crate::SessionConfigPatch::default()
        })
        .await
        .expect("plugin turn-budget mutation settles");
    assert_eq!(runtime.session_policy().turn_budget, persisted_budget);
    drop(
        Box::pin(runtime.park())
            .await
            .expect("park mutated session"),
    );

    let reloaded_state = crate::load_persisted_session_state(runtime_store.as_ref())
        .await
        .expect("load parked session")
        .expect("parked session exists");
    let plugin_host = crate::PluginHost::new(vec![turn_budget_config_mutator(persisted_budget)]);
    let plugins = match reloaded_state.plugin_state() {
        Some(snapshot) => plugin_host.rematerialize_session(
            "root",
            snapshot,
            crate::plugin::RecordedSessionConfig::new(reloaded_state.protocol_turn_options.clone()),
        ),
        None => plugin_host.build_session("root"),
    }
    .expect("reloaded plugins");
    let reloaded = crate::LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::PersistentRuntimeServices::new(plugins, runtime_store),
        reloaded_state,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("reload parked runtime");
    assert_eq!(
        reloaded.session_policy().turn_budget,
        persisted_budget,
        "plugin-mutated durable budget must survive cold reload"
    );
}

#[tokio::test]
pub(super) async fn every_session_config_patch_emits_a_lifecycle_event() {
    let observed = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let observed_hook = Arc::clone(&observed);
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(move |_| {
            let observed = Arc::clone(&observed_hook);
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                tool_result_projector: None,
                runtime_event: Some(Arc::new(move |event| {
                    let observed = Arc::clone(&observed);
                    Box::pin(async move {
                        if let crate::plugin::PluginLifecycleEvent::SessionConfigChanged(ctx) =
                            event
                        {
                            observed.lock().await.push((ctx.previous, ctx.current));
                        }
                        Ok(())
                    })
                })),
                external_registrar: None,
            }))
        }),
    });
    let transport = mock_provider(Vec::new());
    let mut runtime = runtime_with_plugins(vec![plugin], transport).await;

    let alt_provider = TestProvider::builder()
        .kind("alt")
        .complete_error("alt provider not wired")
        .build()
        .into_handle();
    let alt_model = crate::ModelSpec::builder("alt-model")
        .context_window_tokens(123_456)
        .build()
        .expect("valid model spec");
    runtime
        .update_session_config(crate::SessionConfigPatch {
            model: Some(alt_model.clone()),
            ..Default::default()
        })
        .await
        .expect("update model config");
    runtime
        .update_session_config(crate::SessionConfigPatch {
            provider: Some(alt_provider),
            ..Default::default()
        })
        .await
        .expect("update provider config");

    assert_eq!(observed.lock().await.len(), 2);

    let combined_provider = TestProvider::builder()
        .kind("combined")
        .complete_error("combined provider not wired")
        .build()
        .into_handle();
    let combined_model = crate::ModelSpec::builder("combined-model")
        .context_window_tokens(234_567)
        .build()
        .expect("valid combined model spec");
    runtime
        .update_session_config(crate::SessionConfigPatch {
            provider: Some(combined_provider),
            model: Some(combined_model.clone()),
            ..Default::default()
        })
        .await
        .expect("update combined config");

    assert_eq!(observed.lock().await.len(), 3);

    let prompt = crate::PromptLayer::new().with_contribution(crate::PromptContribution::guidance(
        "Patch",
        "prompt-only session config",
    ));
    runtime
        .update_session_config(crate::SessionConfigPatch::with_prompt(prompt.clone()))
        .await
        .expect("update prompt config");

    assert_eq!(observed.lock().await.len(), 4);

    let generation = crate::GenerationOptions {
        seed: Some(42),
        ..Default::default()
    };
    runtime
        .update_session_config(crate::SessionConfigPatch {
            generation: Some(crate::GenerationOverlay::Replace(generation.clone())),
            ..Default::default()
        })
        .await
        .expect("update generation config");

    assert_eq!(observed.lock().await.len(), 5);

    let helper_template =
        crate::PromptTemplate::new(vec![crate::PromptTemplateSection::untitled(vec![
            crate::PromptTemplateEntry::text("prompt helper template"),
        ])]);
    runtime
        .set_prompt_template(helper_template.clone())
        .await
        .expect("set prompt template");

    let changes = observed.lock().await;
    assert_eq!(changes.len(), 6);
    let (previous, current) = &changes[0];
    assert_eq!(previous.provider_id, "mock");
    assert_eq!(current.provider_id, "mock");
    assert_eq!(current.model.id, "alt-model");
    assert_ne!(
        previous.context_window_tokens(),
        current.context_window_tokens()
    );
    let (previous, current) = &changes[1];
    assert_eq!(previous.provider_id, "mock");
    assert_eq!(previous.model.id, "alt-model");
    assert_eq!(current.provider_id, "alt");
    assert_eq!(current.model.id, "alt-model");
    let (previous, current) = &changes[2];
    assert_eq!(previous.provider_id, "alt");
    assert_eq!(previous.model.id, "alt-model");
    assert_eq!(current.provider_id, "combined");
    assert_eq!(current.model, combined_model);
    let (previous, current) = &changes[3];
    assert_eq!(previous.model.id, "combined-model");
    assert_eq!(current.prompt, prompt);
    let (previous, current) = &changes[4];
    assert_eq!(previous.prompt, prompt);
    assert_eq!(current.generation, generation);
    let (previous, current) = &changes[5];
    assert_eq!(previous.generation, generation);
    assert_eq!(
        current.prompt.template,
        Some(helper_template),
        "prompt helper changes emit SessionConfigChanged"
    );
}

#[tokio::test]
pub(super) async fn turn_provider_override_does_not_persist_into_session_policy_or_agent_frame() {
    let mut runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    let alt_provider = TestProvider::builder()
        .kind("alt")
        .complete(|_| async {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "alt response".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle();
    let mut turn_context = crate::TurnContext::default();
    turn_context.set_provider(alt_provider);

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "use override".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context,
            },
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("provider-override-turn"),
            ),
        )
        .await
        .expect("turn");

    assert_eq!(turn.assistant_output.safe_text, "alt response");
    assert_eq!(turn.state.policy.recorded_provider_id(), "mock");
    assert_eq!(
        runtime.state.effective_policy().recorded_provider_id(),
        "mock"
    );
    assert!(
        runtime.state.agent_frames.iter().all(|frame| frame
            .assignment
            .policy
            .recorded_provider_id()
            == "mock")
    );
}

#[tokio::test]
pub(super) async fn plugin_before_turn_can_abort_and_inject_messages() {
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: Some(Arc::new(|_| {
                    Box::pin(async {
                        Ok(vec![
                            crate::TurnPluginDirective::EnqueueMessages(
                                crate::EnqueueMessagesDirective {
                                    messages: vec![crate::PluginMessage::text(
                                        crate::MessageRole::System,
                                        "plugin preface",
                                    )],
                                },
                            ),
                            crate::TurnPluginDirective::AbortTurn(crate::AbortTurnDirective {
                                code: "blocked".to_string(),
                                message: "plugin stopped the turn".to_string(),
                            }),
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
    let transport = mock_provider(Vec::new());
    let mut runtime = runtime_with_plugins(vec![plugin], transport).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("plugin-extension-turn"),
            ),
        )
        .await
        .expect("turn");

    assert!(matches!(&turn.outcome, TurnOutcome::Stopped(_)));
    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Stopped(TurnStop::PluginAbort)
    ));
    assert!(turn.errors.iter().any(|issue| issue.kind == "plugin"));
    assert!(
        active_conversation_messages(&turn.state)
            .iter()
            .any(|message| {
                message
                    .parts
                    .iter()
                    .any(|part| part.content.contains("plugin preface"))
            })
    );
}

#[tokio::test]
pub(super) async fn normal_turn_stores_effective_user_text_in_state() {
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
    let mut runtime = runtime_with_plugins(Vec::new(), transport).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "/yolopush\n\n<skill>\nbody\n</skill>".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("skill-command-visibility-turn"),
            ),
        )
        .await
        .expect("turn");

    let read_model = turn.state.read_model();
    let read_model = read_model.expect("accepted turn frame scope resolves");
    let user_message = read_model
        .messages
        .iter()
        .find(|message| message.role == MessageRole::User)
        .expect("user message");
    assert_eq!(
        user_message.parts.first().map(|part| part.content.as_str()),
        Some("/yolopush\n\n<skill>\nbody\n</skill>")
    );
    // The committed turn input carries typed provenance so a host that rendered
    // its own row for this turn recognizes this copy without parsing the
    // runtime-minted message id (FIG-972). The direct path has no durable turn
    // input behind it, so `input_id` is absent.
    assert_eq!(
        user_message.origin,
        Some(crate::MessageOrigin::TurnInput {
            turn_id: TurnId::from("skill-command-visibility-turn"),
            input_id: None,
        })
    );
}

#[tokio::test]
pub(super) async fn retryable_llm_failures_exhaust_and_fail_turn() {
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Err(
                crate::llm::transport::LlmTransportError::new("provider unavailable")
                    .with_retry_verdict(
                        crate::llm::transport::TransportRetryVerdict::RetryableTransient,
                    )
                    .with_code("http_500"),
            ),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Err(
                crate::llm::transport::LlmTransportError::new("provider unavailable")
                    .with_retry_verdict(
                        crate::llm::transport::TransportRetryVerdict::RetryableTransient,
                    )
                    .with_code("http_500"),
            ),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Err(
                crate::llm::transport::LlmTransportError::new("provider unavailable")
                    .with_retry_verdict(
                        crate::llm::transport::TransportRetryVerdict::RetryableTransient,
                    )
                    .with_code("http_500"),
            ),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Err(
                crate::llm::transport::LlmTransportError::new("provider unavailable")
                    .with_retry_verdict(
                        crate::llm::transport::TransportRetryVerdict::RetryableTransient,
                    )
                    .with_code("http_500"),
            ),
        },
    ]);
    let mut runtime = runtime_with_plugins(Vec::new(), transport).await;
    runtime.host.core.clock = Arc::new(CancelWatchTestClock(crate::testing::TestClock::new(
        1_700_000_000_123,
    )));

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("retryable-error-turn"),
            ),
        )
        .await
        .expect("turn");

    assert!(matches!(&turn.outcome, TurnOutcome::Stopped(_)));
    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Stopped(TurnStop::ProviderError)
    ));
    assert!(turn.errors.iter().any(|issue| issue.kind == "llm_provider"));
    assert!(
        turn.errors
            .iter()
            .any(|issue| issue.message.contains("provider unavailable"))
    );
    // The transport's typed retryable signal survives into the host-facing
    // issue instead of living only in trace records.
    assert!(
        turn.errors
            .iter()
            .any(|issue| issue.kind == "llm_provider" && issue.retryable == Some(true))
    );
    assert_eq!(turn.llm_calls.len(), 1);
    assert_eq!(turn.llm_calls[0].attempts.len(), 4);
}

#[tokio::test]
pub(super) async fn provider_failure_surfaces_typed_kind_and_retryability_on_turn_issue() {
    // A 400 classifies as a non-retryable Validation failure, so the turn
    // fails on the first attempt with fully typed failure signals.
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Err(crate::llm::transport::LlmTransportError::new("bad request").with_code("400")),
    }]);
    let mut runtime = runtime_with_plugins(Vec::new(), transport).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("typed-provider-failure-turn"),
            ),
        )
        .await
        .expect("turn");

    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Stopped(TurnStop::ProviderError)
    ));
    let issue = turn
        .errors
        .iter()
        .find(|issue| issue.kind == "llm_provider")
        .expect("llm_provider issue");
    assert_eq!(issue.retryable, Some(false));
    assert_eq!(
        issue.provider_failure_kind,
        Some(crate::ProviderFailureKind::Validation)
    );
    assert_eq!(issue.code.as_deref(), Some("400"));
    assert_eq!(turn.llm_calls.len(), 1);
    assert_eq!(turn.llm_calls[0].attempts.len(), 1);
}

#[tokio::test]
pub(super) async fn assembled_turn_reports_turn_timing_from_injected_clock() {
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
    let mut runtime = runtime_with_plugins(Vec::new(), transport).await;
    runtime.host.core.clock = Arc::new(ManualClock::new(4_242));

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope(&SessionId::from("root"), &TurnId::from("turn-timing-turn")),
        )
        .await
        .expect("turn");

    // `started_at_ms` is read from the injected wall clock, so a
    // deterministic clock yields a deterministic timestamp (the OS clock
    // would report the current epoch here).
    assert_eq!(turn.execution.started_at_ms, 4_242);
}

#[tokio::test]
pub(super) async fn queued_checkpoint_input_commits_before_continuing_standard_turn() {
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
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    enqueue_turn_input_for_checkpoint(
        store.as_ref(),
        &SessionId::from("root"),
        &TurnId::from("queued-checkpoint-turn"),
        None,
        TurnInput::text("one more thing"),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("queued-checkpoint-turn"),
            ),
        )
        .await
        .expect("turn");

    assert!(
        active_conversation_messages(&turn.state)
            .iter()
            .any(|message| {
                message.role == MessageRole::Assistant
                    && message
                        .parts
                        .iter()
                        .any(|part| part.content.contains("Second answer."))
            })
    );
    let admitted = active_conversation_messages(&turn.state)
        .into_iter()
        .filter(|message| {
            message.role == MessageRole::User
                && message
                    .parts
                    .iter()
                    .any(|part| part.content == "one more thing")
        })
        .collect::<Vec<_>>();
    assert_eq!(admitted.len(), 1);
    // A normal user message that records which turn absorbed it, not a plugin or
    // process injection (FIG-972).
    assert!(matches!(
        admitted[0].origin.as_ref(),
        Some(crate::MessageOrigin::TurnInput { turn_id, input_id })
            if turn_id == "queued-checkpoint-turn" && input_id.is_some()
    ));
}

#[tokio::test]
pub(super) async fn queued_checkpoint_input_preserves_images() {
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
    let store = Arc::new(RecordingStore::default());
    let mut runtime = TestRuntime::new(transport)
        .plugins(Vec::new())
        .host(test_host_config())
        .store(store.clone())
        .attachment_acceptance(
            crate::attachments::attachment_test_capability().attachment_acceptance,
        )
        .build()
        .await;
    enqueue_turn_input_for_checkpoint(
        store.as_ref(),
        &SessionId::from("root"),
        &TurnId::from("image-attachment-turn"),
        None,
        TurnInput::text("see image").with_attachment(crate::AttachmentSource::inline(
            crate::MediaType::parse("image/png").unwrap(),
            vec![1, 2, 3],
        )),
    )
    .await;

    runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("image-attachment-turn"),
            ),
        )
        .await
        .expect("turn");

    let requests = requests.lock_recover().clone();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().any(|message| {
        message.role == crate::llm::types::LlmRole::User
            && message
                .blocks
                .iter()
                .any(|block| matches!(block, crate::llm::types::LlmContentBlock::Attachment { .. }))
    }));
}

// Boundary: active-turn checkpoint input tests stay in `turns.rs` when they
// assert model prompt replay, plugin checkpoint hooks, injected-input stream
// events, image materialization, or persisted conversation projection. Runtime
// Scenarios own the host-level active-input redrive/cancel/queue invariants.
#[tokio::test]
pub(super) async fn checkpoint_hook_can_inject_messages() {
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: Some(Arc::new(|ctx| {
                    Box::pin(async move {
                        if ctx.checkpoint == crate::CheckpointKind::BeforeCompletion {
                            Ok(vec![crate::TurnPluginDirective::EnqueueMessages(
                                crate::EnqueueMessagesDirective {
                                    messages: vec![crate::PluginMessage::text(
                                        crate::MessageRole::System,
                                        "checkpoint injected",
                                    )],
                                },
                            )])
                        } else {
                            Ok(Vec::new())
                        }
                    })
                })),
                tool_result_projector: None,
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
    let mut runtime = runtime_with_plugins(vec![plugin], transport).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("plugin-action-turn"),
            ),
        )
        .await
        .expect("turn");

    assert!(
        active_conversation_messages(&turn.state)
            .iter()
            .any(|message| {
                message.role == MessageRole::System
                    && message
                        .parts
                        .iter()
                        .any(|part| part.content == "checkpoint injected")
            })
    );
}

#[tokio::test]
pub(super) async fn checkpoint_plugin_abort_leaves_active_input_pending_without_application_evidence()
 {
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: Some(Arc::new(|_| {
                    Box::pin(async {
                        Ok(vec![crate::TurnPluginDirective::AbortTurn(
                            crate::AbortTurnDirective {
                                code: "checkpoint_rejected".to_string(),
                                message: "reject checkpoint delivery".to_string(),
                            },
                        )])
                    })
                })),
                tool_result_projector: None,
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
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![plugin],
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
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

    let turn = runtime
        .stream_turn(
            TurnInput::text("hello"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("checkpoint-plugin-abort-turn"),
                ),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("plugin-aborted turn assembles");

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
                crate::TurnEvent::QueuedInputAccepted { applications } => applications
                    .iter()
                    .all(|application| application.input_id != admitted.input_id),
                _ => true,
            }),
        "a rejected checkpoint must not emit live application evidence"
    );
    assert!(
        crate::store::TurnInputStore::list_turn_input_applications(
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
        crate::store::TurnInputStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("list pending input after rejected checkpoint")
        .iter()
        .any(|input| input.input_id == admitted.input_id),
        "a rejected checkpoint input must remain claimable"
    );
    assert!(
        active_conversation_messages(&turn.state)
            .iter()
            .all(|message| message
                .parts
                .iter()
                .all(|part| part.content != "must remain pending")),
        "a rejected checkpoint input must not enter canonical history"
    );
}

#[tokio::test]
pub(super) async fn checkpoint_attachment_failure_leaves_active_input_pending_without_application_evidence()
 {
    #[derive(Debug)]
    struct DenyHostCheckpointAttachments;

    impl crate::AttachmentSourcePolicy for DenyHostCheckpointAttachments {
        fn authorize(
            &self,
            producer: &crate::AttachmentProducer,
            _source: &crate::AttachmentSource,
        ) -> Result<(), crate::test_support::AttachmentSourcePolicyError> {
            if matches!(producer, crate::AttachmentProducer::Host) {
                return Err(crate::test_support::AttachmentSourcePolicyError {
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
                        let mut message =
                            crate::PluginMessage::text(crate::MessageRole::System, "plugin upload");
                        message
                            .attachments
                            .push(crate::AttachmentSource::external_url(
                                crate::MediaType::parse("application/pdf")
                                    .expect("valid test media type"),
                                "https://example.test/checkpoint.pdf",
                            ));
                        Ok(vec![crate::TurnPluginDirective::EnqueueMessages(
                            crate::EnqueueMessagesDirective {
                                messages: vec![message],
                            },
                        )])
                    })
                })),
                tool_result_projector: None,
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
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![plugin],
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
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

    let turn = runtime
        .stream_turn(
            TurnInput::text("hello"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("checkpoint-attachment-failure-turn"),
                ),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("attachment-failed turn assembles");

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
                crate::TurnEvent::QueuedInputAccepted { applications } => applications
                    .iter()
                    .all(|application| application.input_id != admitted.input_id),
                _ => true,
            }),
        "a failed checkpoint attachment must not emit live application evidence"
    );
    assert!(
        crate::store::TurnInputStore::list_turn_input_applications(
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
        crate::store::TurnInputStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("list pending input after attachment failure")
        .iter()
        .any(|input| input.input_id == admitted.input_id),
        "an attachment-failed checkpoint input must remain claimable"
    );
    assert!(
        active_conversation_messages(&turn.state)
            .iter()
            .all(|message| message
                .parts
                .iter()
                .all(|part| part.content != "must remain pending after attachment failure")),
        "an attachment-failed checkpoint input must not enter canonical history"
    );
}

#[tokio::test]
pub(super) async fn queued_checkpoint_input_accepts_and_persists_one_normal_user_message() {
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
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    enqueue_turn_input_for_checkpoint(
        store.as_ref(),
        &SessionId::from("root"),
        &TurnId::from("injection-accepted-turn"),
        Some("host:follow-up-id".to_string()),
        TurnInput::text("follow up"),
    )
    .await;
    let sink = RecordingSink::default();
    let assembled = runtime
        .stream_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("injection-accepted-turn"),
                ),
            )
            .with_events(&sink),
        )
        .await
        .expect("turn");

    let mut saw_injected_accept = false;
    for event in sink.snapshot() {
        if let crate::SessionStreamEvent::InjectedTurnInputAccepted { inputs, .. } = event {
            saw_injected_accept = inputs.iter().any(|input| {
                input.id.as_deref() == Some("follow-up-id")
                    && input.message.role == crate::MessageRole::User
                    && input.message.content == "follow up"
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
            message.role == crate::MessageRole::User
                && message.parts.iter().any(|part| part.content == "follow up")
        })
        .count();
    assert_eq!(
        follow_up_count, 1,
        "injected active-turn input must persist exactly once in history"
    );
    let follow_up = projected
        .iter()
        .find(|message| {
            message.role == crate::MessageRole::User
                && message.parts.iter().any(|part| part.content == "follow up")
        })
        .expect("committed injected input");
    // The injected input keeps the normal user-message representation — no
    // plugin or process origin — and records which turn absorbed it and which
    // durable input it came from (FIG-972).
    let crate::MessageOrigin::TurnInput { turn_id, input_id } = follow_up
        .origin
        .as_ref()
        .expect("committed injected input carries turn-input provenance")
    else {
        panic!("injected input must use the normal user-message representation");
    };
    assert_eq!(turn_id, "injection-accepted-turn");
    let input_id = input_id
        .as_deref()
        .expect("queued ingress records the durable input id");
    assert_eq!(follow_up.id, crate::runtime::ingress_message_id(input_id));
    let opening = projected
        .iter()
        .find(|message| {
            message.role == crate::MessageRole::User
                && message.parts.iter().any(|part| part.content == "hello")
        })
        .expect("committed opening input");
    // The opening input entered the same way (ADR 0069): its own acceptance,
    // its own durable id, distinct from the one injected at the checkpoint.
    let crate::MessageOrigin::TurnInput {
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
        crate::runtime::ingress_message_id(opening_input_id)
    );
}

pub(super) async fn commit_checkpoint_injected_turn_for_redrive(
    store: Arc<RecordingStore>,
    controller: Arc<dyn crate::RuntimeEffectController>,
    turn_id: &TurnId,
) -> (crate::TurnInput, crate::TurnInputAcceptanceReceipt) {
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
    let runtime_store: Arc<dyn crate::RuntimePersistence> = store.clone();
    let mut runtime = Box::pin(runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        journal_replay_host(Arc::clone(&controller)),
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
    let scope = crate::ScopedEffectController::shared(
        Arc::clone(&controller),
        crate::ExecutionScope::turn("root", turn_id),
    )
    .expect("scope the first checkpoint-injected turn");
    let committed = runtime
        .stream_turn(
            input.clone(),
            TurnOptions::new(CancellationToken::new(), scope),
        )
        .await
        .expect("commit the checkpoint-injected turn");
    let acceptance = committed
        .turn_input_acceptance
        .expect("the committed direct turn exposes its acceptance");
    (input, acceptance)
}

pub(super) async fn redrive_checkpoint_injected_turn(
    store: Arc<dyn crate::RuntimePersistence>,
    controller: Arc<dyn crate::RuntimeEffectController>,
    turn_id: &TurnId,
    input: TurnInput,
) -> Result<crate::AssembledTurn, crate::RuntimeError> {
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        journal_replay_host(Arc::clone(&controller)),
        store,
    )
    .await;
    let scope = crate::ScopedEffectController::shared(
        controller,
        crate::ExecutionScope::turn("root", turn_id),
    )
    .expect("scope the checkpoint-injected redrive");
    runtime
        .stream_turn(input, TurnOptions::new(CancellationToken::new(), scope))
        .await
}

#[tokio::test]
pub(super) async fn checkpoint_injected_turn_redrive_replays_the_original_commit_identity() {
    let turn_id = &TurnId::from("checkpoint-injected-redrive");
    let store = Arc::new(RecordingStore::default());
    let controller: Arc<dyn crate::RuntimeEffectController> =
        Arc::new(JournalReplayEffectController::default());
    let (input, first_acceptance) = commit_checkpoint_injected_turn_for_redrive(
        Arc::clone(&store),
        Arc::clone(&controller),
        turn_id,
    )
    .await;
    let first_applications = crate::store::TurnInputStore::list_turn_input_applications(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("read first turn applications");
    assert_eq!(first_applications.len(), 3);
    assert_eq!(
        first_applications
            .iter()
            .filter(|application| application.checkpoint.is_some())
            .count(),
        1,
        "the first execution must absorb exactly one checkpoint injection"
    );

    let replay_store: Arc<dyn crate::RuntimePersistence> = Arc::new(JournalRedriveStore {
        inner: Arc::clone(&store),
        application_history_available: true,
        foreign_checkpoint_application: None,
    });
    let replayed = Box::pin(redrive_checkpoint_injected_turn(
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
        crate::store::TurnInputStore::list_turn_input_applications(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("read applications after redrive"),
        first_applications,
        "redrive must preserve the original initial/checkpoint application split"
    );
}

#[tokio::test]
pub(super) async fn checkpoint_injected_turn_redrive_refuses_when_application_history_is_unavailable()
 {
    let turn_id = &TurnId::from("checkpoint-injected-redrive-refusal");
    let store = Arc::new(RecordingStore::default());
    let controller: Arc<dyn crate::RuntimeEffectController> =
        Arc::new(JournalReplayEffectController::default());
    let (input, acceptance) = commit_checkpoint_injected_turn_for_redrive(
        Arc::clone(&store),
        Arc::clone(&controller),
        turn_id,
    )
    .await;
    let unavailable: Arc<dyn crate::RuntimePersistence> = Arc::new(JournalRedriveStore {
        inner: Arc::clone(&store),
        application_history_available: false,
        foreign_checkpoint_application: None,
    });

    let error = Box::pin(redrive_checkpoint_injected_turn(
        unavailable,
        controller,
        turn_id,
        input,
    ))
    .await
    .expect_err("redrive must stop before commit when its application set cannot be rebuilt");

    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::TurnInputRedriveSetUnavailable,
        "the refusal must be typed instead of surfacing a later commit-identity mismatch"
    );
    assert!(
        error.message.contains(&acceptance.input_id),
        "the refusal must name the journaled acceptance that needs recovery: {error:?}"
    );
    assert!(
        error
            .message
            .contains("restore turn-input application history, then redrive the same turn"),
        "the refusal must name the operator recovery step: {error:?}"
    );
}

#[tokio::test]
pub(super) async fn journaled_acceptance_applied_by_a_foreign_turn_refuses_before_commit() {
    let turn_id = &TurnId::from("checkpoint-injected-redrive-foreign-application");
    let store = Arc::new(RecordingStore::default());
    let controller: Arc<dyn crate::RuntimeEffectController> =
        Arc::new(JournalReplayEffectController::default());
    let (input, acceptance) = commit_checkpoint_injected_turn_for_redrive(
        Arc::clone(&store),
        Arc::clone(&controller),
        turn_id,
    )
    .await;
    let foreign: Arc<dyn crate::RuntimePersistence> = Arc::new(JournalRedriveStore {
        inner: Arc::clone(&store),
        application_history_available: true,
        foreign_checkpoint_application: Some((
            acceptance.input_id,
            crate::TurnId::from("foreign-turn"),
        )),
    });

    let error = Box::pin(redrive_checkpoint_injected_turn(
        foreign, controller, turn_id, input,
    ))
    .await
    .expect_err("a foreign checkpoint application must stop before commit");

    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::TurnInputRedriveSetUnavailable,
        "a journaled acceptance applied at another turn's checkpoint must be refused before commit, not surface StoreCommitFailed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn active_input_after_last_call_is_first_admitted_on_next_turn() {
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
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    runtime.set_turn_phase_probe(Arc::new(PauseAtPreparedTurn {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    }));

    let first_turn = crate::task::spawn(async move {
        runtime
            .run_turn_assembled(
                TurnInput::text("first turn input"),
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("after-last-call-turn"),
                ),
            )
            .await
            .expect("first turn");
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

    let pending = crate::store::TurnInputStore::list_pending_turn_inputs(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("deferred late input");
    assert_eq!(pending.len(), 1);
    assert!(matches!(
        pending[0].ingress,
        crate::TurnInputIngress::NextTurn
    ));
    assert_eq!(pending[0].state, crate::TurnInputState::DeferredNextTurn);

    runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("late-active-next-turn"),
            ),
        ))
        .await
        .expect("drain deferred input")
        .ran()
        .expect("deferred input starts a turn");

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
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store(mock_provider(Vec::new())).await;
    let command =
        enqueue_session_command(store.as_ref(), &SessionId::from("root"), "test refresh").await;

    let drained = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("command-only-queue-drain"),
            ),
        ))
        .await
        .expect("command-only drain succeeds")
        .ran();

    assert!(drained.is_none());
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), &SessionId::from("root"))
            .await
            .expect("list queue after command-only drain")
            .is_empty(),
        "command batch `{}` should be completed",
        command.batch_id
    );
}

#[tokio::test]
pub(super) async fn no_queued_work_submit_defers_without_refreshing_resident_state() {
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store(mock_provider(Vec::new())).await;
    let full_loads_before = store.load_session_count();
    let head_reads_before = store.load_session_head_meta_count();

    let receipt = runtime
        .submit_session_command(
            crate::SessionCommand::RefreshToolCatalog {
                reason: "deferred queued lane".to_string(),
            },
            "deferred-queued-command",
        )
        .await
        .expect("NoQueuedWork leaves the durable command pending");

    assert_eq!(store.load_session_count(), full_loads_before);
    assert_eq!(store.load_session_head_meta_count(), head_reads_before);
    let pending =
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), &SessionId::from("root"))
            .await
            .expect("inspect deferred durable command");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].batch_id, receipt.batch_id);
}

// Boundary: these process-wake and active-checkpoint steering tests stay in
// `turns.rs` because they verify the full `LashRuntime` scheduler, provider
// prompt contents, cancellation path, and selected queued-work APIs. Runtime
// Scenarios cover the overlapping store-level queue/input/lease invariants,
// including active-checkpoint process-wake claim eligibility and the selected
// queued-work invariant that pending next-turn input is not consumed. The
// selected-drain case remains here because the owned behavior is the public
// `stream_selected_queued_work` API running a turn while preserving unrelated
// pending input.
#[tokio::test]
pub(super) async fn next_turn_input_turn_claims_process_wake_at_active_checkpoint() {
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
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
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
    let target_scope = crate::SessionScope::new("root");
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "wake-after-user-input",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::session(target_scope.clone()),
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
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
        &ProcessId::from("wake-after-user-input"),
        crate::ProcessEventAppendRequest::new(
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

    let drained = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("next-input-before-wake-drain"),
            ),
        ))
        .await
        .expect("queued drain succeeds")
        .ran()
        .expect("pending turn input drains first");

    assert_eq!(
        drained.assistant_output.safe_text,
        "wake checkpoint response"
    );
    assert!(
        crate::store::TurnInputStore::list_pending_turn_inputs(
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
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), &SessionId::from("root"))
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
pub(super) async fn selected_process_wake_drain_does_not_claim_pending_next_turn_input() {
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "selected wake response".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    let queued_input = enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "still pending user",
    )
    .await;
    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    let target_scope = crate::SessionScope::new("root");
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "selected-wake",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::session(target_scope.clone()),
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
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
        &ProcessId::from("selected-wake"),
        crate::ProcessEventAppendRequest::new(
            "process.wake",
            json!({
                "text": "selected wake",
                "value": {
                    "status": "selected wake"
                }
            }),
        ),
    )
    .await;
    let wake_batch =
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), &SessionId::from("root"))
            .await
            .expect("queued work before selected drain")
            .into_iter()
            .find(|batch| {
                batch.items.iter().any(|item| {
                    matches!(
                        &item.payload,
                        crate::QueuedWorkPayload::ProcessWake { wake: queued_wake }
                            if queued_wake.wake_id == wake.wake_id
                    )
                })
            })
            .expect("wake batch");

    let drained = runtime
        .stream_selected_queued_work(
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("selected-wake-drain"),
                ),
            ),
            std::slice::from_ref(&wake_batch.batch_id),
        )
        .await
        .expect("selected wake drain succeeds")
        .expect("selected wake produces a turn");

    assert_eq!(drained.assistant_output.safe_text, "selected wake response");
    let pending_inputs = crate::store::TurnInputStore::list_pending_turn_inputs(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("pending inputs after selected wake drain");
    assert_eq!(
        pending_inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![queued_input.input_id.as_str()],
        "selected queued-work drains must not also claim pending user input"
    );
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), &SessionId::from("root"))
            .await
            .expect("queued work after selected wake drain")
            .is_empty(),
        "selected wake batch should be completed"
    );
}

#[tokio::test]
pub(super) async fn process_wake_claimed_at_checkpoint_is_completed_when_turn_is_cancelled() {
    // Commit admission is process-wide and keyed by session id. Keep this
    // cancellation rendezvous out of the shared `root` lane so unrelated
    // libtest cases cannot make its final commit contend with their turn.
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
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store_for_session(
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
    let target_scope = crate::SessionScope::new(SESSION_ID);
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "cancel-claimed-wake",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::session(target_scope.clone()),
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
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
        &ProcessId::from("cancel-claimed-wake"),
        crate::ProcessEventAppendRequest::new(
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
    let canceller = crate::task::spawn(async move {
        wake_started_rx
            .await
            .expect("wake provider call should start");
        cancel_after_wake_started.cancel();
    });

    let drained = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        runtime.stream_next_queued_work(TurnOptions::new(
            cancel,
            named_turn_scope(
                &SessionId::from(SESSION_ID),
                &TurnId::from("cancel-claimed-wake-drain"),
            ),
        )),
    )
    .await
    .expect("cancelled wake drain should finish")
    .expect("cancelled wake drain should not error")
    .ran()
    .expect("cancelled queued input turn should still assemble");
    canceller.await.expect("canceller task");

    assert!(matches!(
        drained.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    assert!(
        crate::store::TurnInputStore::list_pending_turn_inputs(
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
        crate::store::QueuedWorkStore::list_queued_work(
            store.as_ref(),
            &SessionId::from(SESSION_ID)
        )
        .await
        .expect("queued work after cancellation")
        .is_empty(),
        "claimed wake `{}` should be completed by the cancelled turn",
        wake.wake_id
    );
    assert!(
        runtime
            .stream_next_queued_work(TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from(SESSION_ID),
                    &TurnId::from("after-cancel-claimed-wake-drain")
                ),
            ))
            .await
            .expect("post-cancel drain should succeed")
            .ran()
            .is_none(),
        "neither the cancelled input nor the claimed wake should replay"
    );
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

// Regression (ADR 0029): a long-running turn must keep the queued-work claim it
// already holds alive across a stall, no matter how short the lease TTL is.
// Queued-work batches are claimed at active-turn checkpoints under the session
// execution lease's generation; the claim carries no TTL of its own. So a turn
// that claims a batch at one checkpoint, stalls past the (tiny) lease TTL --
// here a slow provider call, while the session lease keeps renewing on its
// background cadence and preserves its generation -- then crosses another
// checkpoint re-runs `claim_ready_queued_work` under the *same* live generation,
// which can never self-steal its own rows. At finalization the original claim
// still owns its rows and the commit succeeds. Before generation fencing this
// failed with `QueuedWorkClaimExpired` because the claim expired under the
// stalled owner.
//
// This test must FAIL if anyone reintroduces time- or renewal-based claim
// invalidation. The turn is driven with an in-process `TurnInput` (not a
// store-claimed pending input) so the queued-work claim is the store claim
// under scrutiny; the equally-unrenewed turn-input claim is covered by the
// conformance generation-supersession cases.
