//! Assertion floor for the standard driver's plugin identity, its `batch`
//! sugar (expansion, fold and refusal), and the three decisions `handle_llm_success` / `handle_tool_results`
//! make: whether a response carries tool calls, whether a completed call's
//! control is terminal, and where the max-turns budget lands.
//!
//! These drive the sans-io [`TurnMachine`] directly, so every step is bounded by
//! the responses the test hands back. A driver that loses its tool-call
//! predicate stalls a live runtime; here it fails on the next assertion.

use super::*;

use lash_core::sansio::{self, ChatContextProjector, Response};
use lash_core::{
    Effect, Message, MessageRole, Part, ToolCallOutput, ToolControl, ToolFailure, ToolFailureClass,
    ToolValue, TurnMachine, TurnMachineConfig,
};

fn machine_config(max_turns: Option<usize>) -> TurnMachineConfig {
    let protocol_driver: Arc<dyn ProtocolDriverHandle<lash_core::HostTurnProtocol>> =
        Arc::new(StandardDriver::default());
    TurnMachineConfig {
        protocol_driver,
        projector: Arc::new(ChatContextProjector),
        sync_execution_environment: false,
        model: "test-model".to_string(),
        max_context_tokens: None,
        turn_budget: max_turns
            .map(lash_core::TurnBudget::bounded)
            .unwrap_or(lash_core::TurnBudget::Unbounded),
        no_progress_budget: Default::default(),
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        autonomous: false,
        tool_specs: Vec::new().into(),
        system_prompt: Arc::from(""),
        projector_turn_inputs: Default::default(),
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
            parts: vec![Part::text("m0.p0".to_string(), "drive".to_string(), None)].into(),
            origin: None,
        }],
        Arc::new(Vec::new()),
        0,
    )
}

