//! Red-side anchors for the staged two-phase LLM-call effect boundary
//! (FIG-1276).
//!
//! Phase 1 journals the raw provider completion before any fallible host code
//! runs; phase 2 journals what assistant-response hooks derived from it. Each
//! test here goes red if the phases are folded back into one.

use super::effect::{RecordingEffectController, host_with_effect_recorder, scoped_test_turn};
use super::*;
use lash_sansio::core_support::MessageSequenceCoreSupport;
use lash_sansio::sync::MutexExt;

struct ResponseHookFixture {
    provider_calls: Arc<std::sync::atomic::AtomicUsize>,
    hook_calls: Arc<std::sync::atomic::AtomicUsize>,
    plugin: Arc<dyn lash_core::facade_support::PluginFactory>,
    transport: lash_core::testing::TestProvider,
}

/// A counting provider plus a counting assistant-response hook.
///
/// `hook_failures` failing invocations come first; `hook_events` are emitted by
/// every successful one.
fn response_hook_fixture(hook_failures: usize, hook_events: usize) -> ResponseHookFixture {
    let provider_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hook_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("mock")
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            move |_| {
                let provider_calls = Arc::clone(&provider_calls);
                async move {
                    let call = provider_calls.fetch_add(1, Ordering::SeqCst) + 1;
                    let text = format!("paid completion {call}");
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text,
                            response_meta: None,
                        }],
                        terminal_reason: lash_core::LlmTerminalReason::Stop,
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let plugin: Arc<dyn lash_core::facade_support::PluginFactory> =
        Arc::new(RuntimeTestPluginFactory {
            build: Arc::new({
                let hook_calls = Arc::clone(&hook_calls);
                move |_| {
                    let hook_calls = Arc::clone(&hook_calls);
                    Ok(Arc::new(RuntimeTestPlugin {
                        before_turn: None,
                        checkpoint: None,
                        presentation_steps: vec![],
                        runtime_event: None,
                        external_registrar: Some(Arc::new(move |reg| {
                            let hook_calls = Arc::clone(&hook_calls);
                            reg.output().response(Arc::new(move |context| {
                                let hook_calls = Arc::clone(&hook_calls);
                                Box::pin(async move {
                                    if hook_calls.fetch_add(1, Ordering::SeqCst) < hook_failures {
                                        return Err(lash_core::PluginError::Invoke(
                                            "injected assistant response hook failure".to_string(),
                                        ));
                                    }
                                    Ok(lash_core::facade_support::AssistantResponseTransform {
                                    response: context.response,
                                    events: (0..hook_events)
                                        .map(|index| lash_core::PluginRuntimeEvent::Custom {
                                            name: format!("derived-{index}"),
                                            payload: serde_json::json!({"from": "response-hook"}),
                                        })
                                        .collect(),
                                })
                                })
                            }));
                            Ok(())
                        })),
                    }))
                }
            }),
        });
    ResponseHookFixture {
        provider_calls,
        hook_calls,
        plugin,
        transport,
    }
}

fn journaled(
    recorder: &RecordingEffectController,
    select: impl Fn(&RuntimeEffectOutcome) -> bool,
) -> Option<RuntimeEffectOutcome> {
    recorder
        .replay_outcomes
        .lock_recover()
        .values()
        .find(|outcome| select(outcome))
        .cloned()
}

fn journaled_raw_completion(recorder: &RecordingEffectController) -> LlmResponse {
    let outcome = journaled(recorder, |outcome| {
        matches!(outcome, RuntimeEffectOutcome::LlmCall { .. })
    })
    .expect("phase 1 journals the provider completion");
    let RuntimeEffectOutcome::LlmCall { result, .. } = outcome else {
        unreachable!("selected the LLM outcome")
    };
    (*result)
        .expect("phase 1's journaled outcome is the raw provider completion, never a hook error")
}

/// One physical drive of the same logical turn.
///
/// Prepared turns pin the turn index, so a second drive addresses the *same*
/// journal entries — which is what a redrive is. `run_turn_assembled` would
/// allocate the next index and silently become a fresh logical turn.
async fn drive_turn(
    runtime: &mut LashRuntime,
    backend: &Arc<dyn lash_core::Backend>,
    recorder: &RecordingEffectController,
    turn_id: &TurnId,
) -> Result<AssembledTurn, RuntimeError> {
    runtime
        .stream_prepared_turn(
            lash_core::facade_support::MessageSequence::from_owned(vec![Message {
                id: format!("{turn_id}-user"),
                role: MessageRole::User,
                parts: vec![Part::text(
                    format!("{turn_id}-user.p0"),
                    "produce a completion".to_string(),
                    None,
                )]
                .into(),
                origin: None,
            }]),
            None,
            None,
            None,
            lash_core::TurnContext::default(),
            Vec::new(),
            TurnId::from(turn_id.to_string()),
            1,
            &NoopEventSink,
            &NoopTurnActivitySink,
            scoped_test_turn(backend, recorder, turn_id),
            CancellationToken::new(),
            None,
            None,
        )
        .await
}

