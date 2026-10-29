//! Assertion floor for the standard driver's plugin identity, its `batch`
//! sugar (expansion, fold and refusal), and the three decisions `handle_llm_success` / `handle_tool_results`
//! make: whether a response carries tool calls, whether a completed call's
//! control is terminal, and where the max-turns budget lands.
//!
//! These shift the sans-io [`TurnMachine`] directly, so every step is bounded by
//! the responses the test hands back. A driver that loses its tool-call
//! predicate stalls a live runtime; here it fails on the next assertion.

use super::*;

use lash_core::sansio::{self, ChatContextProjector, Response};
use lash_core::{
    Effect, Message, MessageRole, Part, ToolCallOutput, ToolControl, ToolFailure, ToolFailureClass,
    ToolValue, TurnControl, TurnMachine, TurnMachineConfig,
};

fn machine_config(max_turns: Option<usize>) -> TurnMachineConfig {
    let protocol_driver: Arc<dyn ProtocolDriverHandle<lash_core::HostTurnProtocol>> =
        Arc::new(StandardDriver::default());
    TurnMachineConfig {
        model_tool_calls: lash_core::sansio::ModelToolCalls::fixture(),
        protocol_driver,
        projector: Arc::new(ChatContextProjector::default()),
        model: lash_protocol_types::llm_profile::LlmProfileConfig::new(
            lash_protocol_types::llm_profile::RecordedLlmProfile::mint(
                lash_protocol_types::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_protocol_types::llm_profile::LlmProfileMetadata::builder(
                    "test-model".to_string(),
                )
                .context_window_tokens(128_000)
                .capability(lash_core::LlmProfileCapability::default())
                .extra_body(Default::default())
                .cache_retention(lash_protocol_types::llm::capability::CacheRetention::Short)
                .build()
                .expect("valid profile"),
            ),
        )
        .with_reasoning(Default::default()),
        turn_budget: max_turns
            .map(lash_core::TurnBudget::bounded)
            .unwrap_or(lash_core::TurnBudget::Unbounded),
        no_progress_budget: lash_core::NoProgressBudget::bounded(12),
        attachment_acceptance: Default::default(),
        generation: lash_core::GenerationOptions::default(),
        session_id: lash_kernel_value::identity::SessionId::from("standard-driver-contract"),
        agent_frame_id: "standard-frame".to_string(),
        turn_id: TurnId::from("standard-driver-turn"),
        emit_llm_trace: false,
        writer_formats: lash_core::build_newest_writer_formats(),
        termination: lash_core::ProtocolTurnOptions::empty(),
    }
}

fn machine(max_turns: Option<usize>) -> TurnMachine {
    TurnMachine::new(
        machine_config(max_turns),
        vec![Message {
            id: "m0".to_string(),
            role: MessageRole::User,
            parts: vec![Part::text("m0.p0".to_string(), "shift".to_string(), None)].into(),
            origin: None,
            reply_marker: None,
        }],
        Default::default(),
        0,
    )
}

/// Every ready effect, with each execution-environment sync answered on the
/// way by an environment whose `finish` and `switch` end the turn.
fn drain(machine: &mut TurnMachine) -> Vec<Effect> {
    let mut effects = Vec::new();
    while let Some(effect) = machine.poll_effect() {
        if let Effect::SyncExecutionEnvironment { id } = effect {
            machine.handle_response(sansio::Response::ExecutionEnvironmentSynced {
                id,
                result: Ok(sansio::ExecutionEnvironmentSync {
                    turn_controls: [
                        (
                            "finish".to_string(),
                            lash_kernel_value::tool_declaration::TurnControls::finish(
                                lash_kernel_value::json_schema::JsonSchema::any(),
                            ),
                        ),
                        (
                            "switch".to_string(),
                            lash_kernel_value::tool_declaration::TurnControls::switch_agent_frame(),
                        ),
                    ]
                    .into(),
                    ..sansio::ExecutionEnvironmentSync::default()
                }),
            });
            continue;
        }
        effects.push(effect);
    }
    effects
}

fn llm_call_id(effects: &[Effect]) -> sansio::EffectId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::LlmCall { id, .. } => Some(*id),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a pending LLM call, got {effects:?}"))
}

fn tool_calls(effects: &[Effect]) -> Option<(sansio::EffectId, Vec<sansio::PendingToolCall>)> {
    effects.iter().find_map(|effect| match effect {
        Effect::ToolCalls { id, calls, .. } => Some((*id, calls.clone())),
        _ => None,
    })
}

fn checkpoint(effects: &[Effect]) -> Option<(sansio::EffectId, CheckpointKind)> {
    effects.iter().find_map(|effect| match effect {
        Effect::Checkpoint { id, checkpoint } => Some((*id, *checkpoint)),
        _ => None,
    })
}

fn turn_outcomes(effects: &[Effect]) -> Vec<TurnOutcome> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Emit(SessionStreamEvent::TurnOutcome { outcome }) => Some(outcome.clone()),
            _ => None,
        })
        .collect()
}

