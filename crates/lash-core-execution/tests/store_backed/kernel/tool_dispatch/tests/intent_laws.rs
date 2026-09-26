use super::*;
use crate::ProcessId;
use crate::SessionId;

/// One memory backend's registry, env store and trigger store: every port
/// an intent law's dispatch, process service, runtime execution and trigger
/// router share.
struct IntentLawWorld {
    registry: Arc<dyn crate::ProcessRegistry>,
    env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    trigger_store: Arc<dyn crate::TriggerStore>,
}

async fn intent_law_world() -> IntentLawWorld {
    let backend = crate::support::memory_store_set().await;
    IntentLawWorld {
        registry: backend.process_registry(),
        env_store: backend.process_env_store(),
        trigger_store: backend.trigger_store(),
    }
}

fn recorded_event_intents(event_types: &[&str]) -> crate::ToolIntents {
    crate::ToolIntents::v3(
        event_types
            .iter()
            .enumerate()
            .map(|(index, event_type)| {
                crate::ToolIntent::EmitProcessEvent(crate::EmitProcessEventIntent {
                    session_id: SessionId::from("session"),
                    process_id: ProcessId::from("intent-law-target"),
                    event_type: (*event_type).to_string(),
                    payload: json!({"source_index": index}),
                })
            })
            .collect(),
    )
}

async fn register_intent_law_target(
    registry: &Arc<dyn crate::ProcessRegistry>,
    event_types: &[&str],
) {
    registry
        .register_process_with_observers(
            crate::ProcessRegistration::new(
                "intent-law-target",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::host(),
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types(event_types.iter().map(|event_type| {
                crate::ProcessEventType {
                    name: (*event_type).to_string(),
                    payload_schema: crate::LashSchema::any(),
                    semantics: crate::ProcessEventSemanticsSpec::default(),
                }
            })),
            &[SessionId::from("session")],
        )
        .await
        .expect("register the intent law target");
}

async fn fixed_intent_dispatch_context(
    controller: Arc<IntentReplayController>,
    world: &IntentLawWorld,
    intents: crate::ToolIntents,
    calls: Arc<AtomicUsize>,
) -> ToolDispatchContext<'static> {
    let definition = named_beta_tool("fixed_intent_law");
    let provider: Arc<dyn ToolProvider> = Arc::new(FixedAttemptIntentTools {
        definition,
        intents,
        calls,
    });
    let mut context = exact_dispatch_context(provider).await;
    context.effect_controller = RuntimeEffectControllerHandle::shared(controller);
    context.processes = crate::testing::effect_backed_process_service(
        Arc::clone(&world.registry),
        Arc::clone(&world.env_store),
    );
    context.clock = Arc::new(FrozenIntentLawClock::new());
    context
}

fn runtime_execution_for_intent_law(
    context: ToolDispatchContext<'static>,
    world: &IntentLawWorld,
    cancellation: tokio_util::sync::CancellationToken,
) -> crate::RuntimeExecutionContext<'static> {
    let attachment_store = Arc::clone(&context.attachment_store);
    crate::RuntimeExecutionContext::new(
        SessionId::from("session"),
        Arc::new(context),
        Arc::clone(&world.env_store),
        attachment_store,
        Arc::new(crate::ChronologicalProjection::default()),
        None,
        crate::TurnContext::default(),
    )
    .with_cancellation_token(cancellation)
}

fn intent_law_batch_parent(label: &str) -> crate::RuntimeInvocation {
    crate::RuntimeInvocation::effect(
        crate::EffectAddress::new(
            crate::ExecutionScope::turn("session", "intent-law-turn"),
            label,
        )
        .expect("valid intent-law address"),
        crate::RuntimeAttribution::for_turn("session", "intent-law-turn", 0, 0),
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
        ToolCallLaunch::ControllerAborted(error) => {
            panic!("fixed intent law unexpectedly aborted: {error}")
        }
    }
}

async fn crash_redrive_law(pause: IntentPausePoint) {
    let event_types = ["intent.crash.first", "intent.crash.second"];
    let world = intent_law_world().await;
    let registry = Arc::clone(&world.registry);
    register_intent_law_target(&registry, &event_types).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let controller = Arc::new(IntentReplayController::new(Some(pause)).await);
    let context = fixed_intent_dispatch_context(
        Arc::clone(&controller),
        &world,
        recorded_event_intents(&event_types),
        Arc::clone(&calls),
    )
    .await;

    let crashed_context = context.clone();
    let crashed =
        crate::task::spawn(async move { run_fixed_intent_attempt(&crashed_context).await });
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
        .full_event_window(&ProcessId::from("intent-law-target"), 0)
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

fn recorded_start_intents() -> crate::ToolIntents {
    crate::ToolIntents::v3(vec![crate::ToolIntent::StartProcess(Box::new(
        crate::StartProcessIntent {
            session_id: SessionId::from("session"),
            declaration: crate::ProcessStartDeclaration::external(
                crate::ProcessOriginator::host_scoped("intent-law"),
                json!({"step": "start"}),
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
                ),
            ),
        },
    ))])
}