/// Anchor (a): a hook that fails *after* the provider completed.
///
/// Red before the split: the hook error occupied the journal slot the paid
/// response would have, so recovery had nothing to replay and a retry bought a
/// second generation.
#[tokio::test]
async fn failing_hook_leaves_the_paid_completion_journaled_and_redrive_reruns_only_the_hook() {
    let backend = memory_backend().await;
    let fixture = response_hook_fixture(1, 0);
    let recorder = RecordingEffectController::default()
        .with_local_llm_execution()
        .with_replay_by_key();
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![Arc::clone(&fixture.plugin)],
        Arc::new(EmptyTools),
        fixture.transport,
        host_with_effect_recorder(&backend, recorder.clone()),
    )
    .await;

    let failed = drive_turn(
        &mut runtime,
        &backend,
        &recorder,
        &TurnId::from("response-hook-failure"),
    )
    .await
    .expect_err("a failing hook aborts the turn so a redrive re-derives phase 2");

    assert_eq!(fixture.provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.hook_calls.load(Ordering::SeqCst), 1);

    // The host-visible shape of a failed hook, pinned because this change moved
    // it: before the split a hook failure was an `LlmCallError` with
    // `code: "plugin_assistant_response"` and surfaced as
    // `TurnStop::ProviderError`. It is now the retryable phase-2 diagnostic,
    // a live fault on every host (FIG-3575): the turn aborts instead of
    // recording a failure a redrive repairs.
    assert_eq!(
        failed.code,
        RuntimeErrorCode::RuntimeEffectAssistantResponseHook,
        "the abort must name the phase-2 diagnostic: {failed:?}"
    );

    // The decisive assertion: the paid completion — not our post-processing's
    // opinion of it — is what the journal holds.
    assert_eq!(
        journaled_raw_completion(&recorder).full_text(),
        "paid completion 1"
    );
    assert!(
        journaled(&recorder, |outcome| matches!(
            outcome,
            RuntimeEffectOutcome::AssistantResponseHooks { .. }
        ))
        .is_none(),
        "an incomplete derivation must not be sealed into phase 2's entry"
    );

    let redriven = drive_turn(
        &mut runtime,
        &backend,
        &recorder,
        &TurnId::from("response-hook-failure"),
    )
    .await
    .expect("the redrive completes from the recorded completion");

    assert_eq!(
        fixture.provider_calls.load(Ordering::SeqCst),
        1,
        "the redrive must serve the recorded completion, never re-buy one"
    );
    assert_eq!(
        fixture.hook_calls.load(Ordering::SeqCst),
        2,
        "phase 2 re-runs the hook over the replayed completion"
    );
    assert_eq!(redriven.assistant_output.safe_text, "paid completion 1");
}

/// Anchor (b): a crash between phase 1 and phase 2.
///
/// The hook never runs on the first pass; the redrive completes phase 2 from
/// the recorded completion with no provider re-invocation.
#[tokio::test]
async fn crash_between_the_phases_redrives_phase_two_without_reinvoking_the_provider() {
    let backend = memory_backend().await;
    let fixture = response_hook_fixture(0, 0);
    let recorder = RecordingEffectController::default()
        .with_local_llm_execution()
        .with_replay_by_key()
        .with_crash_before_first_response_hooks();
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![Arc::clone(&fixture.plugin)],
        Arc::new(EmptyTools),
        fixture.transport,
        host_with_effect_recorder(&backend, recorder.clone()),
    )
    .await;

    let crashed = drive_turn(
        &mut runtime,
        &backend,
        &recorder,
        &TurnId::from("phase-crash"),
    )
    .await
    .expect_err("the crashed phase aborts the turn: a host crash is a live fault");

    assert_eq!(fixture.provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.hook_calls.load(Ordering::SeqCst),
        0,
        "the host died before post-processing ran"
    );
    assert_eq!(crashed.code, RuntimeErrorCode::RuntimeEffectLocalTaskClosed);
    assert_eq!(
        journaled_raw_completion(&recorder).full_text(),
        "paid completion 1",
        "phase 1 was durable before the crash window opened"
    );

    let redriven = drive_turn(
        &mut runtime,
        &backend,
        &recorder,
        &TurnId::from("phase-crash"),
    )
    .await
    .expect("the redrive completes phase 2 from the recorded completion");

    assert_eq!(
        fixture.provider_calls.load(Ordering::SeqCst),
        1,
        "recovery across the inter-phase window must not re-invoke the provider"
    );
    assert_eq!(fixture.hook_calls.load(Ordering::SeqCst), 1);
    assert_eq!(redriven.assistant_output.safe_text, "paid completion 1");
}