fn tool_call_response(call_id: &str, tool_name: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::ToolCall {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            input_json: "{}".to_string(),
            replay: None,
        }],
        ..LlmResponse::default()
    }
}

fn completed_call(
    call: &sansio::PendingToolCall,
    output: ToolCallOutput,
) -> sansio::CompletedToolCall {
    sansio::CompletedToolCall {
        control_origin: sansio::ToolControlOrigin::OwnCall,
        call_id: call.call_id.clone(),
        provider_call_id: call.provider_call_id.clone(),
        tool_name: call.tool_name.clone(),
        args: call.args.clone(),
        output: output.clone(),
        model_return: lash_core::facade_support::ModelToolReturn {
            outcome: lash_protocol_types::ModelToolOutcome::from(&output.outcome),
            tool_name: call.tool_name.clone(),
            parts: vec![lash_core::facade_support::ModelToolReturnPart::text(
                "result",
            )],
        },
        intent_outcomes: Vec::new(),
        replay: None,
    }
}

/// Answer the pending LLM call, then answer the tool calls it produced with
/// `output`, returning the effects drained after the tool results.
fn one_tool_round(
    machine: &mut TurnMachine,
    effects: &[Effect],
    output: ToolCallOutput,
) -> Vec<Effect> {
    let llm_id = llm_call_id(effects);
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(tool_call_response("call-1", "probe")),
    });
    let effects = drain(machine);
    let (tool_id, calls) = tool_calls(&effects)
        .unwrap_or_else(|| panic!("expected dispatched tool calls, got {effects:?}"));
    assert_eq!(calls.len(), 1, "one tool call was dispatched");
    machine.handle_response(Response::ToolResults {
        id: tool_id,
        results: calls
            .iter()
            .map(|call| completed_call(call, output.clone()))
            .collect(),
    });
    drain(machine)
}

fn machine_with(driver: StandardDriver) -> TurnMachine {
    let mut config = machine_config(Some(4));
    config.protocol_driver = Arc::new(driver);
    TurnMachine::new(
        config,
        vec![Message {
            id: "m0".to_string(),
            role: MessageRole::User,
            parts: vec![Part::text("m0.p0".to_string(), "shift".to_string(), None)].into(),
            origin: None,
            reply_marker: None,
        }],
        Default::default(),
        0,
    )
}

fn batch_call(call_id: &str, members: serde_json::Value) -> LlmOutputPart {
    LlmOutputPart::ToolCall {
        call_id: call_id.to_string(),
        tool_name: "batch".to_string(),
        input_json: serde_json::json!({ "tool_calls": members }).to_string(),
        replay: Some(ProviderReplayMeta {
            item_id: Some(format!("provider-{call_id}")),
            ..ProviderReplayMeta::default()
        }),
    }
}

/// Answer the pending LLM call with `parts`, returning the effects drained
/// after it.
fn respond(
    machine: &mut TurnMachine,
    effects: &[Effect],
    parts: Vec<LlmOutputPart>,
) -> Vec<Effect> {
    machine.handle_response(Response::LlmComplete {
        id: llm_call_id(effects),
        text_streamed: false,
        result: Ok(LlmResponse {
            parts,
            ..LlmResponse::default()
        }),
    });
    drain(machine)
}