fn started_process_id(outcome: &crate::ToolIntentExecutionOutcome) -> (ProcessId, ProcessId) {
    match outcome {
        crate::ToolIntentExecutionOutcome::Executed {
            identity, result, ..
        } => (
            // The outcome carries the handle and the parts it names. The
            // process id is `process_id`; `id` is the opaque handle (ADR 0095)
            // and reading it here is what the one handle kind stops.
            ProcessId::from(
                result
                    .get("process_id")
                    .and_then(serde_json::Value::as_str)
                    .expect("a start outcome names its process id")
                    .to_string(),
            ),
            ProcessId::from_intent_identity(identity),
        ),
        other => panic!("expected an executed start intent, got {other:?}"),
    }
}

/// FIG-2994: the id the attempt can derive from its own intent identity and
/// the id the executor starts under are one value, on the first drain and on
/// the redrive of the same attempt after a crash. Replacing the executor's
/// `ProcessId::from_intent_identity` call with a freshly minted id fails the
/// first assertion; carrying the id in the declaration instead of deriving it
/// fails the redrive equality.
#[tokio::test]
async fn crash_redrive_of_a_start_declaration_derives_the_same_process_id() {
    let world = intent_law_world().await;
    let registry = Arc::clone(&world.registry);
    let calls = Arc::new(AtomicUsize::new(0));
    let controller = Arc::new(
        IntentReplayController::new(Some(IntentPausePoint::AfterProcessCommandCommit(1))).await,
    );
    let context = fixed_intent_dispatch_context(
        Arc::clone(&controller),
        &world,
        recorded_start_intents(),
        Arc::clone(&calls),
    )
    .await;

    let crashed_context = context.clone();
    let crashed =
        crate::task::spawn(async move { run_fixed_intent_attempt(&crashed_context).await });
    controller.wait_until_paused().await;
    crashed.abort();
    assert!(
        crashed
            .await
            .expect_err("the injected crash aborts the first drain")
            .is_cancelled(),
        "the first coordinator task must stop after the start command commits"
    );

    let redriven = run_fixed_intent_attempt(&context).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the attempt result replays rather than re-running the tool body"
    );
    let outcome = redriven
        .intent_outcomes
        .first()
        .expect("the redrive reports the start declaration");
    let (started, derived) = started_process_id(outcome);
    assert_eq!(
        started, derived,
        "the executor must start under the id the attempt identity derives"
    );

    let live = registry
        .list_processes(&crate::ProcessListFilter::default())
        .await
        .expect("list the started processes")
        .into_iter()
        .map(|record| record.id)
        .filter(|id| *id == derived)
        .collect::<Vec<_>>();
    assert_eq!(
        live,
        vec![derived],
        "the crash and its redrive must converge on exactly one process row"
    );
}

#[tokio::test]
async fn crash_after_result_commit_redrives_the_recorded_intent_batch() {
    Box::pin(crash_redrive_law(IntentPausePoint::AfterToolAttemptCommit)).await;
}

#[tokio::test]
async fn crash_mid_drain_redrives_the_committed_prefix_and_finishes_the_suffix() {
    Box::pin(crash_redrive_law(IntentPausePoint::BeforeProcessCommand(2))).await;
}

#[tokio::test]
async fn crash_mid_intent_replays_the_command_committed_before_its_reply() {
    Box::pin(crash_redrive_law(
        IntentPausePoint::AfterProcessCommandCommit(1),
    ))
    .await;
}