/// Anchor (c): hook-emitted plugin events belong to phase 2's journal entry.
///
/// They are never folded into the provider-completion entry, and a replay of
/// phase 2 serves them from its own record instead of re-running the hook.
#[tokio::test]
async fn hook_emitted_events_belong_to_phase_twos_entry_and_replay_from_it() {
    let backend = memory_backend().await;
    let fixture = response_hook_fixture(0, 1);
    let recorder = RecordingEffectController::default()
        .with_local_llm_execution()
        .with_replay_by_key();
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![Arc::clone(&fixture.plugin)],
        Arc::new(EmptyTools),
        fixture.transport,
        host_with_effect_recorder(&backend, recorder.clone()),
    )
    .await;

    let first = drive_turn(
        &mut runtime,
        &backend,
        &recorder,
        &TurnId::from("hook-events"),
    )
    .await
    .expect("turn completes");
    assert_eq!(first.assistant_output.safe_text, "paid completion 1");

    let phase_one = journaled(&recorder, |outcome| {
        matches!(outcome, RuntimeEffectOutcome::LlmCall { .. })
    })
    .expect("phase 1 entry");
    let phase_one_json = serde_json::to_value(&phase_one).expect("encode phase 1");
    assert!(
        !phase_one_json.to_string().contains("response-hook"),
        "no hook-emitted event may be attributed to the provider-completion entry"
    );

    let phase_two = journaled(&recorder, |outcome| {
        matches!(outcome, RuntimeEffectOutcome::AssistantResponseHooks { .. })
    })
    .expect("phase 2 entry");
    let RuntimeEffectOutcome::AssistantResponseHooks { events, .. } = &phase_two else {
        unreachable!("selected phase 2")
    };
    assert_eq!(events.len(), 1, "the hook's events ride phase 2's entry");
    assert_eq!(events[0].events.len(), 1);
    assert!(matches!(
        &events[0].events[0],
        lash_core::PluginRuntimeEvent::Custom { name, .. } if name == "derived-0"
    ));

    let replayed = drive_turn(
        &mut runtime,
        &backend,
        &recorder,
        &TurnId::from("hook-events"),
    )
    .await
    .expect("replay of both phases");
    assert_eq!(
        fixture.provider_calls.load(Ordering::SeqCst),
        1,
        "phase 1 replays"
    );
    assert_eq!(
        fixture.hook_calls.load(Ordering::SeqCst),
        1,
        "phase 2 replays: the recorded events are served, the hook is not re-run"
    );
    assert_eq!(replayed.assistant_output.safe_text, "paid completion 1");
}

#[tokio::test]
async fn recording_response_hook_terminal_error_replays() {
    for recorder in [
        RecordingEffectController::default().with_replay_by_key(),
        RecordingEffectController::default().with_strict_replay_by_address(),
    ] {
        let backend = memory_backend().await;
        let controller = super::effect::layered_controller(
            &backend,
            Arc::new(recorder.clone()),
            lash_core::AdmittedScope::runtime_operation("recording-terminal"),
        );
        let scope = ExecutionScope::runtime_operation("recording-terminal");
        let envelope = RuntimeEffectEnvelope::new(
            lash_core::RuntimeEffectInvocation::new(
                lash_core::EffectAddress::new(scope, "response-hook").expect("effect address"),
                lash_core::RuntimeAttribution::for_turn("session", "turn", 1, 0),
                "response-hook",
            ),
            RuntimeEffectCommand::AssistantResponseHooks {
                response: Box::default(),
                stream_hook_states: Vec::new(),
            },
        );
        controller
            .execute_effect(
                envelope.clone(),
                lash_core::RuntimeEffectLocalExecutor::testing(|_| async {
                    Err(RuntimeEffectControllerError::new(
                        lash_core::RuntimeErrorCode::RuntimeEffectAssistantResponseHook,
                        "terminal derivation error",
                    ))
                }),
            )
            .await
            .expect_err("first attempt fails");
        let error = controller
            .execute_effect(
                envelope,
                lash_core::RuntimeEffectLocalExecutor::testing(|_| async {
                    panic!("a terminal hook error must replay without executing")
                }),
            )
            .await
            .expect_err("terminal replays");
        assert_eq!(error.message, "terminal derivation error");
    }
}