fn tool_work(
    effects: &[Effect],
) -> (
    sansio::EffectId,
    Vec<sansio::PendingToolCall>,
    sansio::ToolExpansionPlan,
) {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::ToolCalls {
                id,
                calls,
                expansion,
            } => Some((*id, calls.clone(), expansion.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected the step's tool work, got {effects:?}"))
}

#[test]
fn a_disabled_batch_is_an_ordinary_tool_call() {
    let mut machine = machine_with(StandardDriver {
        discovery: false,
        batch: BatchSugar::Disabled,
    });
    let effects = drain(&mut machine);
    let effects = respond(
        &mut machine,
        &effects,
        vec![batch_call(
            "wrapper",
            serde_json::json!([{ "tool": "probe", "parameters": {} }]),
        )],
    );
    let (_, calls, expansion) = tool_work(&effects);
    assert!(expansion.is_empty());
    assert_eq!(
        calls
            .iter()
            .map(|call| {
                (
                    call.provider_call_id.as_deref().unwrap_or_default(),
                    call.tool_name.as_str(),
                )
            })
            .collect::<Vec<_>>(),
        vec![("wrapper", "batch")],
        "the call goes to the host as named, where preparation refuses an unknown tool"
    );
}

#[test]
fn the_preamble_offers_batch_only_when_enabled() {
    let catalog = lash_core::ToolCatalog::default();
    let build = |batch: BatchSugar| {
        StandardProtocolDriver {
            config: StandardProtocolConfig::default().batch(batch),
        }
        .build_preamble(ProtocolBuildInput {
            tool_catalog: Arc::new(catalog.clone()),
            plugin_extensions: Default::default(),
            writer_formats: lash_core::build_newest_writer_formats(),
        })
    };
    let enabled = build(BatchSugar::Enabled {
        max_members: std::num::NonZeroUsize::new(8).expect("non-zero"),
    });
    let names = enabled
        .tool_specs
        .iter()
        .map(|spec| spec.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["batch"]);
    assert_eq!(
        enabled.tool_specs[0].input_schema.canonical.as_value()["properties"]["tool_calls"]["maxItems"],
        serde_json::json!(8)
    );
    assert!(
        crate::standard_execution_section(
            BatchSugar::Enabled {
                max_members: std::num::NonZeroUsize::new(8).expect("eight is non-zero"),
            },
            lash_core::TerminationMode::Natural,
            &[],
        )
        .contains("at most 8 per batch")
    );
    assert!(
        !enabled.tool_names.iter().any(|name| name == "batch"),
        "batch is not a catalog entry"
    );
    let disabled = build(BatchSugar::Disabled);
    assert!(disabled.tool_specs.is_empty());
}

#[test]
fn a_failed_tool_outcome_control_is_not_taken_as_the_turn_outcome() {
    // `handle_tool_results` reads the control of *successful* outcomes only. A
    // failed call that still carries a `Finish` control must not end the turn
    // with that value; the driver keeps going and checkpoints after work.
    let mut machine = machine(Some(4));
    let effects = drain(&mut machine);
    let failed_with_control = ToolCallOutput::failure(ToolFailure::tool(
        ToolFailureClass::Execution,
        "probe_failed",
        "probe failed",
    ))
    .with_control(ToolControl::Turn {
        control: TurnControl::Finish {
            value: ToolValue::String("smuggled terminal value".to_string()),
            value_schema: None,
        },
    });
    assert!(!failed_with_control.is_success());

    let effects = one_tool_round(&mut machine, &effects, failed_with_control);

    assert_eq!(
        turn_outcomes(&effects),
        Vec::new(),
        "a failed outcome's control must not finish the turn: {effects:?}"
    );
    assert!(!machine.is_done());
    let (_, kind) = checkpoint(&effects)
        .unwrap_or_else(|| panic!("expected an after-work checkpoint, got {effects:?}"));
    assert_eq!(kind, CheckpointKind::AfterWork);
}

/// A settled control call does not end the turn where it settles: its
/// candidate is decided at BeforeCompletion, and with no input there the
/// turn finishes with the call's value (FIG-5781).
#[test]
fn a_settled_control_call_finishes_the_turn_at_before_completion() {
    for code in [false, true] {
        for (turns, steer) in [(1, false), (1, true), (2, true)] {
            let mut machine = TurnMachine::new(
                loop_config(code, false, turns),
                Vec::new(),
                Default::default(),
                7,
            );
            let ready = drain(&mut machine);
            let output = ToolCallOutput::finish(serde_json::json!("terminal value"));
            let effects = if code {
                let work = respond(
                    &mut machine,
                    &ready,
                    vec![LlmOutputPart::ToolCall {
                        call_id: "cell".into(),
                        tool_name: "execute_code".into(),
                        input_json: serde_json::json!({"code": "await finish('terminal value');"})
                            .to_string(),
                        replay: None,
                    }],
                );
                answer_cell(
                    &mut machine,
                    &work,
                    "finish",
                    output,
                    lash_core::CellOutcome::Controlled {
                        tool_name: "finish".into(),
                        call_id: lash_core::ToolCallId::fixture("demo-call"),
                        control: lash_core::CellControl::Finish {
                            value: serde_json::json!("terminal value").into(),
                        },
                    },
                );
                drain(&mut machine)
            } else {
                one_tool_round(&mut machine, &ready, output)
            };
            assert!(!machine.is_done());
            assert!(turn_outcomes(&effects).is_empty());
            let (id, kind) = checkpoint(&effects).expect("a declared completion checkpoints");
            assert_eq!(kind, CheckpointKind::BeforeCompletion);
            let saved = serde_json::from_slice(&serde_json::to_vec(&machine.checkpoint()).unwrap())
                .unwrap();
            machine =
                TurnMachine::restore_from_checkpoint(loop_config(code, false, turns), saved, None)
                    .unwrap();
            drain(&mut machine);
            machine.handle_response(Response::Checkpoint {
                id,
                delivery: sansio::CheckpointDelivery {
                    committed_user_messages: if steer {
                        vec![Message {
                            id: "steer".into(),
                            role: MessageRole::User,
                            parts: vec![Part::text("steer.p0".into(), "go on".into(), None)].into(),
                            origin: None,
                            reply_marker: None,
                        }]
                    } else {
                        Vec::new()
                    },
                },
            });
            let effects = drain(&mut machine);
            assert_eq!(machine.decided_completions().len(), 1);
            if steer {
                assert_eq!(
                    machine.decided_completions()[0].disposition,
                    lash_core::CompletionDisposition::Superseded
                );
                if turns == 1 {
                    assert_eq!(
                        turn_outcomes(&effects),
                        vec![TurnOutcome::Stopped(TurnStop::MaxTurns)]
                    );
                } else {
                    assert!(!machine.is_done());
                    assert!(turn_outcomes(&effects).is_empty());
                    llm_call_id(&effects);
                }
            } else {
                assert_eq!(
                    turn_outcomes(&effects),
                    vec![TurnOutcome::Finished(TurnFinish::Finished {
                        tool_name: if code { "finish" } else { "probe" }.to_string(),
                        value: serde_json::json!("terminal value"),
                    })],
                    "a successful answer may finish on the last iteration: {effects:?}"
                );
                assert_eq!(
                    machine.decided_completions()[0].disposition,
                    lash_core::CompletionDisposition::Accepted
                );
                assert!(machine.is_done());
            }
        }
    }
}

/// A driver whose synced surface's `finish` and `switch` end the turn
/// ([`drain`]).
fn controlling_machine() -> TurnMachine {
    machine_with(StandardDriver {
        batch: BatchSugar::Enabled {
            max_members: std::num::NonZeroUsize::new(8).expect("eight is non-zero"),
        },
        ..StandardDriver::default()
    })
}

fn call(call_id: &str, tool_name: &str) -> LlmOutputPart {
    LlmOutputPart::ToolCall {
        call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        input_json: "{}".to_string(),
        replay: None,
    }
}

fn answer(
    machine: &mut TurnMachine,
    id: sansio::EffectId,
    calls: &[sansio::PendingToolCall],
    output: impl Fn(&str) -> ToolCallOutput,
) -> Vec<Effect> {
    machine.handle_response(Response::ToolResults {
        id,
        results: calls
            .iter()
            .map(|call| completed_call(call, output(&call.tool_name)))
            .collect(),
    });
    drain(machine)
}

// Completion transfers the records to the consumer and releases the machine's copy.
fn delivered_events(effects: &[Effect]) -> impl Iterator<Item = &lash_core::SessionHistoryRecord> {
    effects.iter().flat_map(|effect| match effect {
        Effect::Progress { event_delta, .. } | Effect::Done { event_delta, .. } => {
            event_delta.as_slice()
        }
        _ => &[],
    })
}

fn reported_causes(machine: &TurnMachine) -> String {
    serde_json::to_string(&machine.events().iter().cloned().collect::<Vec<_>>())
        .expect("events encode")
}

fn reported_failure(effects: &[Effect], code: &str) -> bool {
    effects.iter().any(|effect| {
        matches!(effect,
        Effect::Emit(SessionStreamEvent::ToolCall { output, .. })
            if matches!(&output.outcome, lash_core::ToolCallOutcome::Failure(failure)
                if failure.code == code))
    })
}

/// Law 8 (FIG-5781): a step's one control call runs after its siblings
/// settled, and a second control call of the step is refused on its own
/// while the rest of the step runs.
#[test]
fn a_step_runs_its_control_call_after_its_siblings_and_refuses_a_second() {
    let mut machine = controlling_machine();
    let effects = drain(&mut machine);
    let effects = respond(
        &mut machine,
        &effects,
        vec![
            call("call-1", "finish"),
            call("call-2", "probe"),
            call("call-3", "switch"),
        ],
    );
    assert!(reported_failure(&effects, "control_attempt_spent"));
    let (id, calls) = tool_calls(&effects).expect("the siblings' round");
    let names: Vec<_> = calls.iter().map(|call| call.tool_name.as_str()).collect();
    assert_eq!(names, ["probe"], "the control call waits for its sibling");
    let effects = answer(&mut machine, id, &calls, |_| {
        ToolCallOutput::success(serde_json::json!("ok"))
    });
    let (id, calls) = tool_calls(&effects).expect("the control call's round");
    let names: Vec<_> = calls.iter().map(|call| call.tool_name.as_str()).collect();
    assert_eq!(names, ["finish"]);
    let effects = answer(&mut machine, id, &calls, |_| {
        ToolCallOutput::finish(serde_json::json!("done"))
    });
    let (_, kind) = checkpoint(&effects).expect("a completion checkpoint");
    assert_eq!(kind, CheckpointKind::BeforeCompletion);
}

/// Law 8 (FIG-5781): a sibling that failed refuses the step's control call
/// before its body runs; every result is reported and the turn goes on.
#[test]
fn a_failed_sibling_refuses_the_control_call_before_it_runs() {
    let mut machine = controlling_machine();
    let effects = drain(&mut machine);
    let effects = respond(
        &mut machine,
        &effects,
        vec![call("call-1", "probe"), call("call-2", "finish")],
    );
    let (id, calls) = tool_calls(&effects).expect("the siblings' round");
    assert_eq!(calls.len(), 1);
    let effects = answer(&mut machine, id, &calls, |_| {
        ToolCallOutput::failure(ToolFailure::tool(
            ToolFailureClass::Execution,
            "probe_failed",
            "probe failed",
        ))
    });
    assert!(
        tool_calls(&effects).is_none(),
        "the control call never runs"
    );
    assert_eq!(turn_outcomes(&effects), Vec::new());
    let (_, kind) = checkpoint(&effects).expect("an after-work checkpoint");
    assert_eq!(kind, CheckpointKind::AfterWork);
    let reported = reported_causes(&machine);
    assert_eq!(
        reported.matches("\"kind\":\"ToolResult\"").count(),
        2,
        "both calls are reported: {reported}"
    );
    assert!(reported_failure(&effects, "control_sibling_failed"));
}

/// The tool calls an answered round reported, by name, in slot order.
fn reported_calls(effects: &[Effect]) -> Vec<String> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Emit(SessionStreamEvent::ToolCall { name, .. }) => Some(name.clone()),
            _ => None,
        })
        .collect()
}

