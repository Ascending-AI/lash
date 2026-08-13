fn recorded_event_intents(event_types: &[&str]) -> crate::ToolIntents {
    crate::ToolIntents::v1(
        event_types
            .iter()
            .enumerate()
            .map(|(index, event_type)| {
                crate::ToolIntent::EmitProcessEvent(crate::EmitProcessEventIntent {
                    session_id: "session".to_string(),
                    process_id: "intent-law-target".to_string(),
                    event_type: (*event_type).to_string(),
                    payload: json!({"source_index": index}),
                })
            })
            .collect(),
    )
}

async fn register_intent_law_target(
    registry: &Arc<crate::TestLocalProcessRegistry>,
    event_types: &[&str],
) {
    registry
        .register_process_with_observers(
            crate::ProcessRegistration::new(
                "intent-law-target",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryDisposition::ExternallyOwned,
                crate::ProcessProvenance::host(),
            )
            .with_extra_event_types(event_types.iter().map(|event_type| {
                crate::ProcessEventType {
                    name: (*event_type).to_string(),
                    payload_schema: crate::LashSchema::any(),
                    semantics: crate::ProcessEventSemanticsSpec::default(),
                }
            })),
            &["session".to_string()],
        )
        .await
        .expect("register the intent law target");
}

fn fixed_intent_dispatch_context(
    controller: Arc<IntentReplayController>,
    registry: Arc<crate::TestLocalProcessRegistry>,
    intents: crate::ToolIntents,
    calls: Arc<AtomicUsize>,
) -> ToolDispatchContext<'static> {
    let definition = named_beta_tool("fixed_intent_law");
    let provider: Arc<dyn ToolProvider> = Arc::new(FixedAttemptIntentTools {
        definition,
        intents,
        calls,
    });
    let mut context = exact_dispatch_context(provider);
    context.effect_controller = RuntimeEffectControllerHandle::shared(controller);
    context.processes = crate::testing::effect_backed_process_service(registry);
    context.clock = Arc::new(FrozenIntentLawClock::new());
    context
}

fn runtime_execution_for_intent_law(
    context: ToolDispatchContext<'static>,
    cancellation: tokio_util::sync::CancellationToken,
) -> crate::RuntimeExecutionContext<'static> {
    let attachment_store = Arc::clone(&context.attachment_store);
    crate::RuntimeExecutionContext::new(
        "session".to_string(),
        Arc::new(context),
        Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
        attachment_store,
        Arc::new(crate::ChronologicalProjection::default()),
        None,
        crate::TurnContext::default(),
    )
    .with_cancellation_token(cancellation)
}

fn intent_law_batch_parent(label: &str) -> crate::RuntimeInvocation {
    crate::RuntimeInvocation::effect(
        crate::RuntimeScope::for_turn("session", "intent-law-turn", 0, 0),
        label,
        crate::RuntimeEffectKind::ToolBatch,
        label,
    )
}

async fn run_fixed_intent_attempt(
    context: &ToolDispatchContext<'static>,
) -> Box<crate::tool_dispatch::ToolDispatchOutcome> {
    let prepared = crate::PreparedToolCall::from_parts(
        "fixed-intent-call",
        "tool:fixed_intent_law",
        "fixed_intent_law",
        json!({"value": "drive"}),
        None,
        serde_json::Value::Null,
    );
    let tool_context = tool_context_for_prepared(context, &prepared);
    match coordinate_prepared_tool_call_launch_with_execution_context(
        context,
        prepared,
        None,
        tool_context,
    )
    .await
    {
        ToolCallLaunch::Done(outcome) => outcome,
        ToolCallLaunch::Pending(_) => panic!("fixed intent law cannot defer"),
    }
}