#[tokio::test]
async fn public_coordinator_redrive_is_byte_stable_after_live_terminal_mutation() {
    let event_types = ["signal.redrive.signal"];
    let world = intent_law_world().await;
    let registry = Arc::clone(&world.registry);
    register_intent_law_target(&registry, &event_types).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let controller = Arc::new(IntentReplayController::new(None).await);
    let intents = crate::ToolIntents::v3(vec![crate::ToolIntent::SignalProcess(
        crate::SignalProcessIntent {
            session_id: SessionId::from("session"),
            process_id: ProcessId::from("intent-law-target"),
            signal_name: "redrive.signal".to_string(),
            payload: json!({"recorded": true}),
        },
    )]);
    let context =
        fixed_intent_dispatch_context(Arc::clone(&controller), &world, intents, Arc::clone(&calls))
            .await;

    let first = run_fixed_intent_attempt(&context).await;
    let first_bytes = serde_json::to_vec(&first).expect("serialize first public outcome");
    registry
        .complete_process(
            &ProcessId::from("intent-law-target"),
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(json!(
                "terminal after the first drain"
            ))),
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
async fn tool_intent_outcome_replay_is_scoped_to_its_minting_emission() {
    let event_type = "intent.emission.recovered";
    let world = intent_law_world().await;
    let registry = Arc::clone(&world.registry);
    let calls = Arc::new(AtomicUsize::new(0));
    let controller = Arc::new(IntentReplayController::new(None).await);
    let mut first_emission = fixed_intent_dispatch_context(
        Arc::clone(&controller),
        &world,
        recorded_event_intents(&[event_type]),
        Arc::clone(&calls),
    )
    .await;
    first_emission.parent_invocation = Some(intent_law_batch_parent("emission-turn-n"));

    let refused = run_fixed_intent_attempt(&first_emission).await;
    assert!(
        matches!(
            refused.intent_outcomes.as_slice(),
            [crate::ToolIntentExecutionOutcome::Refused {
                kind: crate::ToolIntentKind::EmitProcessEvent,
                refusal: crate::ToolIntentRefusalReason::CommandFailed { .. },
                ..
            }]
        ),
        "the first emission must retain its terminal validation refusal: {:?}",
        refused.intent_outcomes
    );

    register_intent_law_target(&registry, &[event_type]).await;
    let mut later_emission = first_emission.clone();
    later_emission.parent_invocation = Some(intent_law_batch_parent("emission-turn-n-plus-1"));

    let recovered = run_fixed_intent_attempt(&later_emission).await;
    assert!(
        matches!(
            recovered.intent_outcomes.as_slice(),
            [crate::ToolIntentExecutionOutcome::Executed {
                kind: crate::ToolIntentKind::EmitProcessEvent,
                ..
            }]
        ),
        "the later emission must reach validation instead of replaying the old refusal: {:?}",
        recovered.intent_outcomes
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "distinct emissions execute two provider attempts"
    );

    let recovered_bytes = serde_json::to_vec(&recovered).expect("serialize recovered outcome");
    let replayed = run_fixed_intent_attempt(&later_emission).await;
    assert_eq!(
        serde_json::to_vec(&replayed).expect("serialize replayed outcome"),
        recovered_bytes,
        "the same emission must replay its recorded outcome byte-for-byte"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "same-emission replay must not execute another provider attempt"
    );
    assert_eq!(
        registry
            .full_event_window(&ProcessId::from("intent-law-target"), 0)
            .await
            .expect("read recovered target events")
            .iter()
            .filter(|event| event.event_type == event_type)
            .count(),
        1,
        "same-emission replay must not duplicate the realized command"
    );
}

#[tokio::test]
async fn refusal_after_success_preserves_the_committed_prefix_and_replays_typed_evidence() {
    let event_types = ["intent.refusal.first"];
    let world = intent_law_world().await;
    let registry = Arc::clone(&world.registry);
    register_intent_law_target(&registry, &event_types).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let controller = Arc::new(IntentReplayController::new(None).await);
    let intents = crate::ToolIntents::v3(vec![
        crate::ToolIntent::EmitProcessEvent(crate::EmitProcessEventIntent {
            session_id: SessionId::from("session"),
            process_id: ProcessId::from("intent-law-target"),
            event_type: "intent.refusal.first".to_string(),
            payload: json!({"committed": true}),
        }),
        crate::ToolIntent::CancelProcess(crate::CancelProcessIntent {
            session_id: SessionId::from("session"),
            process_id: ProcessId::from("missing-intent-target"),
        }),
    ]);
    let context =
        fixed_intent_dispatch_context(Arc::clone(&controller), &world, intents, Arc::clone(&calls))
            .await;

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
            crate::RecoveryContract::ExternallyOwned,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
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
        .full_event_window(&ProcessId::from("intent-law-target"), 0)
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
async fn replay_mismatch_during_scalar_intent_drain_latches_the_enclosing_effect_abort() {
    let world = intent_law_world().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let controller = Arc::new(IntentReplayController::new(None).await.with_process_abort(
        crate::RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::EffectReplayDivergence,
            "reconstructed process-command envelope diverged",
        ),
    ));
    let context = fixed_intent_dispatch_context(
        controller,
        &world,
        recorded_event_intents(&["replacement.abort"]),
        Arc::clone(&calls),
    )
    .await;
    let execution = runtime_execution_for_intent_law(
        context,
        &world,
        tokio_util::sync::CancellationToken::new(),
    );

    let reply = Box::pin(execution.call_command_tool(
        &crate::CommandReplayKey::new("fixed-intent-call"),
        crate::session::ToolInvocation::new(
            "fixed-intent-call",
            crate::ToolId::from("tool:fixed_intent_law"),
            json!({"value": "drive"}),
        ),
    ))
    .await;

    assert!(!reply.output.is_success());
    let error = execution
        .take_nested_effect_error()
        .expect("the fixed scalar host reply must latch the enclosing controller abort");
    assert_eq!(error.code, crate::RuntimeErrorCode::EffectReplayDivergence);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ordinary_controller_wrapped_intent_refusal_preserves_its_typed_code() {
    let world = intent_law_world().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let controller = Arc::new(IntentReplayController::new(None).await.with_process_abort(
        crate::RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
            "process command returned the wrong outcome kind",
        ),
    ));
    let context = fixed_intent_dispatch_context(
        controller,
        &world,
        recorded_event_intents(&["controller.refusal"]),
        calls,
    )
    .await;

    let outcome = run_fixed_intent_attempt(&context).await;
    let crate::ToolIntentExecutionOutcome::Refused {
        refusal: crate::ToolIntentRefusalReason::CommandFailed { code, message },
        ..
    } = &outcome.intent_outcomes[0]
    else {
        panic!("ordinary controller-wrapped failures must remain per-intent refusals")
    };
    assert_eq!(code, "runtime_effect_wrong_outcome");
    assert_eq!(message, "process command returned the wrong outcome kind");
}

#[tokio::test]
async fn cancellation_after_result_commit_drains_all_intents_unconditionally() {
    let event_types = ["post.cancel.intent.0", "post.cancel.intent.1"];
    let world = intent_law_world().await;
    let registry = Arc::clone(&world.registry);
    register_intent_law_target(&registry, &event_types).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let controller = Arc::new(
        IntentReplayController::new(Some(IntentPausePoint::BeforeProcessCommand(1))).await,
    );
    let context = fixed_intent_dispatch_context(
        Arc::clone(&controller),
        &world,
        recorded_event_intents(&event_types),
        Arc::clone(&calls),
    )
    .await;
    let cancellation = tokio_util::sync::CancellationToken::new();
    let execution = runtime_execution_for_intent_law(context, &world, cancellation.clone());
    let run = crate::task::spawn(async move {
        Box::pin(execution.call_command_tool(
            &crate::CommandReplayKey::new("fixed-intent-call"),
            crate::session::ToolInvocation::new(
                "fixed-intent-call",
                crate::ToolId::from("tool:fixed_intent_law"),
                json!({"value": "drive"}),
            ),
        ))
        .await
    });
    controller.wait_until_paused().await;
    cancellation.cancel();
    controller.release();
    let reply = timeout(Duration::from_secs(2), run)
        .await
        .expect("post-result drain must not hang")
        .expect("call task joins");
    assert!(
        reply.output.is_success(),
        "the committed result stands after a post-commit cancel: {:?}",
        reply.output
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let events = registry
        .full_event_window(&ProcessId::from("intent-law-target"), 0)
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
async fn retry_drains_only_the_final_attempts_intents() {
    let definition =
        named_beta_tool("retry_intents").with_retry_policy(crate::ToolRetryPolicy::safe(2, 0, 0));
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ToolProvider> = Arc::new(RetryingIntentTools {
        definition: definition.clone(),
        calls: Arc::clone(&calls),
    });
    let mut context = exact_dispatch_context(provider).await;
    let world = intent_law_world().await;
    let registry = Arc::clone(&world.registry);
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "retry-intent-target",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::host(),
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([crate::ProcessEventType {
                name: "attempt.retry.final".to_string(),
                payload_schema: crate::LashSchema::any(),
                semantics: crate::ProcessEventSemanticsSpec::default(),
            }]),
        )
        .await
        .expect("register retry intent target");
    context.processes = crate::testing::effect_backed_process_service(
        Arc::clone(&registry),
        Arc::clone(&world.env_store),
    );
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
        .full_event_window(&ProcessId::from("retry-intent-target"), 0)
        .await
        .expect("read retry intent target events");
    assert_eq!(events.len(), 1, "the retried declaration never drains");
    assert_eq!(events[0].payload, json!({"attempt": 2}));
}
#[tokio::test]
async fn empty_v2_batch_without_a_recorded_call_id_is_a_noop() {
    let outcomes = execute_final_tool_intents(
        &dispatch_context().await,
        None,
        &crate::ToolIntents::default(),
        None,
    )
    .await
    .expect("empty v2 batch is a no-op");
    assert!(outcomes.is_empty());
}

async fn register_trigger_intent_subscription(
    world: &IntentLawWorld,
) -> crate::TriggerSubscriptionRecord {
    register_trigger_intent_subscription_with_schema(world, crate::LashSchema::any()).await
}

async fn register_trigger_intent_subscription_with_schema(
    world: &IntentLawWorld,
    payload_schema: crate::LashSchema,
) -> crate::TriggerSubscriptionRecord {
    let store = &world.trigger_store;
    let process_env_ref =
        crate::testing::process_execution_env_fixture(world.env_store.as_ref()).await;
    let draft = crate::TriggerSubscriptionDraft::for_process(
        "test/intent-trigger-delivery",
        process_env_ref,
        "intent.trigger.emitted",
        "intent-law-source",
        crate::ProcessInput::Engine {
            kind: "testing-fixture".to_string(),
            payload: json!({"process": "intent-trigger-delivery"}),
        },
        crate::ProcessIdentity::labelled("testing-fixture", Some("intent-trigger-delivery")),
    )
    .with_payload_schema(payload_schema);
    let outcome = store
        .execute_command(
            "intent-trigger-subscription",
            crate::TriggerCommand::Register {
                owner_scope: crate::TriggerOwnerScope::host("intent-law").expect("owner scope"),
                actor: crate::ProcessOriginator::host_scoped("intent-law"),
                draft,
            },
        )
        .await
        .expect("execute the intent-law trigger registration")
        .expect("register the intent-law trigger subscription");
    let crate::TriggerCommandOutcome::Mutation { receipt } = outcome else {
        panic!("registration must return a mutation receipt")
    };
    receipt.record_snapshot
}

fn recorded_trigger_intents() -> crate::ToolIntents {
    crate::ToolIntents::v3(vec![crate::ToolIntent::EmitTrigger(
        crate::EmitTriggerIntent {
            session_id: SessionId::from("session"),
            request: crate::TriggerOccurrenceRequest::new(
                "intent.trigger.emitted",
                "intent-law-source",
                json!({"declared": true}),
                "intent-law-occurrence",
            ),
        },
    )])
}

/// Builds a dispatch context whose single recorded intent is the trigger
/// declaration, routed at a router over `store` with a process registry so the
/// reserved delivery actually starts.
async fn trigger_intent_dispatch_context(
    controller: Arc<IntentReplayController>,
    world: &IntentLawWorld,
    calls: Arc<AtomicUsize>,
) -> ToolDispatchContext<'static> {
    let mut context =
        fixed_intent_dispatch_context(controller, world, recorded_trigger_intents(), calls).await;
    context.trigger_router = Some(
        crate::TriggerRouter::new(
            Arc::clone(&world.trigger_store),
            crate::testing::process_work_wiring_for_registry(Arc::clone(&world.registry)),
        )
        .with_process_artifacts(
            Arc::clone(&world.env_store),
            crate::testing::process_engine_fixture(),
        ),
    );
    context
}

/// Drives one recorded `EmitTrigger` declaration, crashes the coordinator at
/// `pause`, and redrives it. Returns the store, the registered subscription and
/// the redriven outcome so each half of the exactly-once law can assert on the
/// state its crash point leaves behind.
async fn crashed_trigger_intent_redrive(
    pause: IntentPausePoint,
    at_crash: impl AsyncFnOnce(&dyn crate::TriggerStore),
) -> (
    Arc<dyn crate::TriggerStore>,
    crate::TriggerSubscriptionRecord,
    Box<crate::tool_dispatch::ToolDispatchOutcome>,
) {
    let world = intent_law_world().await;
    let store = Arc::clone(&world.trigger_store);
    let subscription = register_trigger_intent_subscription(&world).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let controller = Arc::new(IntentReplayController::new(Some(pause)).await);
    let context =
        trigger_intent_dispatch_context(controller.clone(), &world, Arc::clone(&calls)).await;

    let crashed_context = context.clone();
    let crashed =
        crate::task::spawn(async move { run_fixed_intent_attempt(&crashed_context).await });
    controller.wait_until_paused().await;
    crashed.abort();
    assert!(
        crashed
            .await
            .expect_err("the injected crash aborts the first drain")
            .is_cancelled(),
        "the first coordinator task must stop at the injected crash point"
    );
    at_crash(store.as_ref()).await;

    let redriven = run_fixed_intent_attempt(&context).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the attempt result replays instead of re-running the provider"
    );
    (store, subscription, redriven)
}