fn drain(machine: &mut TurnMachine) -> Vec<Effect> {
    let mut effects = Vec::new();
    while let Some(effect) = machine.poll_effect() {
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
        tool_name: call.tool_name.clone(),
        args: call.args.clone(),
        output,
        model_return: lash_core::facade_support::ModelToolReturn {
            call_id: call.call_id.clone(),
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

#[test]
fn standard_session_plugin_reports_the_registered_protocol_id() {
    let plugin = StandardProtocolPlugin {
        config: StandardProtocolConfig::default(),
    };

    assert_eq!(SessionPlugin::id(&plugin), STANDARD_PROTOCOL_PLUGIN_ID);
    assert_eq!(SessionPlugin::id(&plugin), "standard_protocol");
    assert_eq!(
        SessionPlugin::id(&plugin),
        StandardProtocolPluginFactory::new().id(),
        "the session plugin and the factory that builds it claim one slot id"
    );
}

fn machine_with(driver: StandardDriver) -> TurnMachine {
    let mut config = machine_config(Some(4));
    config.protocol_driver = Arc::new(driver);
    TurnMachine::new(
        config,
        vec![Message {
            id: "m0".to_string(),
            role: MessageRole::User,
            parts: vec![Part::text("m0.p0".to_string(), "drive".to_string(), None)].into(),
            origin: None,
        }],
        Arc::new(Vec::new()),
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

fn native_call(call_id: &str, tool_name: &str) -> LlmOutputPart {
    LlmOutputPart::ToolCall {
        call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        input_json: "{}".to_string(),
        replay: None,
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

fn reported_tool_calls(effects: &[Effect]) -> Vec<(Option<String>, String)> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Emit(SessionStreamEvent::ToolCall { call_id, name, .. }) => {
                Some((call_id.clone(), name.clone()))
            }
            _ => None,
        })
        .collect()
}

fn appended_tool_results(machine: &TurnMachine) -> Vec<(String, String)> {
    machine
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| part.kind() == PartKind::ToolResult)
        .map(|part| {
            (
                part.tool_call_id()
                    .expect("a result names its call")
                    .to_string(),
                part.tool_name()
                    .expect("a result names its tool")
                    .to_string(),
            )
        })
        .collect()
}

#[test]
fn batch_folds_to_one_transcript_call() {
    let mut machine = machine_with(StandardDriver::default());
    let effects = drain(&mut machine);
    let effects = respond(
        &mut machine,
        &effects,
        vec![
            native_call("native", "probe"),
            batch_call(
                "wrapper",
                serde_json::json!([
                    { "tool": "probe", "parameters": {} },
                    { "tool": "batch", "parameters": { "tool_calls": [] } },
                    { "tool": "probe", "parameters": { "value": "second" } },
                ]),
            ),
        ],
    );
    let (id, calls, expansion) = tool_work(&effects);
    assert_eq!(
        calls
            .iter()
            .map(|call| call.call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["native", "wrapper/batch/0", "wrapper/batch/2"],
        "the wrapper's admitted members join the native call in one tool work"
    );
    assert_eq!(expansion.wrappers.len(), 1);

    machine.handle_response(Response::ToolResults {
        id,
        results: calls
            .iter()
            .map(|call| completed_call(call, ToolCallOutput::success(serde_json::json!("ok"))))
            .collect(),
    });
    let effects = drain(&mut machine);
    assert_eq!(
        reported_tool_calls(&effects),
        vec![
            (Some("native".to_string()), "probe".to_string()),
            (Some("wrapper".to_string()), "batch".to_string()),
        ],
        "the stream shows one call per wrapper and no member call"
    );
    assert_eq!(
        appended_tool_results(&machine),
        vec![
            ("native".to_string(), "probe".to_string()),
            ("wrapper".to_string(), "batch".to_string()),
        ],
        "the transcript answers the provider's call id once per wrapper"
    );
    let assistant_calls = machine
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| part.kind() == PartKind::ToolCall)
        .map(|part| {
            (
                part.tool_call_id().expect("a call has an id").to_string(),
                part.tool_replay().cloned(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        assistant_calls,
        vec![
            ("native".to_string(), None),
            (
                "wrapper".to_string(),
                Some(ProviderReplayMeta {
                    item_id: Some("provider-wrapper".to_string()),
                    ..ProviderReplayMeta::default()
                })
            ),
        ],
        "the assistant turn keeps the wrapper call with its replay metadata"
    );
}

#[test]
fn batch_all_refused_opens_no_group() {
    let mut machine = machine_with(StandardDriver::default());
    let effects = drain(&mut machine);
    let effects = respond(
        &mut machine,
        &effects,
        vec![batch_call(
            "wrapper",
            serde_json::json!([{ "tool": "batch", "parameters": {} }]),
        )],
    );
    let (id, calls, expansion) = tool_work(&effects);
    assert!(
        calls.is_empty(),
        "no slot remains, so the host opens no group"
    );
    assert_eq!(expansion.wrappers.len(), 1);
    machine.handle_response(Response::ToolResults {
        id,
        results: Vec::new(),
    });
    let effects = drain(&mut machine);
    assert_eq!(
        reported_tool_calls(&effects),
        vec![(Some("wrapper".to_string()), "batch".to_string())],
        "the fully refused wrapper still answers its folded rows"
    );
}

#[test]
fn an_oversized_batch_is_refused_whole_and_starts_nothing() {
    let mut machine = machine_with(StandardDriver::default());
    let effects = drain(&mut machine);
    let members = (0..65)
        .map(|_| serde_json::json!({ "tool": "probe", "parameters": {} }))
        .collect::<Vec<_>>();
    let effects = respond(
        &mut machine,
        &effects,
        vec![batch_call("wrapper", serde_json::Value::Array(members))],
    );
    assert!(
        tool_calls(&effects).is_none(),
        "no member starts: {effects:?}"
    );
    let refused = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::ReportToolCalls { completed } => Some(completed.clone()),
            _ => None,
        })
        .expect("the wrapper is reported refused");
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].call_id, "wrapper");
    assert!(!refused[0].output.is_success());
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
            .map(|call| (call.call_id.as_str(), call.tool_name.as_str()))
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
            trigger_events: Default::default(),
            extra_prompt_contributions: Vec::new(),
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
        enabled.tool_specs[0].input_schema.canonical["properties"]["tool_calls"]["maxItems"],
        serde_json::json!(8)
    );
    assert!(enabled.execution_prompt.contains("at most 8 per batch"));
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
    .with_control(ToolControl::Finish {
        value: ToolValue::String("smuggled terminal value".to_string()),
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

#[test]
fn a_successful_tool_outcome_control_finishes_the_turn() {
    // The precondition for the test above: the same control on a *successful*
    // outcome is the one the driver is supposed to take.
    let mut machine = machine(Some(4));
    let effects = drain(&mut machine);
    let succeeded_with_control =
        ToolCallOutput::success(serde_json::json!("ok")).with_control(ToolControl::Finish {
            value: ToolValue::String("terminal value".to_string()),
        });
    assert!(succeeded_with_control.is_success());

    let effects = one_tool_round(&mut machine, &effects, succeeded_with_control);

    assert_eq!(
        turn_outcomes(&effects),
        vec![TurnOutcome::Finished(TurnFinish::ToolValue {
            tool_name: "probe".to_string(),
            value: serde_json::json!("terminal value"),
        })],
        "a successful outcome's control is the turn's finish value: {effects:?}"
    );
    assert!(machine.is_done());
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