/// FIG-5831: a tool wave owns the calls it dispatched. An answer that is not
/// one result per dispatched call, by identity and in slot order, answers
/// nothing; the held control call's result then joins between the slots
/// before and after it.
#[test]
fn a_tool_wave_takes_only_the_answer_to_the_calls_it_dispatched() {
    let mut machine = controlling_machine();
    let effects = drain(&mut machine);
    let effects = respond(
        &mut machine,
        &effects,
        vec![
            call("call-1", "probe"),
            call("call-2", "finish"),
            call("call-3", "lookup"),
        ],
    );
    let (id, calls) = tool_calls(&effects).expect("the siblings' round");
    let names: Vec<_> = calls.iter().map(|call| call.tool_name.as_str()).collect();
    assert_eq!(names, ["probe", "lookup"]);
    let ok = |_: &str| ToolCallOutput::success(serde_json::json!("ok"));
    for wrong in [
        vec![calls[0].clone()],
        vec![calls[1].clone(), calls[0].clone()],
    ] {
        let effects = answer(&mut machine, id, &wrong, ok);
        assert!(effects.is_empty(), "{effects:?}");
    }
    let effects = answer(&mut machine, id, &calls, ok);
    let (id, control) = tool_calls(&effects).expect("the control call's round");
    let names: Vec<_> = control.iter().map(|call| call.tool_name.as_str()).collect();
    assert_eq!(names, ["finish"]);
    let finished = |_: &str| ToolCallOutput::finish(serde_json::json!("done"));
    let doubled = [control[0].clone(), calls[0].clone()];
    let effects = answer(&mut machine, id, &doubled, finished);
    assert!(effects.is_empty(), "{effects:?}");
    let effects = answer(&mut machine, id, &control, finished);
    assert_eq!(reported_calls(&effects), ["probe", "finish", "lookup"]);
    let (_, kind) = checkpoint(&effects).expect("a completion checkpoint");
    assert_eq!(kind, CheckpointKind::BeforeCompletion);
}