fn executed_trigger_outcome(
    outcome: &crate::tool_dispatch::ToolDispatchOutcome,
) -> (&serde_json::Value, String) {
    let [
        crate::ToolIntentExecutionOutcome::Executed {
            kind: crate::ToolIntentKind::EmitTrigger,
            result,
            ..
        },
    ] = outcome.intent_outcomes.as_slice()
    else {
        panic!(
            "the drain must execute the recorded trigger declaration: {:?}",
            outcome.intent_outcomes
        )
    };
    let occurrence_id = result["occurrence_id"]
        .as_str()
        .expect("the executed outcome reports its occurrence")
        .to_string();
    (result, occurrence_id)
}

/// The at-least-once half of the atomic-emission law: a recorded `EmitTrigger`
/// declaration reaches the trigger router only after the attempt result
/// commits, so a crash in between emits nothing, and the redrive still ingests
/// the occurrence and reserves its delivery.
#[tokio::test]
async fn crash_after_result_commit_emits_the_recorded_trigger_exactly_once() {
    let (store, subscription, redriven) = Box::pin(crashed_trigger_intent_redrive(
        IntentPausePoint::AfterToolAttemptCommit,
        async |store| {
            assert_eq!(
                store
                    .list_occurrences(crate::TriggerOccurrenceFilter::default())
                    .await
                    .expect("read occurrences after the crash")
                    .len(),
                0,
                "no occurrence may exist before the recorded declaration drains"
            );
        },
    ))
    .await;

    let (_, occurrence_id) = executed_trigger_outcome(&redriven);
    let occurrences = store
        .list_occurrences(crate::TriggerOccurrenceFilter::default())
        .await
        .expect("read occurrences after the redrive");
    assert_eq!(
        occurrences.len(),
        1,
        "redrive must not duplicate the emission"
    );
    assert_eq!(occurrences[0].occurrence_id, occurrence_id);
    let deliveries = store
        .list_deliveries_by_occurrence_id(&occurrence_id)
        .await
        .expect("read the reserved deliveries");
    assert_eq!(deliveries.len(), 1, "exactly one delivery is reserved");
    assert_eq!(
        deliveries[0].subscription.subscription_id,
        subscription.subscription_id
    );
}