/// A plugin whose response hook derives the response from what its stream
/// hooks saw. Each built instance has its own memory, as each worker does.
fn stream_state_plugin() -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                presentation_steps: vec![],
                runtime_event: None,
                external_registrar: Some(Arc::new(|reg| {
                    let seen = Arc::new(std::sync::Mutex::new(String::new()));
                    let stream_seen = Arc::clone(&seen);
                    reg.output().stream(Arc::new(move |context| {
                        stream_seen.lock_recover().push_str(&context.chunk);
                        Box::pin(async move {
                            Ok(lash_core::plugin::AssistantStreamTransform {
                                chunk: context.chunk,
                                reasoning_deltas: Vec::new(),
                                events: Vec::new(),
                                abort_stream: false,
                            })
                        })
                    }));
                    let finished_seen = Arc::clone(&seen);
                    reg.output().stream_finished(Arc::new(move |_| {
                        let seen = std::mem::take(&mut *finished_seen.lock_recover());
                        Box::pin(async move { Ok(Some(serde_json::json!({ "seen": seen }))) })
                    }));
                    reg.output().response(Arc::new(move |context| {
                        let seen = context
                            .stream_state
                            .as_ref()
                            .and_then(|state| state["seen"].as_str())
                            .unwrap_or("<nothing>")
                            .to_string();
                        let mut response = context.response;
                        response.parts = vec![LlmOutputPart::Text {
                            text: format!("derived from the stream: {seen}"),
                            response_meta: None,
                        }];
                        Box::pin(async move {
                            Ok(lash_core::facade_support::AssistantResponseTransform {
                                response,
                                events: Vec::new(),
                            })
                        })
                    }));
                    Ok(())
                })),
            }))
        }),
    })
}

/// Anchor (e): phase 2 redriven alone on another worker derives what the
/// streaming worker would have.
///
/// The worker that streamed the completion dies between the phases. A fresh
/// runtime, whose plugin instance never saw the stream, redrives phase 2 from
/// the journal: the stream hooks' end state rides phase 1's recorded outcome
/// and phase 2's command, so the derivation reads it rather than the memory of
/// a worker that is gone.
#[tokio::test]
async fn phase_two_on_another_worker_derives_from_the_journaled_stream_state() {
    let backend = memory_backend().await;
    let provider_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let transport = || {
        let provider_calls = Arc::clone(&provider_calls);
        TestProvider::builder()
            .kind("mock")
            .requires_streaming(true)
            .complete(move |request| {
                provider_calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if let Some(stream) = request.stream_events.as_ref() {
                        for text in ["alpha ", "beta"] {
                            stream.send(LlmStreamEvent::Delta {
                                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                                text: text.to_string(),
                            });
                        }
                    }
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "alpha beta".to_string(),
                            response_meta: None,
                        }],
                        terminal_reason: lash_core::LlmTerminalReason::Stop,
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            })
            .build()
    };
    let recorder = RecordingEffectController::default()
        .with_local_llm_execution()
        .with_replay_by_key()
        .with_crash_before_first_response_hooks();
    let turn_id = TurnId::from("phase-two-elsewhere");

    let mut streaming_worker = runtime_with_plugins_and_tools_and_host(
        vec![stream_state_plugin()],
        Arc::new(EmptyTools),
        transport(),
        host_with_effect_recorder(&backend, recorder.clone()),
    )
    .await;
    drive_turn(&mut streaming_worker, &backend, &recorder, &turn_id)
        .await
        .expect_err("the streaming worker dies between the phases");
    drop(streaming_worker);

    let mut other_worker = runtime_with_plugins_and_tools_and_host(
        vec![stream_state_plugin()],
        Arc::new(EmptyTools),
        transport(),
        host_with_effect_recorder(&backend, recorder.clone()),
    )
    .await;
    let redriven = drive_turn(&mut other_worker, &backend, &recorder, &turn_id)
        .await
        .expect("another worker completes phase 2 from the journal");

    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        1,
        "the completion is served from the journal"
    );
    assert_eq!(
        redriven.assistant_output.safe_text, "derived from the stream: alpha beta",
        "phase 2 reads the recorded stream state, not the redriving worker's memory"
    );
}