async fn crash_redrive_law(pause: IntentPausePoint) {
    let event_types = ["intent.crash.first", "intent.crash.second"];
    let registry = Arc::new(crate::TestLocalProcessRegistry::default());
    register_intent_law_target(&registry, &event_types).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let controller = Arc::new(IntentReplayController::new(Some(pause)));
    let context = fixed_intent_dispatch_context(
        Arc::clone(&controller),
        Arc::clone(&registry),
        recorded_event_intents(&event_types),
        Arc::clone(&calls),
    );

    let crashed_context = context.clone();
    let crashed = crate::task::spawn(async move {
        run_fixed_intent_attempt(&crashed_context).await
    });
    controller.wait_until_paused().await;
    crashed.abort();
    assert!(
        crashed
            .await
            .expect_err("the injected crash aborts the first drain")
            .is_cancelled(),
        "the first coordinator task must stop at the selected crash boundary"
    );

    let redriven = run_fixed_intent_attempt(&context).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the attempt result replays"
    );
    assert_eq!(
        redriven
            .intent_outcomes
            .iter()
            .map(crate::ToolIntentExecutionOutcome::kind)
            .collect::<Vec<_>>(),
        vec![
            Some(crate::ToolIntentKind::EmitProcessEvent),
            Some(crate::ToolIntentKind::EmitProcessEvent),
        ]
    );
    assert!(
        redriven
            .intent_outcomes
            .iter()
            .all(|outcome| matches!(outcome, crate::ToolIntentExecutionOutcome::Executed { .. }))
    );
    let events = registry
        .events_after("intent-law-target", 0)
        .await
        .expect("read crash-law target events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type.starts_with("intent.crash."))
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        event_types,
        "redrive neither drops nor duplicates commands and preserves source order"
    );
    for (key, sightings) in controller.frame_sightings() {
        if sightings.len() > 1 {
            assert!(
                sightings.iter().all(|frame| frame == &sightings[0]),
                "redrive changed the command frame for replay key {key}"
            );
        }
    }
}

#[tokio::test]
async fn crash_after_result_commit_redrives_the_recorded_intent_batch() {
    crash_redrive_law(IntentPausePoint::AfterToolAttemptCommit).await;
}

#[tokio::test]
async fn crash_mid_drain_redrives_the_committed_prefix_and_finishes_the_suffix() {
    crash_redrive_law(IntentPausePoint::BeforeProcessCommand(2)).await;
}

#[tokio::test]
async fn crash_mid_intent_replays_the_command_committed_before_its_reply() {
    crash_redrive_law(IntentPausePoint::AfterProcessCommandCommit(1)).await;
}

#[tokio::test]
async fn public_coordinator_redrive_is_byte_stable_after_live_terminal_mutation() {
    let event_types = ["signal.redrive.signal"];
    let registry = Arc::new(crate::TestLocalProcessRegistry::default());
    register_intent_law_target(&registry, &event_types).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let controller = Arc::new(IntentReplayController::new(None));
    let intents = crate::ToolIntents::v1(vec![crate::ToolIntent::SignalProcess(
        crate::SignalProcessIntent {
            session_id: "session".to_string(),
            process_id: "intent-law-target".to_string(),
            signal_name: "redrive.signal".to_string(),
            payload: json!({"recorded": true}),
        },
    )]);
    let context = fixed_intent_dispatch_context(
        Arc::clone(&controller),
        Arc::clone(&registry),
        intents,
        Arc::clone(&calls),
    );

    let first = run_fixed_intent_attempt(&context).await;
    let first_bytes = serde_json::to_vec(&first).expect("serialize first public outcome");
    registry
        .complete_process(
            "intent-law-target",
            crate::ProcessAwaitOutput::Success {
                value: json!("terminal after the first drain"),
                control: None,
            },
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("terminalize the target between public-caller drains");
    let redriven = run_fixed_intent_attempt(&context).await;

    assert_eq!(calls.load(Ordering::SeqCst), 1, "the attempt body replays");
    assert_eq!(
        serde_json::to_vec(&redriven).expect("serialize redriven public outcome"),
        first_bytes,
        "recorded typed outcomes are byte-stable after live mutation"
    );
    for (key, sightings) in controller.frame_sightings() {
        assert!(
            sightings.iter().all(|frame| frame == &sightings[0]),
            "public-caller redrive changed the frame for {key}"
        );
    }
}

#[tokio::test]
async fn refusal_after_success_preserves_the_committed_prefix_and_replays_typed_evidence() {
    let event_types = ["intent.refusal.first"];
    let registry = Arc::new(crate::TestLocalProcessRegistry::default());
    register_intent_law_target(&registry, &event_types).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let controller = Arc::new(IntentReplayController::new(None));
    let intents = crate::ToolIntents::v1(vec![
        crate::ToolIntent::EmitProcessEvent(crate::EmitProcessEventIntent {
            session_id: "session".to_string(),
            process_id: "intent-law-target".to_string(),
            event_type: "intent.refusal.first".to_string(),
            payload: json!({"committed": true}),
        }),
        crate::ToolIntent::CancelProcess(crate::CancelProcessIntent {
            session_id: "session".to_string(),
            process_id: "missing-intent-target".to_string(),
            reason: Some("literal refusal law".to_string()),
        }),
    ]);
    let context = fixed_intent_dispatch_context(
        Arc::clone(&controller),
        Arc::clone(&registry),
        intents,
        Arc::clone(&calls),
    );

    let first = run_fixed_intent_attempt(&context).await;
    assert!(matches!(
        first.intent_outcomes.as_slice(),
        [
            crate::ToolIntentExecutionOutcome::Executed {
                kind: crate::ToolIntentKind::EmitProcessEvent,
                ..
            },
            crate::ToolIntentExecutionOutcome::Refused {
                kind: crate::ToolIntentKind::CancelProcess,
                refusal: crate::ToolIntentRefusalReason::CommandFailed { .. },
                ..
            }
        ]
    ));
    let first_bytes = serde_json::to_vec(&first.intent_outcomes)
        .expect("serialize first refusal-after-success evidence");

    registry
        .register_process(crate::ProcessRegistration::new(
            "missing-intent-target",
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::RecoveryDisposition::ExternallyOwned,
            crate::ProcessProvenance::host(),
        ))
        .await
        .expect("mutate the formerly missing target after the recorded refusal");
    let redriven = run_fixed_intent_attempt(&context).await;
    assert_eq!(
        serde_json::to_vec(&redriven.intent_outcomes)
            .expect("serialize redriven refusal-after-success evidence"),
        first_bytes,
        "the recorded refusal cannot become success after live state changes"
    );
    let events = registry
        .events_after("intent-law-target", 0)
        .await
        .expect("read the committed prefix");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "intent.refusal.first")
            .count(),
        1,
        "the successful prefix executes exactly once"
    );
}