/// A delivery that never started is the declaration's own refusal, not a
/// failure buried inside a successful outcome. Its reason is a live error
/// string the next drive need not reproduce, so letting it reach
/// `Executed { result }` would both call a start that did not happen a success
/// and put replay-varying bytes on the durable wire.
#[tokio::test]
async fn recorded_trigger_refuses_when_a_delivery_does_not_start() {
    let world = intent_law_world().await;
    register_trigger_intent_subscription_with_schema(
        &world,
        crate::LashSchema::new(json!({"type": "string"})),
    )
    .await;
    let controller = Arc::new(IntentReplayController::new(None).await);
    let context =
        trigger_intent_dispatch_context(controller, &world, Arc::new(AtomicUsize::new(0))).await;

    let outcome = run_fixed_intent_attempt(&context).await;

    let [
        crate::ToolIntentExecutionOutcome::Refused {
            kind: crate::ToolIntentKind::EmitTrigger,
            refusal: crate::ToolIntentRefusalReason::CommandFailed { message, .. },
            ..
        },
    ] = outcome.intent_outcomes.as_slice()
    else {
        panic!(
            "a declaration whose delivery did not start must refuse: {:?}",
            outcome.intent_outcomes
        )
    };
    assert!(
        message.contains("did not start") && message.contains("invalid payload for trigger"),
        "the refusal must name the unstarted delivery and why: {message}"
    );
}