fn raw_call(call_id: &str, tool_name: &str, input_json: &str) -> LlmOutputPart {
    LlmOutputPart::ToolCall {
        call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        input_json: input_json.to_string(),
        replay: None,
    }
}

fn failed(_: &str) -> ToolCallOutput {
    ToolCallOutput::failure(ToolFailure::tool(
        ToolFailureClass::Execution,
        "probe_failed",
        "probe failed",
    ))
}

/// The typed refusal of a control call whose sibling failed.
const SIBLING_FAILED: &str = "control_sibling_failed";

/// The step's control call never ran: no wave dispatched it, nothing ended
/// the turn, the turn checkpoints after its work, and `cause` was reported.
fn assert_control_refused(effects: &[Effect], machine: &TurnMachine, cause: &str) {
    assert!(
        tool_calls(effects)
            .is_none_or(|(_, calls)| calls.iter().all(|call| call.tool_name != "finish")),
        "the control call never runs: {effects:?}"
    );
    assert_eq!(turn_outcomes(effects), Vec::new(), "{effects:?}");
    assert!(!machine.is_done());
    assert!(
        reported_failure(effects, cause),
        "{cause} is reported: {effects:?}"
    );
}

/// Law 8 (FIG-5801): a failing member of a `batch` is a failed sibling of
/// the step's control call, though the wrapper folds into one result: the
/// control call is refused before its body runs.
#[test]
fn a_batch_with_a_failing_member_blocks_the_control() {
    let mut machine = controlling_machine();
    let effects = drain(&mut machine);
    let effects = respond(
        &mut machine,
        &effects,
        vec![
            batch_call(
                "call-b",
                serde_json::json!([{ "tool": "probe", "parameters": {} }]),
            ),
            call("call-f", "finish"),
        ],
    );
    let (id, calls) = tool_calls(&effects).expect("the siblings' round");
    let names: Vec<_> = calls.iter().map(|call| call.tool_name.as_str()).collect();
    assert_eq!(names, ["probe"], "the control call waits for the batch");
    let effects = answer(&mut machine, id, &calls, failed);
    assert_control_refused(&effects, &machine, SIBLING_FAILED);
    let (_, kind) = checkpoint(&effects).expect("an after-work checkpoint");
    assert_eq!(kind, CheckpointKind::AfterWork);
}

