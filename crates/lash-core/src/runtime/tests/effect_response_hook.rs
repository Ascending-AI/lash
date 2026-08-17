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
    plugin: Arc<dyn crate::PluginFactory>,
    transport: crate::testing::TestProvider,
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
                        full_text: text.clone(),
                        parts: vec![LlmOutputPart::Text {
                            text,
                            response_meta: None,
                        }],
                        terminal_reason: crate::LlmTerminalReason::Stop,
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let plugin: Arc<dyn crate::PluginFactory> = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new({
            let hook_calls = Arc::clone(&hook_calls);
            move |_| {
                let hook_calls = Arc::clone(&hook_calls);
                Ok(Arc::new(RuntimeTestPlugin {
                    before_turn: None,
                    checkpoint: None,
                    tool_result_projector: None,
                    runtime_event: None,
                    external_registrar: Some(Arc::new(move |reg| {
                        let hook_calls = Arc::clone(&hook_calls);
                        reg.output().response(Arc::new(move |context| {
                            let hook_calls = Arc::clone(&hook_calls);
                            Box::pin(async move {
                                if hook_calls.fetch_add(1, Ordering::SeqCst) < hook_failures {
                                    return Err(crate::PluginError::Invoke(
                                        "injected assistant response hook failure".to_string(),
                                    ));
                                }
                                Ok(crate::AssistantResponseTransform {
                                    response: context.response,
                                    events: (0..hook_events)
                                        .map(|index| crate::PluginRuntimeEvent::Custom {
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
    recorder: &RecordingEffectController,
    turn_id: &str,
) -> Result<AssembledTurn, RuntimeError> {
    runtime
        .stream_prepared_turn(
            crate::MessageSequence::from_owned(vec![Message {
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
            crate::TurnContext::default(),
            Vec::new(),
            turn_id.to_string(),
            1,
            &NoopEventSink,
            &NoopTurnActivitySink,
            scoped_test_turn(recorder, turn_id),
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
    let fixture = response_hook_fixture(1, 0);
    let recorder = RecordingEffectController::default()
        .with_local_llm_execution()
        .with_replay_by_key();
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![Arc::clone(&fixture.plugin)],
        Arc::new(EmptyTools),
        fixture.transport,
        host_with_effect_recorder(recorder.clone()),
    )
    .await;

    let failed = drive_turn(&mut runtime, &recorder, "response-hook-failure")
        .await
        .expect("a failing hook is an assembled failed turn, not a runtime abort");

    assert_eq!(fixture.provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.hook_calls.load(Ordering::SeqCst), 1);
    assert!(failed.assistant_output.safe_text.is_empty());

    // The host-visible shape of a failed hook, pinned because this change moved
    // it: before the split a hook failure was an `LlmCallError` with
    // `code: "plugin_assistant_response"` and surfaced as
    // `TurnStop::ProviderError`. It is now a runtime-effect-controller failure
    // carrying the retryable phase-2 diagnostic, and under a controller that
    // owns replay it raises a runtime abort instead of assembling at all.
    assert!(
        matches!(failed.outcome, TurnOutcome::Stopped(TurnStop::RuntimeError)),
        "unexpected host-visible outcome: {:?}",
        failed.outcome
    );
    assert!(
        failed.errors.iter().any(|issue| {
            format!("{issue:?}")
                .contains(RuntimeErrorCode::RuntimeEffectAssistantResponseHook.as_str())
        }),
        "the turn must name the phase-2 diagnostic: {:?}",
        failed.errors
    );
    assert!(
        active_conversation_messages(&failed.state)
            .iter()
            .flat_map(|message| message.parts.iter())
            .all(|part| !part.content.contains("paid completion 1")),
        "an underived completion must not survive in committed history"
    );

    // The decisive assertion: the paid completion — not our post-processing's
    // opinion of it — is what the journal holds.
    assert_eq!(
        journaled_raw_completion(&recorder).full_text,
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
    assert_eq!(failed.llm_calls.len(), 1);
    assert_eq!(
        failed.llm_calls[0].attempts[0].outcome,
        crate::AttemptOutcome::Completed
    );

    let redriven = drive_turn(&mut runtime, &recorder, "response-hook-failure")
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
    let fixture = response_hook_fixture(0, 0);
    let recorder = RecordingEffectController::default()
        .with_local_llm_execution()
        .with_replay_by_key()
        .with_crash_before_first_response_hooks();
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![Arc::clone(&fixture.plugin)],
        Arc::new(EmptyTools),
        fixture.transport,
        host_with_effect_recorder(recorder.clone()),
    )
    .await;

    let crashed = drive_turn(&mut runtime, &recorder, "phase-crash")
        .await
        .expect("the crashed phase is an assembled failed turn");

    assert_eq!(fixture.provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.hook_calls.load(Ordering::SeqCst),
        0,
        "the host died before post-processing ran"
    );
    assert!(crashed.assistant_output.safe_text.is_empty());
    assert_eq!(
        journaled_raw_completion(&recorder).full_text,
        "paid completion 1",
        "phase 1 was durable before the crash window opened"
    );

    let redriven = drive_turn(&mut runtime, &recorder, "phase-crash")
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
    let fixture = response_hook_fixture(0, 1);
    let recorder = RecordingEffectController::default()
        .with_local_llm_execution()
        .with_replay_by_key();
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![Arc::clone(&fixture.plugin)],
        Arc::new(EmptyTools),
        fixture.transport,
        host_with_effect_recorder(recorder.clone()),
    )
    .await;

    let first = drive_turn(&mut runtime, &recorder, "hook-events")
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
        crate::PluginRuntimeEvent::Custom { name, .. } if name == "derived-0"
    ));

    let replayed = drive_turn(&mut runtime, &recorder, "hook-events")
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
