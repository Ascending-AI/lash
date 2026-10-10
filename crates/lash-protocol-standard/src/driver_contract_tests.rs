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
        projector: Arc::new(ChatContextProjector),
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::builder("test-model".to_string())
                    .context_window_tokens(128_000)
                    .capability(lash_core::LlmProfileCapability::default())
                    .extra_body(Default::default())
                    .cache_retention(lash_sansio::llm::capability::CacheRetention::Short)
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
        session_id: lash_core::SessionId::from("standard-driver-contract"),
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
                        ("finish".to_string(), lash_core::TurnControls::finish()),
                        (
                            "switch".to_string(),
                            lash_core::TurnControls::switch_agent_frame(),
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
        call_id: call.call_id.clone(),
        provider_call_id: call.provider_call_id.clone(),
        tool_name: call.tool_name.clone(),
        args: call.args.clone(),
        output,
        model_return: lash_core::facade_support::ModelToolReturn {
            tool_name: call.tool_name.clone(),
            parts: vec![lash_core::facade_support::ModelToolReturnPart::text(
                "result",
            )],
            attachment_notices: Vec::new(),
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
        crate::standard_execution_section(BatchSugar::Enabled {
            max_members: std::num::NonZeroUsize::new(8).expect("eight is non-zero"),
        })
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
    let mut machine = machine(Some(4));
    let effects = drain(&mut machine);
    let effects = one_tool_round(
        &mut machine,
        &effects,
        ToolCallOutput::finish(serde_json::json!("terminal value")),
    );
    assert_eq!(turn_outcomes(&effects), Vec::new(), "{effects:?}");
    let (id, kind) = checkpoint(&effects)
        .unwrap_or_else(|| panic!("expected a completion checkpoint, got {effects:?}"));
    assert_eq!(kind, CheckpointKind::BeforeCompletion);
    machine.handle_response(Response::Checkpoint {
        id,
        delivery: sansio::CheckpointDelivery::default(),
    });
    let effects = drain(&mut machine);
    assert_eq!(
        turn_outcomes(&effects),
        vec![TurnOutcome::Finished(TurnFinish::Finished {
            tool_name: "probe".to_string(),
            value: serde_json::json!("terminal value"),
        })],
        "{effects:?}"
    );
    assert!(machine.is_done());
}

/// Law 6 (FIG-5781): input that arrives at BeforeCompletion supersedes the
/// candidate. The turn goes on into a new iteration, the model reads that
/// its finish was superseded, and a later finish ends the turn.
#[test]
fn input_at_before_completion_supersedes_the_finish_and_a_later_finish_ends_the_turn() {
    let mut machine = machine(Some(4));
    let effects = drain(&mut machine);
    let effects = one_tool_round(
        &mut machine,
        &effects,
        ToolCallOutput::finish(serde_json::json!("first")),
    );
    let (id, kind) = checkpoint(&effects).expect("a completion checkpoint");
    assert_eq!(kind, CheckpointKind::BeforeCompletion);
    machine.handle_response(Response::Checkpoint {
        id,
        delivery: sansio::CheckpointDelivery {
            committed_user_messages: vec![Message {
                id: "m1".to_string(),
                role: MessageRole::User,
                parts: vec![Part::text(
                    "m1.p0".to_string(),
                    "one more thing".to_string(),
                    None,
                )]
                .into(),
                origin: None,
                reply_marker: None,
            }],
        },
    });
    let effects = drain(&mut machine);
    assert_eq!(turn_outcomes(&effects), Vec::new(), "{effects:?}");
    assert!(!machine.is_done());
    let candidates = machine.completion_candidates();
    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].disposition,
        lash_core::CompletionDisposition::Superseded
    );
    let rendered = serde_json::to_string(&machine.events().iter().cloned().collect::<Vec<_>>())
        .expect("events encode");
    assert!(rendered.contains("superseded"), "{rendered}");

    // The next model call is the new iteration's: it finishes again.
    let effects = if checkpoint(&effects).is_some() {
        let (id, _) = checkpoint(&effects).expect("checkpoint");
        machine.handle_response(Response::Checkpoint {
            id,
            delivery: sansio::CheckpointDelivery::default(),
        });
        drain(&mut machine)
    } else {
        effects
    };
    let effects = one_tool_round(
        &mut machine,
        &effects,
        ToolCallOutput::finish(serde_json::json!("second")),
    );
    let (id, kind) = checkpoint(&effects).expect("a second completion checkpoint");
    assert_eq!(kind, CheckpointKind::BeforeCompletion);
    machine.handle_response(Response::Checkpoint {
        id,
        delivery: sansio::CheckpointDelivery::default(),
    });
    let effects = drain(&mut machine);
    assert_eq!(
        turn_outcomes(&effects),
        vec![TurnOutcome::Finished(TurnFinish::Finished {
            tool_name: "probe".to_string(),
            value: serde_json::json!("second"),
        })]
    );
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

fn reported_causes(machine: &TurnMachine) -> String {
    serde_json::to_string(&machine.events().iter().cloned().collect::<Vec<_>>())
        .expect("events encode")
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
    let reported = reported_causes(&machine);
    assert!(reported.contains("control_attempt_spent"), "{reported}");
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
    assert!(
        reported.contains("another call of the same step failed"),
        "{reported}"
    );
}

/// Law 8 (FIG-5781): a control call is the step's own call, never a member
/// of `batch`; the member is refused and the batch's other members run.
#[test]
fn a_batch_member_naming_a_control_tool_is_refused() {
    let mut machine = controlling_machine();
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
    let (_, calls, plan) = tool_work(&effects);
    let names: Vec<_> = calls.iter().map(|call| call.tool_name.as_str()).collect();
    assert_eq!(names, ["probe"]);
    let rows = serde_json::to_string(&plan).expect("plan encodes");
    assert!(rows.contains("cannot run inside `batch`"), "{rows}");
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