/// Law 8 (FIG-5801): a sibling refused before dispatch, here for malformed
/// JSON arguments, failed: the control call is refused before its body runs.
#[test]
fn malformed_json_for_a_sibling_blocks_the_control() {
    let mut machine = controlling_machine();
    let effects = drain(&mut machine);
    let effects = respond(
        &mut machine,
        &effects,
        vec![raw_call("call-1", "probe", "{"), call("call-2", "finish")],
    );
    assert_control_refused(&effects, &machine, SIBLING_FAILED);
    assert!(
        reported_failure(&effects, "invalid_tool_call_json"),
        "the malformed sibling is reported"
    );
}

/// Law 8 (FIG-5801): the step's one control attempt is reserved by the
/// first control call in response order, before its arguments are parsed:
/// a malformed first control call spends it, and the next one is refused.
#[test]
fn a_malformed_first_control_spends_the_slot() {
    let mut machine = controlling_machine();
    let effects = drain(&mut machine);
    let effects = respond(
        &mut machine,
        &effects,
        vec![raw_call("call-1", "finish", "{"), call("call-2", "finish")],
    );
    assert_control_refused(&effects, &machine, "control_attempt_spent");
    assert!(
        reported_failure(&effects, "invalid_tool_call_json"),
        "the malformed control call is reported"
    );
}

/// Law 8 (FIG-5801): a control call made as a member of `batch` keeps its
/// member identity: it runs after the batch's other members, the turn's
/// candidate names the member, and the wrapper's result reports both rows.
#[test]
fn a_control_member_inside_a_batch_finishes_with_its_identity() {
    let mut machine = controlling_machine();
    let mut history = Vec::new();
    let effects = drain(&mut machine);
    let effects = respond(
        &mut machine,
        &effects,
        vec![batch_call(
            "call-b",
            serde_json::json!([
                { "tool": "probe", "parameters": {} },
                { "tool": "finish", "parameters": { "value": 1 } }
            ]),
        )],
    );
    history.extend(delivered_events(&effects).cloned());
    let (id, calls, plan) = tool_work(&effects);
    let names: Vec<_> = calls.iter().map(|call| call.tool_name.as_str()).collect();
    assert_eq!(names, ["probe"], "the control member waits for its sibling");
    let wrapper = plan.wrappers[0].call_id.clone();
    let effects = answer(&mut machine, id, &calls, |_| {
        ToolCallOutput::success(serde_json::json!("ok"))
    });
    history.extend(delivered_events(&effects).cloned());
    let (id, calls) = tool_calls(&effects).expect("the control member's round");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool_name, "finish");
    assert_eq!(
        calls[0].call_id,
        wrapper.child(1),
        "the member's own identity"
    );
    let effects = answer(&mut machine, id, &calls, |_| {
        ToolCallOutput::finish(serde_json::json!({ "value": 1 }))
    });
    history.extend(delivered_events(&effects).cloned());
    let (id, kind) = checkpoint(&effects).expect("a completion checkpoint");
    assert_eq!(kind, CheckpointKind::BeforeCompletion);
    machine.handle_response(Response::Checkpoint {
        id,
        delivery: sansio::CheckpointDelivery::default(),
    });
    let effects = drain(&mut machine);
    history.extend(delivered_events(&effects).cloned());
    assert_eq!(
        turn_outcomes(&effects),
        vec![TurnOutcome::Finished(TurnFinish::Finished {
            tool_name: "finish".to_string(),
            value: serde_json::json!({ "value": 1 }),
        })],
        "{effects:?}"
    );
    let decided = machine.decided_completions();
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0].candidate.call_id, wrapper.child(1));
    let reported = serde_json::to_string(&history).expect("delivered events encode");
    assert_eq!(
        reported.matches("\"kind\":\"ToolResult\"").count(),
        1,
        "the wrapper answers once for its members: {reported}"
    );
    assert!(
        reported.contains("\\\"tool\\\":\\\"finish\\\""),
        "the control member has its row: {reported}"
    );
}

/// The namespace of a session whose turns only a control call ends.
fn terminal_required() -> lash_core::ProtocolTurnOptions {
    lash_core::ProtocolTurnOptions::from_payload(serde_json::json!({
        "behaviour": StandardProtocolConfig::default()
            .recorded_behaviour()
            .expect("the standard tool presentation admits"),
        "termination": "terminal_required",
    }))
}

/// Law 8 (FIG-5801): under TerminalRequired, a reply in prose does not end
/// the turn: the reply is kept, the model is told to end the turn through a
/// control call, and the turn goes on to its next iteration.
#[test]
fn terminal_required_standard_prose_takes_the_repair_path() {
    let mut config = machine_config(Some(4));
    config.termination = terminal_required();
    let mut machine = TurnMachine::new(
        config,
        vec![Message {
            id: "m0".to_string(),
            role: MessageRole::User,
            parts: vec![Part::text("m0.p0".to_string(), "shift".to_string(), None)].into(),
            origin: None,
            reply_marker: None,
        }],
        Default::default(),
        0,
    );
    let effects = drain(&mut machine);
    let effects = respond(
        &mut machine,
        &effects,
        vec![LlmOutputPart::Text {
            text: "the answer is 4".to_string(),
            response_meta: None,
        }],
    );
    assert_eq!(turn_outcomes(&effects), Vec::new(), "{effects:?}");
    assert!(!machine.is_done());
    let (_, kind) = checkpoint(&effects).expect("an after-work checkpoint");
    assert_eq!(kind, CheckpointKind::AfterWork);
    assert_eq!(machine.protocol_iteration(), 1, "the turn goes on");
    let reported = reported_causes(&machine);
    assert!(reported.contains("the answer is 4"), "{reported}");
    assert!(
        reported.contains("`finish`"),
        "the repair names the control: {reported}"
    );
}