/// The trigger arm of the public byte-stability law: two clean drives of the
/// same recorded declaration hand the caller identical bytes, even though the
/// second drive re-ingests an occurrence that already exists and re-starts a
/// delivery the store now reads back as `AlreadyReserved` (FIG-806).
#[tokio::test]
async fn public_coordinator_redrive_is_byte_stable_for_the_recorded_trigger_emission() {
    let world = intent_law_world().await;
    let store = Arc::clone(&world.trigger_store);
    let subscription = register_trigger_intent_subscription(&world).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let controller = Arc::new(IntentReplayController::new(None).await);
    let context =
        trigger_intent_dispatch_context(Arc::clone(&controller), &world, Arc::clone(&calls)).await;

    let first = run_fixed_intent_attempt(&context).await;
    // Rendered rather than raw bytes: this is the same byte equality, with a
    // readable diff when a live read leaks into the recorded outcome.
    let first_rendered = serde_json::to_string(&first).expect("serialize the first public outcome");
    let (_, occurrence_id) = executed_trigger_outcome(&first);
    assert_eq!(
        store
            .list_deliveries_by_occurrence_id(&occurrence_id)
            .await
            .expect("read the deliveries the first drive reserved")
            .len(),
        1,
        "the first drive must reserve the delivery the second one reads back as already reserved"
    );

    let redriven = run_fixed_intent_attempt(&context).await;

    assert_eq!(calls.load(Ordering::SeqCst), 1, "the attempt body replays");
    assert_eq!(
        serde_json::to_string(&redriven).expect("serialize the redriven public outcome"),
        first_rendered,
        "the recorded trigger outcome is byte-stable across drives"
    );
    let deliveries = store
        .list_deliveries_by_occurrence_id(&occurrence_id)
        .await
        .expect("read the deliveries after the redrive");
    assert_eq!(deliveries.len(), 1, "the redrive reserves nothing new");
    assert_eq!(
        deliveries[0].subscription.subscription_id,
        subscription.subscription_id
    );
    for (key, sightings) in controller.frame_sightings() {
        assert!(
            sightings.iter().all(|frame| frame == &sightings[0]),
            "public-caller redrive changed the frame for {key}"
        );
    }
}