#[tokio::test]
async fn concurrent_batch_drains_intents_in_call_order_then_intent_index() {
    let event_types = [
        "batch-first-call.intent.0",
        "batch-first-call.intent.1",
        "batch-second-call.intent.0",
        "batch-second-call.intent.1",
    ];
    let registry = Arc::new(crate::TestLocalProcessRegistry::default());
    register_intent_law_target(&registry, &event_types).await;
    let provider: Arc<dyn ToolProvider> = Arc::new(OrderedBatchIntentTools {
        definitions: vec![
            named_beta_tool("intent_batch_first"),
            named_beta_tool("intent_batch_second"),
        ],
        second_attempt_finished: Arc::new(tokio::sync::Notify::new()),
    });
    let mut context = exact_dispatch_context(provider);
    context.processes = crate::testing::effect_backed_process_service(registry.clone());
    let execution =
        runtime_execution_for_intent_law(context, tokio_util::sync::CancellationToken::new());
    let batch = crate::PreparedToolBatch::new(
        "intent-order-batch",
        vec![
            crate::PreparedToolCall::from_parts(
                "batch-first-call",
                "tool:intent_batch_first",
                "intent_batch_first",
                json!({"value": "first"}),
                None,
                serde_json::Value::Null,
            ),
            crate::PreparedToolCall::from_parts(
                "batch-second-call",
                "tool:intent_batch_second",
                "intent_batch_second",
                json!({"value": "second"}),
                None,
                serde_json::Value::Null,
            ),
        ],
    );

    let outcome = execution
        .execute_prepared_tool_batch_launches(
            batch,
            intent_law_batch_parent("intent-order-parent"),
            std::collections::HashMap::new(),
        )
        .await
        .expect("execute ordered intent batch");
    assert_eq!(outcome.launches.len(), 2);
    assert!(outcome.launches.iter().all(|launch| matches!(
        launch,
        crate::runtime::ToolCallLaunch::Done { result }
            if result.intent_outcomes.len() == 2
    )));
    let events = registry
        .events_after("intent-law-target", 0)
        .await
        .expect("read ordered intent events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type.contains(".intent."))
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        event_types,
        "completion-race order cannot reorder call-order then intent-index drain"
    );
}

#[tokio::test]
async fn cancellation_before_result_commit_executes_no_intents() {
    let definition = named_beta_tool("blocking_intent_attempt");
    let entered = Arc::new(tokio::sync::Notify::new());
    let provider: Arc<dyn ToolProvider> = Arc::new(BlockingAttemptIntentTools {
        definition,
        entered: Arc::clone(&entered),
    });
    let context = exact_dispatch_context(provider);
    let cancellation = tokio_util::sync::CancellationToken::new();
    let execution = runtime_execution_for_intent_law(context, cancellation.clone());
    let batch = crate::PreparedToolBatch::new(
        "pre-result-cancel-batch",
        vec![crate::PreparedToolCall::from_parts(
            "pre-result-cancel-call",
            "tool:blocking_intent_attempt",
            "blocking_intent_attempt",
            json!({"value": "block"}),
            None,
            serde_json::Value::Null,
        )],
    );
    let run = crate::task::spawn(async move {
        execution
            .execute_prepared_tool_batch_launches(
                batch,
                intent_law_batch_parent("pre-result-cancel-parent"),
                std::collections::HashMap::new(),
            )
            .await
    });
    entered.notified().await;
    cancellation.cancel();
    let outcome = timeout(Duration::from_secs(2), run)
        .await
        .expect("pre-result cancellation must not hang")
        .expect("batch task joins")
        .expect("batch cancellation is a typed launch");
    assert!(matches!(
        outcome.launches.as_slice(),
        [crate::runtime::ToolCallLaunch::Done { result }]
            if matches!(result.output.outcome, crate::ToolCallOutcome::Cancelled(_))
                && result.intent_outcomes.is_empty()
    ));
}