#[test]
fn the_max_turns_budget_is_the_run_offset_plus_the_budget() {
    // After the first tool round the next protocol iteration is 1, and `0 + 2` leaves room for
    // it while `0 * 2` does not.
    let mut machine = machine(Some(2));
    let effects = drain(&mut machine);

    let effects = one_tool_round(
        &mut machine,
        &effects,
        ToolCallOutput::success(serde_json::json!("ok")),
    );
    assert_eq!(
        turn_outcomes(&effects),
        Vec::new(),
        "protocol iteration 1 is inside a budget of 2 turns from offset 0: {effects:?}"
    );
    assert!(!machine.is_done());
    let (checkpoint_id, kind) = checkpoint(&effects)
        .unwrap_or_else(|| panic!("expected an after-work checkpoint, got {effects:?}"));
    assert_eq!(kind, CheckpointKind::AfterWork);

    machine.handle_response(Response::Checkpoint {
        id: checkpoint_id,
        delivery: sansio::CheckpointDelivery::default(),
    });
    let effects = drain(&mut machine);

    // Second round: the next protocol iteration is 2, which the budget refuses.
    let effects = one_tool_round(
        &mut machine,
        &effects,
        ToolCallOutput::success(serde_json::json!("ok")),
    );
    assert_eq!(
        turn_outcomes(&effects),
        vec![TurnOutcome::Stopped(TurnStop::MaxTurns)],
        "protocol iteration 2 exhausts a budget of 2 turns from offset 0: {effects:?}"
    );
    assert!(machine.is_done());
}

fn loop_config(code: bool, batch: bool, turns: usize) -> TurnMachineConfig {
    let mut config = machine_config(Some(turns));
    config.no_progress_budget = lash_core::NoProgressBudget::bounded(2);
    if batch {
        config.protocol_driver = Arc::new(StandardDriver {
            batch: BatchSugar::Enabled {
                max_members: std::num::NonZeroUsize::new(8).unwrap(),
            },
            ..StandardDriver::default()
        });
    }
    if code {
        let factory = lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .instruction_limit(lash::rlm::InstructionBound::instructions(1000))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(1))
                .build(),
            lash::rlm::CellDialect::typescript(),
        );
        let host = lash_core::facade_support::PluginHost::new(
            vec![Arc::new(factory)],
            lash_core::ExecutionBudgets::recommended(),
            lash_core::trace::TraceRuntime::new(Arc::new(lash_core::facade_support::SystemClock)),
        );
        let session = host
            .build_session(lash_core::plugin::PluginSessionRequest::creation(
                "loop-law",
                lash_core::plugin::SessionAuthorityContext::fixture(),
            ))
            .unwrap();
        let preamble = session
            .protocol_driver()
            .build_preamble(lash_core::ProtocolBuildInput {
                tool_catalog: lash_core::testing::environment_projection(&session)
                    .unwrap()
                    .catalog,
                plugin_extensions: Default::default(),
                writer_formats: lash_core::build_newest_writer_formats(),
            });
        config.protocol_driver = preamble.config.protocol;
        config.projector = preamble.config.projector;
    }
    config
}

fn demo_round(
    machine: &mut TurnMachine,
    effects: &[Effect],
    code: bool,
    productive: bool,
    batch: bool,
    caught: bool,
) -> Vec<Effect> {
    let operation = if productive && !caught {
        "demo.ok"
    } else {
        "demo.fail"
    };
    let source = if caught && productive {
        "try { await demo.fail({message: 'boom'}); } catch (_) {}".to_string()
    } else {
        format!("await {operation}({{message: 'boom'}});")
    };
    machine.handle_response(Response::LlmComplete {
        id: llm_call_id(effects),
        text_streamed: false,
        result: Ok(if code {
            LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "cell".into(),
                    tool_name: "execute_code".into(),
                    input_json: serde_json::json!({"code": source}).to_string(),
                    replay: None,
                }],
                ..Default::default()
            }
        } else if batch {
            LlmResponse {
                parts: vec![batch_call(
                    "batch-fail",
                    serde_json::json!([
                        {"tool": operation.replace('.', "__"), "parameters": {"message": "boom"}},
                        {"tool": "demo__fail", "parameters": {"message": "boom"}}
                    ]),
                )],
                ..Default::default()
            }
        } else {
            tool_call_response("unit", &operation.replace('.', "__"))
        }),
    });
    let effects = drain(machine);
    let output = if productive && !caught {
        ToolCallOutput::success(serde_json::json!("ok"))
    } else {
        ToolCallOutput::failure(ToolFailure::tool(
            ToolFailureClass::Execution,
            "demo_failure",
            "boom",
        ))
    };
    if code {
        answer_cell(
            machine,
            &effects,
            operation,
            output,
            if productive {
                lash_core::CellOutcome::Completed
            } else {
                lash_core::CellOutcome::Failed(lash_core::CellFailure::new(
                    lash_core::CellFailureKind::Program,
                    "boom",
                ))
            },
        );
    } else {
        let (id, calls) = tool_calls(&effects).expect("demo.fail dispatch");
        machine.handle_response(Response::ToolResults {
            id,
            results: calls
                .iter()
                .enumerate()
                .map(|(index, call)| {
                    completed_call(
                        call,
                        if batch && index > 0 {
                            failed("demo.fail")
                        } else {
                            output.clone()
                        },
                    )
                })
                .collect(),
        });
    }
    drain(machine)
}