/// The occurrence dedupe point belongs to the durable declaration, not to the
/// caller-selected key inside its payload. Distinct declaration replay keys
/// must produce distinct occurrences even when callers reuse that key, while
/// redriving those same declarations must not add another occurrence.
#[tokio::test]
async fn recorded_trigger_occurrence_identity_follows_the_declaration_replay_key() {
    let world = intent_law_world().await;
    let store = Arc::clone(&world.trigger_store);
    register_trigger_intent_subscription(&world).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let controller = Arc::new(IntentReplayController::new(None).await);
    let declaration = crate::ToolIntent::EmitTrigger(crate::EmitTriggerIntent {
        session_id: SessionId::from("session"),
        request: crate::TriggerOccurrenceRequest::new(
            "intent.trigger.emitted",
            "intent-law-source",
            json!({"declared": true}),
            "shared-caller-key",
        ),
    });
    let mut context = fixed_intent_dispatch_context(
        Arc::clone(&controller),
        &world,
        crate::ToolIntents::v3(vec![declaration.clone(), declaration]),
        Arc::clone(&calls),
    )
    .await;
    context.trigger_router = Some(
        crate::TriggerRouter::new(
            Arc::clone(&store),
            crate::testing::process_work_wiring_for_registry(Arc::clone(&world.registry)),
        )
        .with_process_artifacts(
            Arc::clone(&world.env_store),
            crate::testing::process_engine_fixture(),
        ),
    );

    let first = run_fixed_intent_attempt(&context).await;
    let replay_keys = first
        .intent_outcomes
        .iter()
        .map(|outcome| match outcome {
            crate::ToolIntentExecutionOutcome::Executed { identity, .. } => {
                identity.replay_key.clone()
            }
            outcome => panic!("both trigger declarations must execute: {outcome:?}"),
        })
        .collect::<Vec<_>>();
    assert_ne!(
        replay_keys[0], replay_keys[1],
        "the two declarations must have distinct replay keys"
    );
    let occurrences = store
        .list_occurrences(crate::TriggerOccurrenceFilter::default())
        .await
        .expect("read distinct declaration occurrences");
    assert_eq!(
        occurrences.len(),
        2,
        "same caller key must not collapse distinct declarations"
    );
    assert_eq!(
        occurrences
            .iter()
            .map(|occurrence| occurrence.idempotency_key.clone())
            .collect::<std::collections::BTreeSet<_>>(),
        replay_keys.iter().cloned().collect(),
        "each occurrence must be stamped with its declaration replay key"
    );

    let redriven = run_fixed_intent_attempt(&context).await;

    assert_eq!(calls.load(Ordering::SeqCst), 1, "the attempt body replays");
    assert_eq!(
        redriven.intent_outcomes, first.intent_outcomes,
        "redrive reports the same declaration outcomes"
    );
    assert_eq!(
        store
            .list_occurrences(crate::TriggerOccurrenceFilter::default())
            .await
            .expect("read occurrences after redrive")
            .len(),
        2,
        "redriving the same replay keys must not add occurrences"
    );
}