#[tokio::test]
async fn cancellation_after_result_commit_drains_all_intents_unconditionally() {
    let event_types = ["post.cancel.intent.0", "post.cancel.intent.1"];
    let registry = Arc::new(crate::TestLocalProcessRegistry::default());
    register_intent_law_target(&registry, &event_types).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let controller = Arc::new(IntentReplayController::new(Some(
        IntentPausePoint::BeforeProcessCommand(1),
    )));
    let context = fixed_intent_dispatch_context(
        Arc::clone(&controller),
        Arc::clone(&registry),
        recorded_event_intents(&event_types),
        Arc::clone(&calls),
    );
    let cancellation = tokio_util::sync::CancellationToken::new();
    let execution = runtime_execution_for_intent_law(context, cancellation.clone());
    let batch = crate::PreparedToolBatch::new(
        "post-result-cancel-batch",
        vec![crate::PreparedToolCall::from_parts(
            "fixed-intent-call",
            "tool:fixed_intent_law",
            "fixed_intent_law",
            json!({"value": "drive"}),
            None,
            serde_json::Value::Null,
        )],
    );
    let run = crate::task::spawn(async move {
        execution
            .execute_prepared_tool_batch_launches(
                batch,
                intent_law_batch_parent("post-result-cancel-parent"),
                std::collections::HashMap::new(),
            )
            .await
    });
    controller.wait_until_paused().await;
    cancellation.cancel();
    controller.release();
    let outcome = timeout(Duration::from_secs(2), run)
        .await
        .expect("post-result drain must not hang")
        .expect("batch task joins")
        .expect("post-result drain succeeds");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        outcome.launches.as_slice(),
        [crate::runtime::ToolCallLaunch::Done { result }]
            if result.intent_outcomes.len() == 2
                && result.intent_outcomes.iter().all(|outcome| matches!(
                    outcome,
                    crate::ToolIntentExecutionOutcome::Executed { .. }
                ))
    ));
    let events = registry
        .events_after("intent-law-target", 0)
        .await
        .expect("read post-cancel events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type.starts_with("post.cancel.intent."))
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        event_types,
        "live cancellation after commit cannot truncate the durable drain"
    );
}

#[tokio::test]
async fn parent_end_policies_are_literal_and_redrive_stable() {
    let mut context = dispatch_context();
    let registry = Arc::new(crate::TestLocalProcessRegistry::default());
    context.processes = crate::testing::effect_backed_process_service(registry.clone());
    let policies = [
        crate::ProcessParentEndPolicy::Abandon,
        crate::ProcessParentEndPolicy::Cancel,
    ];
    let intents = crate::ToolIntents::v1(
        policies
            .into_iter()
            .enumerate()
            .map(|(index, policy)| {
                crate::ToolIntent::StartProcess(Box::new(crate::StartProcessIntent {
                    session_id: "session".to_string(),
                    request: crate::ProcessStartRequest::external(
                        format!("ignored-parent-policy-{index}"),
                        crate::ProcessOriginator::host_scoped("parent-policy-law"),
                        json!({"policy_index": index}),
                    ),
                    on_parent_end: policy,
                }))
            })
            .collect(),
    );
    let outcomes =
        execute_final_tool_intents(&context, Some("parent-policy-call"), &intents, None).await;
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, crate::ToolIntentExecutionOutcome::Executed { .. }))
    );
    let process_ids = outcomes
        .iter()
        .map(|outcome| match outcome {
            crate::ToolIntentExecutionOutcome::Executed { identity, .. } => {
                identity.replay_key.clone()
            }
            other => panic!("expected executed start, got {other:?}"),
        })
        .collect::<Vec<_>>();

    context.recorded_intent_outcomes.record(&outcomes);
    intent_executor::execute_parent_end_actions(&context)
        .await
        .expect("execute first parent-end actions");
    let first_events = futures_util::future::try_join_all(
        process_ids
            .iter()
            .map(|process_id| registry.events_after(process_id, 0)),
    )
    .await
    .expect("read first parent-end event sets");
    assert_eq!(
        first_events
            .iter()
            .map(|events| {
                events
                    .iter()
                    .filter(|event| event.event_type == "process.cancel_requested")
                    .count()
            })
            .collect::<Vec<_>>(),
        vec![0, 1],
        "Abandon is a recorded no-op and Cancel emits exactly one command"
    );
    assert_eq!(
        first_events[1][0].payload["reason"],
        json!("recorded start intent parent ended with cancel policy")
    );
    context.recorded_intent_outcomes.record(&outcomes);
    intent_executor::execute_parent_end_actions(&context)
        .await
        .expect("redrive parent-end actions");
    let redriven_events = futures_util::future::try_join_all(
        process_ids
            .iter()
            .map(|process_id| registry.events_after(process_id, 0)),
    )
    .await
    .expect("read redriven parent-end event sets");
    assert_eq!(
        serde_json::to_vec(&redriven_events).expect("serialize redriven events"),
        serde_json::to_vec(&first_events).expect("serialize first events"),
        "parent-end command outcomes are byte-stable on redrive"
    );
}