fn answer_cell(
    machine: &mut TurnMachine,
    effects: &[Effect],
    tool: &str,
    output: ToolCallOutput,
    result: lash_core::CellOutcome,
) {
    let id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::ExecCode { id, .. } => Some(*id),
            _ => None,
        })
        .expect("cell");
    machine.handle_response(Response::ExecResult {
        id,
        result: Ok(lash_core::ExecResponse {
            consumption: Vec::new(),
            prints: Vec::new(),
            prints_retained: None,
            calls: Vec::new(),
            tool_calls: vec![lash_core::ToolCallRecord {
                call_id: lash_core::ToolCallId::fixture("demo-call"),
                provider_call_id: None,
                tool: tool.into(),
                args: serde_json::json!({"message": "boom"}),
                output,
            }],
            printed_images: Vec::new(),
            result,
            retained_finish_value: None,
            degraded_bindings: Vec::new(),
            bindings: Default::default(),
            suspended: false,
        }),
    });
}

/// FIG-7549: both drivers spend one persistent meter per unproductive iteration,
/// reset it on productive execution, and prefer the silent turn limit.
#[test]
fn both_protocols_share_the_persistent_no_progress_meter() {
    for (code, batch, caught) in [
        (false, false, false),
        (false, true, false),
        (true, false, false),
        (true, false, true),
    ] {
        for (units, turns, expected) in [
            (vec![false, false], 100, 2),
            (vec![false, true, false, false], 100, 4),
            (vec![false, false], 2, 2),
        ] {
            for restore in [false, true] {
                let mut machine = TurnMachine::new(
                    loop_config(code, batch, turns),
                    Vec::new(),
                    Default::default(),
                    7,
                );
                let mut effects = drain(&mut machine);
                let mut calls = 0;
                for productive in &units {
                    effects = demo_round(&mut machine, &effects, code, *productive, batch, caught);
                    calls += 1;
                    if machine.is_done() {
                        break;
                    }
                    let (id, kind) = checkpoint(&effects).expect("continuation admitted");
                    assert_eq!(kind, CheckpointKind::AfterWork);
                    if restore {
                        let saved = serde_json::from_slice(
                            &serde_json::to_vec(&machine.checkpoint()).unwrap(),
                        )
                        .unwrap();
                        machine = TurnMachine::restore_from_checkpoint(
                            loop_config(code, batch, turns),
                            saved,
                            None,
                        )
                        .unwrap();
                        drain(&mut machine);
                    }
                    machine.handle_response(Response::Checkpoint {
                        id,
                        delivery: sansio::CheckpointDelivery {
                            committed_user_messages: if restore {
                                vec![Message {
                                    id: format!("steer-{calls}"),
                                    role: MessageRole::User,
                                    parts: vec![Part::text(
                                        format!("steer-{calls}.p0"),
                                        "keep going".into(),
                                        None,
                                    )]
                                    .into(),
                                    origin: None,
                                    reply_marker: None,
                                }]
                            } else {
                                Vec::new()
                            },
                        },
                    });
                    effects = drain(&mut machine);
                }
                assert!(
                    machine.is_done(),
                    "code={code} restore={restore} calls={calls}"
                );
                assert_eq!(calls, expected);
                assert_eq!(
                    turn_outcomes(&effects),
                    vec![TurnOutcome::Stopped(TurnStop::MaxTurns)]
                );
                let mut fresh = TurnMachine::new(
                    loop_config(code, batch, 100),
                    Vec::new(),
                    machine.events(),
                    17,
                );
                let ready = drain(&mut fresh);
                let _ = demo_round(&mut fresh, &ready, code, false, batch, caught);
                assert!(!fresh.is_done(), "a fresh turn has a fresh meter");
                if turns == 2 {
                    assert!(
                        !serde_json::to_string(&machine.events())
                            .unwrap()
                            .contains("stop_no_progress"),
                        "turn limit stays silent"
                    );
                }
            }
        }
    }
}