/// A fresh coordinator reconstructing a committed predecessor attempt must
/// reject its v1 declaration batch before touching the trigger store. The
/// predecessor occurrence witnesses the dangerous crash window: it was
/// ingested under the caller key, but no executed intent report exists.
#[tokio::test]
async fn cold_public_coordinator_refuses_v1_trigger_batch_before_store_ingress() {
    let world = intent_law_world().await;
    let store = Arc::clone(&world.trigger_store);
    register_trigger_intent_subscription(&world).await;
    let controller = Arc::new(IntentReplayController::new(None).await);
    let request = crate::TriggerOccurrenceRequest::new(
        "intent.trigger.emitted",
        "intent-law-source",
        json!({"declared": true}),
        "predecessor-caller-key",
    );
    let registry = Arc::clone(&world.registry);
    let router = crate::TriggerRouter::new(
        Arc::clone(&store),
        crate::testing::process_work_wiring_for_registry(Arc::clone(&registry)),
    );
    let predecessor = crate::ToolIntents {
        protocol_version: 1,
        intents: vec![crate::ToolIntent::EmitTrigger(crate::EmitTriggerIntent {
            session_id: SessionId::from("session"),
            request: request.clone(),
        })],
    };
    let predecessor_calls = Arc::new(AtomicUsize::new(0));
    let mut predecessor_context = fixed_intent_dispatch_context(
        Arc::clone(&controller),
        &world,
        predecessor,
        Arc::clone(&predecessor_calls),
    )
    .await;
    predecessor_context.trigger_router = Some(router.clone());
    router
        .emit(
            request.clone(),
            &predecessor_context.effect_controller.scoped(),
        )
        .await
        .expect("seed the predecessor caller-key occurrence");
    let first = run_fixed_intent_attempt(&predecessor_context).await;
    assert!(
        matches!(
            first.intent_outcomes.as_slice(),
            [crate::ToolIntentExecutionOutcome::Refused {
                refusal: crate::ToolIntentRefusalReason::UnsupportedProtocolVersion { recorded: 1 },
                ..
            }]
        ),
        "predecessor refusal: {:?}",
        first.intent_outcomes
    );

    let current_calls = Arc::new(AtomicUsize::new(0));
    let mut cold_context = fixed_intent_dispatch_context(
        Arc::clone(&controller),
        &world,
        recorded_trigger_intents(),
        Arc::clone(&current_calls),
    )
    .await;
    cold_context.trigger_router = Some(router);
    let redriven = run_fixed_intent_attempt(&cold_context).await;

    assert_eq!(predecessor_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        current_calls.load(Ordering::SeqCst),
        0,
        "attempt must replay"
    );
    assert_eq!(redriven.intent_outcomes, first.intent_outcomes);
    let occurrences = store
        .list_occurrences(crate::TriggerOccurrenceFilter::default())
        .await
        .expect("read occurrences after cold replay");
    assert_eq!(occurrences.len(), 1, "cold replay adds no occurrence");
    assert_eq!(
        occurrences[0].occurrence_id,
        "trigger:predecessor-caller-key"
    );
    assert_eq!(
        store
            .list_deliveries()
            .await
            .expect("read deliveries after cold replay")
            .len(),
        1,
        "cold replay adds no delivery"
    );
}

/// The at-most-once half: a crash after the occurrence is ingested and its
/// delivery start commits leaves durable state the redrive must not add to. The
/// redrive re-ingests the same idempotency key, replays the same delivery start
/// from the journal, and reports the same bytes — the reservation reads back as
/// `AlreadyReserved` on the second drive, which is exactly the live-state read a
/// recorded outcome may not expose.
#[tokio::test]
async fn crash_after_delivery_start_neither_re_emits_nor_changes_the_recorded_outcome() {
    let (store, subscription, redriven) = Box::pin(crashed_trigger_intent_redrive(
        IntentPausePoint::AfterProcessCommandCommit(1),
        async |store| {
            let occurrences = store
                .list_occurrences(crate::TriggerOccurrenceFilter::default())
                .await
                .expect("read occurrences after the crash");
            assert_eq!(
                occurrences.len(),
                1,
                "the crash lands after the occurrence is ingested"
            );
            assert_eq!(
                store
                    .list_deliveries_by_occurrence_id(&occurrences[0].occurrence_id)
                    .await
                    .expect("read deliveries after the crash")
                    .len(),
                1,
                "the crash lands after the delivery start commits"
            );
        },
    ))
    .await;

    let (result, occurrence_id) = executed_trigger_outcome(&redriven);
    let occurrences = store
        .list_occurrences(crate::TriggerOccurrenceFilter::default())
        .await
        .expect("read occurrences after the redrive");
    assert_eq!(
        occurrences.len(),
        1,
        "a redrive past a committed emission may not ingest a second occurrence"
    );
    assert_eq!(occurrences[0].occurrence_id, occurrence_id);
    let deliveries = store
        .list_deliveries_by_occurrence_id(&occurrence_id)
        .await
        .expect("read the reserved deliveries");
    assert_eq!(
        deliveries.len(),
        1,
        "a redrive may not reserve a second delivery"
    );
    assert_eq!(
        deliveries[0].subscription.subscription_id,
        subscription.subscription_id
    );
    assert_eq!(
        result["deliveries"],
        json!([{
            "occurrence_id": occurrence_id,
            "subscription_id": subscription.subscription_id,
            "process_id": deliveries[0].process_id,
            "outcome": "started",
        }]),
        "the recorded outcome states what every drive did, not which drive reserved first"
    );
}