#[tokio::test]
async fn parent_end_cancel_refusal_is_typed_and_operator_visible() {
    let mut context = dispatch_context();
    let registry = Arc::new(crate::TestLocalProcessRegistry::default());
    context.processes = crate::testing::effect_backed_process_service(registry);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
    context.event_tx = event_tx;
    let identity = crate::derive_tool_intent_identity(
        "session",
        "turn",
        Some("missing-parent-end-call"),
        0,
    )
    .expect("derive valid missing-child intent identity");
    context
        .recorded_intent_outcomes
        .record(&[crate::ToolIntentExecutionOutcome::Executed {
            identity,
            kind: crate::ToolIntentKind::StartProcess,
            result: json!({"id": "missing-parent-end-child"}),
            parent_end: Some(crate::ToolIntentParentEnd {
                process_id: "missing-parent-end-child".to_string(),
                policy: crate::ProcessParentEndPolicy::Cancel,
            }),
        }]);

    intent_executor::execute_parent_end_actions(&context)
        .await
        .expect("a business refusal is a recorded parent-end outcome");
    let event = event_rx
        .recv()
        .await
        .expect("operator-visible refusal event");
    let crate::SessionStreamEvent::Error { message, envelope } = event else {
        panic!("parent-end refusal must register an error event");
    };
    assert!(message.contains("missing-parent-end-child"));
    let envelope = envelope.expect("typed parent-end error envelope");
    assert_eq!(envelope.kind, "tool_intent_parent_end");
    assert_eq!(envelope.code.as_deref(), Some("plugin"));
}

#[tokio::test]
async fn retry_drains_only_the_final_attempts_intents() {
    let definition =
        named_beta_tool("retry_intents").with_retry_policy(crate::ToolRetryPolicy::safe(2, 0, 0));
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ToolProvider> = Arc::new(RetryingIntentTools {
        definition: definition.clone(),
        calls: Arc::clone(&calls),
    });
    let mut context = exact_dispatch_context(provider);
    let registry = Arc::new(crate::TestLocalProcessRegistry::default());
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "retry-intent-target",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryDisposition::ExternallyOwned,
                crate::ProcessProvenance::host(),
            )
            .with_extra_event_types([crate::ProcessEventType {
                name: "attempt.retry.final".to_string(),
                payload_schema: crate::LashSchema::any(),
                semantics: crate::ProcessEventSemanticsSpec::default(),
            }]),
        )
        .await
        .expect("register retry intent target");
    context.processes = crate::testing::effect_backed_process_service(registry.clone());
    let prepared = crate::PreparedToolCall::from_parts(
        "retry-intents-call",
        definition.id().to_string(),
        "retry_intents",
        json!({"value": "drive"}),
        None,
        serde_json::Value::Null,
    );
    let tool_context = tool_context_for_prepared(&context, &prepared);
    let launch = coordinate_prepared_tool_call_launch_with_execution_context(
        &context,
        prepared,
        None,
        tool_context,
    )
    .await;
    let ToolCallLaunch::Done(outcome) = launch else {
        panic!("retrying provider completes on its second attempt");
    };
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(outcome.attempts.len(), 2);
    assert_eq!(outcome.intent_outcomes.len(), 1);
    let events = registry
        .events_after("retry-intent-target", 0)
        .await
        .expect("read retry intent target events");
    assert_eq!(events.len(), 1, "the retried declaration never drains");
    assert_eq!(events[0].payload, json!({"attempt": 2}));
}
