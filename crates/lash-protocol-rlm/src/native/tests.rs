use lash_core::plugin::PluginSessionRequest;
use lash_core::sansio::Response;
use lash_core::{Effect, LlmOutputPart, LlmResponse, TurnMachine, TurnMachineConfig};
use lash_rlm_types::{RlmCreateExtras, RlmProtocolEvent};
use lash_vocabulary::identity::SessionId;
use lash_vocabulary::identity::TurnId;
use std::collections::VecDeque;
use std::sync::Arc;

/// How a test's turns may end.
#[derive(Clone)]
pub(super) struct Ending {
    mode: lash_core::TerminationMode,
}

impl From<lash_core::TerminationMode> for Ending {
    fn from(mode: lash_core::TerminationMode) -> Self {
        Self { mode }
    }
}

pub(super) fn config(ending: impl Into<Ending>) -> TurnMachineConfig {
    config_with_dialect(ending.into(), crate::dialect::CellDialect::typescript())
}

pub(super) fn config_with_dialect(
    ending: Ending,
    dialect: crate::dialect::CellDialect,
) -> TurnMachineConfig {
    let factory = crate::RlmProtocolPluginFactory::new(
        crate::RlmProtocolPluginConfig::builder()
            .instruction_limit(crate::InstructionBound::instructions(1000))
            .memory_limit(crate::MemoryBound::mebibytes(1))
            .build(),
        dialect,
    );
    let host = lash_core::facade_support::PluginHost::new(
        vec![Arc::new(factory)],
        lash_core::ExecutionBudgets::recommended(),
        lash_core::trace::TraceRuntime::new(std::sync::Arc::new(
            lash_core::facade_support::SystemClock,
        )),
    );
    let session = host
        .build_session(PluginSessionRequest::creation(
            "parity",
            lash_core::plugin::SessionAuthorityContext::fixture(),
        ))
        .unwrap();
    let preamble = session
        .protocol_driver()
        .build_preamble(lash_core::ProtocolBuildInput {
            tool_catalog: lash_core::testing::environment_projection(&session)
                .map(|projection| projection.catalog)
                .unwrap(),
            plugin_extensions: Default::default(),
            writer_formats: lash_core::build_newest_writer_formats(),
        });
    TurnMachineConfig {
        model_tool_calls: lash_core::sansio::ModelToolCalls::fixture(),
        protocol_driver: preamble.config.protocol,
        projector: preamble.config.projector,
        model: lash_protocol_types::llm_profile::LlmProfileConfig::new(
            lash_protocol_types::llm_profile::RecordedLlmProfile::mint(
                lash_protocol_types::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_protocol_types::llm_profile::LlmProfileMetadata::builder(
                    "scripted".to_string(),
                )
                .context_window_tokens(128_000)
                .capability(Default::default())
                .extra_body(Default::default())
                .cache_retention(lash_protocol_types::llm::capability::CacheRetention::Short)
                .build()
                .expect("valid profile"),
            ),
        )
        .with_reasoning(Default::default()),
        turn_budget: lash_core::TurnBudget::bounded(4),
        no_progress_budget: lash_core::NoProgressBudget::bounded(3),
        attachment_acceptance: Default::default(),
        generation: Default::default(),
        session_id: SessionId::from("parity"),
        agent_frame_id: "parity-frame".to_string(),
        turn_id: TurnId::from("parity-turn"),
        emit_llm_trace: false,
        writer_formats: lash_core::build_newest_writer_formats(),
        termination: crate::plugin::RlmRecordedConfig::for_testing(RlmCreateExtras {
            finish_value_schema: None,
            termination: Some(ending.mode),
            render: None,
        }),
    }
}

pub(super) fn text(text: &str) -> LlmOutputPart {
    LlmOutputPart::Text {
        text: text.to_string(),
        response_meta: None,
    }
}

pub(super) fn call(id: &str, name: &str, args: &str) -> LlmOutputPart {
    LlmOutputPart::ToolCall {
        call_id: id.to_string(),
        tool_name: name.to_string(),
        input_json: args.to_string(),
        replay: Some(lash_core::llm::types::ProviderReplayMeta {
            item_id: Some("opaque-item".to_string()),
            opaque: Some("signature".to_string()),
            origin: None,
        }),
    }
}
/// Every ready effect, with each execution-environment sync answered by an
/// empty environment on the way.
pub(super) fn drain(machine: &mut TurnMachine) -> Vec<Effect> {
    let mut effects = Vec::new();
    while let Some(effect) = machine.poll_effect() {
        if let Effect::SyncExecutionEnvironment { id } = effect {
            machine.handle_response(lash_core::sansio::Response::ExecutionEnvironmentSynced {
                id,
                result: Ok(lash_core::sansio::ExecutionEnvironmentSync {
                    turn_controls: [(
                        crate::FINISH_TOOL_NAME.to_string(),
                        lash_vocabulary::tool_declaration::TurnControls::finish(
                            lash_vocabulary::json_schema::JsonSchema::any(),
                        ),
                    )]
                    .into(),
                    ..lash_core::sansio::ExecutionEnvironmentSync::default()
                }),
            });
            continue;
        }
        effects.push(effect);
    }
    effects
}

// Finished machines release their records. Retain the history delivered to
// the consumer at progress and completion boundaries, including across restore.
fn delivered_events(effects: &[Effect]) -> impl Iterator<Item = &lash_core::SessionHistoryRecord> {
    effects.iter().flat_map(|effect| match effect {
        Effect::Progress { event_delta, .. } | Effect::Done { event_delta, .. } => {
            event_delta.as_slice()
        }
        _ => &[],
    })
}

fn reply(machine: &mut TurnMachine, effects: &[Effect], parts: Vec<LlmOutputPart>) -> Vec<Effect> {
    reply_with_reason(machine, effects, parts, Default::default())
}
pub(super) fn reply_with_reason(
    machine: &mut TurnMachine,
    effects: &[Effect],
    parts: Vec<LlmOutputPart>,
    terminal_reason: lash_core::LlmTerminalReason,
) -> Vec<Effect> {
    let id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::LlmCall { id, .. } => Some(*id),
            _ => None,
        })
        .expect("scripted provider call");
    machine.handle_response(Response::LlmComplete {
        id,
        text_streamed: false,
        result: Ok(LlmResponse {
            parts,
            terminal_reason,
            ..Default::default()
        }),
    });
    drain(machine)
}
fn run(
    termination: impl Into<Ending>,
    prose: Option<&str>,
    exec: Option<Result<lash_core::ExecResponse, lash_core::ExecCodeFailure>>,
) -> (Vec<serde_json::Value>, Vec<lash_core::CellRecord>) {
    let termination: Ending = termination.into();
    let mut machine = TurnMachine::new(
        config(termination.clone()),
        Vec::new(),
        Default::default(),
        0,
    );
    let initial = drain(&mut machine);
    let parts = match prose {
        Some(prose) => {
            if prose.is_empty() {
                Vec::new()
            } else {
                vec![text(prose)]
            }
        }
        None => vec![call(
            "provider-id",
            "execute_code",
            r#"{"code":"await control.finish(1);"}"#,
        )],
    };
    let mut effects = reply(&mut machine, &initial, parts);
    let mut events = delivered_events(&effects).cloned().collect::<Vec<_>>();
    if let Some(result) = exec {
        let id = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::ExecCode { id, .. } => Some(*id),
                _ => None,
            })
            .expect("exec");
        // Pending provider replay metadata survives the parked execution boundary.
        let checkpoint = serde_json::to_string(&machine.checkpoint()).unwrap();
        let saved = serde_json::from_str(&checkpoint).unwrap();
        machine = TurnMachine::restore_from_checkpoint(config(termination.clone()), saved, None)
            .expect("supported checkpoint");
        drain(&mut machine);
        machine.handle_response(Response::ExecResult { id, result });
        effects = drain(&mut machine);
        events.extend(delivered_events(&effects).cloned());
    }
    let mut checkpoints = Vec::new();
    // The continuation/terminal checkpoint and terminal stream outcome are
    // independent witnesses, not normalization implementation details.
    loop {
        let next = effects.iter().find_map(|effect| match effect {
            Effect::Checkpoint { id, checkpoint, .. } => Some((*id, *checkpoint)),
            _ => None,
        });
        let Some((id, checkpoint)) = next else {
            break;
        };
        checkpoints.push(serde_json::to_value(checkpoint).unwrap());
        let saved =
            serde_json::from_str(&serde_json::to_string(&machine.checkpoint()).unwrap()).unwrap();
        machine = TurnMachine::restore_from_checkpoint(config(termination.clone()), saved, None)
            .expect("supported checkpoint");
        drain(&mut machine);
        machine.handle_response(Response::Checkpoint {
            id,
            delivery: lash_sansio::CheckpointDelivery::default(),
        });
        effects = drain(&mut machine);
        events.extend(delivered_events(&effects).cloned());
    }
    let trajectory: Vec<lash_core::CellRecord> = events
        .iter()
        .filter_map(|record| {
            let lash_core::SessionHistoryRecord::Protocol(event) = record else {
                return None;
            };
            match crate::projection::decode_rlm_protocol_event(event)
                .expect("valid history fixture")
                .as_deref()
            {
                Some(RlmProtocolEvent::RlmTrajectoryEntry(step)) => {
                    Some(lash_core::CellRecord::clone(step))
                }
                _ => None,
            }
        })
        .collect();
    for effect in effects {
        match effect {
            Effect::Emit(lash_core::session_model::SessionStreamEvent::TurnOutcome { outcome }) => {
                checkpoints.push(serde_json::to_value(outcome).unwrap())
            }
            Effect::Done { .. } => checkpoints.push(serde_json::json!("done")),
            Effect::LlmCall { request, .. } => {
                // Channel transport and repair wording deliberately differ. Pin
                // each real projector's rendered continuation independently.
                let scenario = format!(
                    "{}_{}_{}",
                    "native",
                    match termination.mode {
                        lash_core::TerminationMode::Natural => "natural",
                        lash_core::TerminationMode::TerminalRequired => "finish",
                    },
                    if prose.is_some() {
                        "prose"
                    } else if trajectory
                        .iter()
                        .any(|step: &lash_core::CellRecord| { step.result.is_failed() })
                    {
                        "error"
                    } else {
                        "execution"
                    }
                );
                insta::assert_snapshot!(
                    scenario,
                    serde_json::to_string_pretty(&request.messages).unwrap()
                );
            }
            _ => {}
        }
    }
    (checkpoints, trajectory)
}
/// What the executor answers for a cell: one that ran to its end, or one
/// that `control.finish(value)` ended, with that call's settled record.
pub(super) fn response(finish: Option<serde_json::Value>) -> lash_core::ExecResponse {
    let call_id = lash_core::ToolCallId::fixture("finish-call");
    lash_core::ExecResponse {
        prints_retained: None,
        prints: Vec::new(),
        calls: Vec::new(),
        tool_calls: finish
            .iter()
            .map(|value| lash_core::ToolCallRecord {
                call_id: call_id.clone(),
                provider_call_id: None,
                tool: crate::FINISH_TOOL_NAME.to_string(),
                args: value.clone(),
                output: lash_core::ToolCallOutput::finish(value.clone()),
            })
            .collect(),
        printed_images: Vec::new(),
        result: finish.map_or(lash_core::CellOutcome::Completed, |value| {
            lash_core::CellOutcome::Controlled {
                tool_name: crate::FINISH_TOOL_NAME.to_string(),
                call_id,
                control: lash_core::CellControl::Finish {
                    value: value.into(),
                },
            }
        }),
        retained_finish_value: None,
        degraded_bindings: Vec::new(),
        bindings: Default::default(),
        suspended: false,
    }
}

fn scripted_response_contains(parts: &[LlmOutputPart], needle: &str) -> bool {
    parts.iter().any(|part| match part {
        LlmOutputPart::Text { text, .. } | LlmOutputPart::Reasoning { text, .. } => {
            text.contains(needle)
        }
        LlmOutputPart::ToolCall { input_json, .. } => input_json.contains(needle),
    })
}

fn assert_driver_stops_before_queued_provider_response(
    allowed_response: Vec<LlmOutputPart>,
    queued_response: Vec<LlmOutputPart>,
    allowed_code: &str,
    forbidden_code: &str,
) {
    assert!(
        scripted_response_contains(&queued_response, forbidden_code),
        "the queued response must prove it would schedule the forbidden effect"
    );
    let mut provider_script = VecDeque::from([allowed_response, queued_response]);
    let mut turn_config = config(lash_core::TerminationMode::Natural);
    turn_config.turn_budget = lash_core::TurnBudget::bounded(1);
    let mut machine = TurnMachine::new(turn_config, Vec::new(), Default::default(), 0);
    let mut pending = drain(&mut machine);
    let mut observed = Vec::new();
    loop {
        observed.extend(pending.iter().cloned());
        if pending
            .iter()
            .any(|effect| matches!(effect, Effect::Done { .. }))
        {
            break;
        }

        if let Some(id) = pending.iter().find_map(|effect| match effect {
            Effect::LlmCall { id, .. } => Some(*id),
            _ => None,
        }) {
            let parts = provider_script
                .pop_front()
                .expect("the driver exceeded the scripted provider responses");
            machine.handle_response(Response::LlmComplete {
                id,
                text_streamed: false,
                result: Ok(LlmResponse {
                    parts,
                    ..Default::default()
                }),
            });
        } else if let Some(id) = pending.iter().find_map(|effect| match effect {
            Effect::ExecCode { id, .. } => Some(*id),
            _ => None,
        }) {
            machine.handle_response(Response::ExecResult {
                id,
                result: Ok(response(None)),
            });
        } else if let Some(id) = pending.iter().find_map(|effect| match effect {
            Effect::Checkpoint { id, .. } => Some(*id),
            _ => None,
        }) {
            machine.handle_response(Response::Checkpoint {
                id,
                delivery: lash_sansio::CheckpointDelivery::default(),
            });
        } else {
            panic!("driver emitted no blocking effect before completion: {pending:#?}");
        }
        pending = drain(&mut machine);
    }

    assert_eq!(
        observed
            .iter()
            .filter_map(|effect| match effect {
                Effect::ExecCode { code, .. } => Some(code.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![allowed_code],
        "the queued iteration-N effect must never execute"
    );
    assert_eq!(
        observed
            .iter()
            .filter(|effect| matches!(effect, Effect::LlmCall { .. }))
            .count(),
        1,
        "N=1 permits exactly one model call"
    );
    assert_eq!(
        provider_script.len(),
        1,
        "iteration-N response stays unused"
    );
    assert!(scripted_response_contains(
        provider_script.front().expect("unused response"),
        forbidden_code
    ));
    assert!(
        observed.iter().any(|effect| matches!(
            effect,
            Effect::Emit(lash_core::session_model::SessionStreamEvent::TurnOutcome {
                outcome: lash_core::facade_support::TurnOutcome::Stopped(
                    lash_core::facade_support::TurnStop::MaxTurns
                )
            })
        )),
        "budget exhaustion emits the typed stop"
    );
    let done_messages = observed
        .iter()
        .find_map(|effect| match effect {
            Effect::Done { messages, .. } => Some(messages),
            _ => None,
        })
        .expect("budget exhaustion finishes the turn");
    assert!(
        done_messages
            .iter()
            .all(|message| message.role != lash_core::MessageRole::System),
        "the transcript contains no synthetic system message"
    );
    assert!(!observed.iter().any(|effect| {
        matches!(effect, Effect::ExecCode { code, .. } if code.contains(forbidden_code))
    }));
}

#[test]
fn native_driver_stops_at_budget_before_queued_provider_response() {
    assert_driver_stops_before_queued_provider_response(
        vec![call(
            "allowed-call",
            "execute_code",
            r#"{"code":"print \"native-allowed\""}"#,
        )],
        vec![call(
            "forbidden-call",
            "execute_code",
            r#"{"code":"print \"native-forbidden-iteration-one\""}"#,
        )],
        r#"print "native-allowed""#,
        "native-forbidden-iteration-one",
    );
}

fn assert_simultaneous_turn_and_no_progress_exhaustion_prefers_silent_turn_stop() {
    let mut turn_config = config(lash_core::TerminationMode::TerminalRequired);
    turn_config.turn_budget = lash_core::TurnBudget::bounded(1);
    turn_config.no_progress_budget = lash_core::NoProgressBudget::bounded(1);
    let mut machine = TurnMachine::new(turn_config, Vec::new(), Default::default(), 0);

    let initial = drain(&mut machine);
    let effects = reply(
        &mut machine,
        &initial,
        vec![text("prose without a finishing cell")],
    );

    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(lash_core::session_model::SessionStreamEvent::TurnOutcome {
            outcome: lash_core::facade_support::TurnOutcome::Stopped(
                lash_core::facade_support::TurnStop::MaxTurns
            )
        })
    )));
    assert_eq!(
        delivered_events(&initial)
            .chain(delivered_events(&effects))
            .filter(|event| matches!(event, lash_core::SessionHistoryRecord::Conversation(_)))
            .count(),
        0,
        "turn-budget exhaustion must not append no-progress conversation feedback"
    );
    let done_messages = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Done { messages, .. } => Some(messages),
            _ => None,
        })
        .expect("simultaneous exhaustion finishes the turn");
    assert!(
        done_messages
            .iter()
            .all(|message| message.role != lash_core::MessageRole::System),
        "turn-budget exhaustion must not append a synthetic system message"
    );
}

#[test]
fn native_simultaneous_turn_and_no_progress_exhaustion_prefers_silent_turn_stop() {
    assert_simultaneous_turn_and_no_progress_exhaustion_prefers_silent_turn_stop();
}

#[test]
fn native_normalization_covers_every_schema_refusal() {
    let cases = [
        (
            vec![call("a", "execute_code", "null")],
            "retry_invalid_arguments",
        ),
        (
            vec![call("a", "execute_code", "[]")],
            "retry_invalid_arguments",
        ),
        (
            vec![call("a", "execute_code", "42")],
            "retry_invalid_arguments",
        ),
        (
            vec![call("a", "execute_code", r#"{"code":null}"#)],
            "retry_missing_code",
        ),
        (
            vec![call("a", "execute_code", r#"{"code":42}"#)],
            "retry_missing_code",
        ),
        (
            vec![call("a", "execute_code", r#"{"code":[]}"#)],
            "retry_missing_code",
        ),
        (
            vec![call(
                "a",
                "execute_code",
                r#"{"code":"finish(1)","extra":true}"#,
            )],
            "retry_invalid_arguments",
        ),
        (vec![call("a", "unknown", "{}")], "retry_unknown_tool"),
        (
            vec![call("a", "execute_code", "{")],
            "retry_invalid_arguments",
        ),
        (vec![call("a", "execute_code", "{}")], "retry_missing_code"),
        (
            vec![call("a", "execute_code", r#"{"code":" "}"#)],
            "retry_missing_code",
        ),
        (
            vec![
                call("a", "execute_code", "{}"),
                call("b", "execute_code", "{}"),
            ],
            "retry_multiple_calls",
        ),
    ];
    for (parts, decision) in cases {
        let mut machine = TurnMachine::new(
            config(lash_core::TerminationMode::Natural),
            Vec::new(),
            Default::default(),
            0,
        );
        let initial = drain(&mut machine);
        let effects = reply(&mut machine, &initial, parts);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::ExecCode { .. }))
        );
        let decisions = machine
            .events()
            .iter()
            .filter_map(|event| {
                let lash_core::SessionHistoryRecord::Protocol(event) = event else {
                    return None;
                };
                match crate::projection::decode_rlm_protocol_event(event)
                    .expect("valid history fixture")
                    .as_deref()
                {
                    Some(RlmProtocolEvent::RlmDiagnostic(d))
                        if d.phase == lash_rlm_types::RlmDiagnosticPhase::NativeExtraction =>
                    {
                        Some(d.payload["decision"].clone())
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(decisions, vec![serde_json::json!(decision)]);
    }
    let mut machine = TurnMachine::new(
        config(lash_core::TerminationMode::Natural),
        Vec::new(),
        Default::default(),
        0,
    );
    let initial = drain(&mut machine);
    let effects = reply(
        &mut machine,
        &initial,
        vec![call("valid", "execute_code", r#"{"code":"finish(17)"}"#)],
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::ExecCode { code, .. } if code == "finish(17)"))
    );
}
#[test]
fn native_reasoning_only_is_provider_error() {
    let mut machine = TurnMachine::new(
        config(lash_core::TerminationMode::Natural),
        Vec::new(),
        Default::default(),
        0,
    );
    let initial = drain(&mut machine);
    let effects = reply(
        &mut machine,
        &initial,
        vec![LlmOutputPart::Reasoning {
            text: "thinking".to_string(),
            replay: None,
        }],
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(lash_core::session_model::SessionStreamEvent::TurnOutcome {
            outcome: lash_core::facade_support::TurnOutcome::Stopped(
                lash_core::facade_support::TurnStop::ProviderError { .. }
            )
        })
    )));
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Done { .. }))
    );
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::ExecCode { .. } | Effect::LlmCall { .. }))
    );
}

#[tokio::test]
async fn factory_selects_native_abi_and_completed_cell_events() {
    let factory = crate::RlmProtocolPluginFactory::new(
        crate::RlmProtocolPluginConfig::builder()
            .instruction_limit(crate::InstructionBound::instructions(1000))
            .memory_limit(crate::MemoryBound::mebibytes(1))
            .build(),
        crate::dialect::CellDialect::typescript(),
    );
    let host = lash_core::facade_support::PluginHost::new(
        vec![Arc::new(factory)],
        lash_core::ExecutionBudgets::recommended(),
        lash_core::trace::TraceRuntime::new(std::sync::Arc::new(
            lash_core::facade_support::SystemClock,
        )),
    );
    let session = host
        .build_session(PluginSessionRequest::creation(
            "native-plugin",
            lash_core::plugin::SessionAuthorityContext::fixture(),
        ))
        .unwrap();
    let catalog = lash_core::testing::environment_projection(&session)
        .map(|projection| projection.catalog)
        .unwrap();
    let preamble = session
        .protocol_driver()
        .build_preamble(lash_core::ProtocolBuildInput {
            tool_catalog: catalog,
            plugin_extensions: Default::default(),
            writer_formats: lash_core::build_newest_writer_formats(),
        });
    assert_eq!(preamble.tool_specs.len(), 1);
    assert_eq!(preamble.tool_specs[0].name, "execute_code");
    let schema = preamble.tool_specs[0].input_schema.canonical();
    assert_eq!(schema["required"], serde_json::json!(["code"]));
    assert_eq!(schema["additionalProperties"], false);
    let response = LlmResponse {
        parts: vec![call(
            "native-id",
            "execute_code",
            r#"{"code":"finish(1);"}"#,
        )],
        ..Default::default()
    };
    let transforms = session
        .transform_assistant_response(
            &lash_core::plugin::ModelCallAddress {
                session_id: lash_vocabulary::identity::SessionId::from("native-plugin"),
                turn_id: lash_vocabulary::identity::TurnId::from("hook-turn"),
                call_id: lash_core::LlmCallId("hook-call".into()),
                attempt: Some(1),
            },
            response,
            &session.assistant_response_plan(),
            &[],
        )
        .await
        .unwrap();
    let names = transforms
        .iter()
        .flat_map(|transform| &transform.value.events)
        .filter_map(|event| match event {
            lash_core::PluginRuntimeEvent::Custom { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        ["rlm_typescript_cell_start", "rlm_typescript_cell_end"]
    );
}

#[test]
fn multiple_calls_spend_one_stall_attempt_and_answer_every_id() {
    let mut machine = TurnMachine::new(
        config(lash_core::TerminationMode::Natural),
        Vec::new(),
        Default::default(),
        0,
    );
    let mut effects = drain(&mut machine);
    let mut events = Vec::new();
    for attempt in 1..=3 {
        let parts = vec![
            call(
                &format!("a{attempt}"),
                "execute_code",
                r#"{"code":"print 1"}"#,
            ),
            call(
                &format!("b{attempt}"),
                "execute_code",
                r#"{"code":"print 2"}"#,
            ),
        ];
        effects = reply(&mut machine, &effects, parts);
        events.extend(delivered_events(&effects).cloned());
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::ExecCode { .. }))
        );
        if attempt < 3 {
            let id = effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::Checkpoint { id, .. } => Some(*id),
                    _ => None,
                })
                .expect("one repair, budget remains");
            machine.handle_response(Response::Checkpoint {
                id,
                delivery: lash_sansio::CheckpointDelivery::default(),
            });
            effects = drain(&mut machine);
            events.extend(delivered_events(&effects).cloned());
        } else {
            assert!(
                effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::Done { .. }))
            );
        }
    }
    let mut pairs = Vec::new();
    let mut refused_call = None;
    for event in &events {
        let lash_core::SessionHistoryRecord::Conversation(record) = event else {
            continue;
        };
        if !super::transport::is_exchange_message(record.origin.as_ref(), &record.parts) {
            continue;
        }
        if record.role == lash_core::MessageRole::Assistant {
            refused_call = Some(record.parts.clone());
            continue;
        }
        let text = record
            .parts
            .iter()
            .filter_map(lash_core::Part::tool_result_content)
            .flatten()
            .find_map(|block| match block {
                lash_core::facade_support::ModelToolReturnPart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .expect("a refused call's results carry the correction");
        {
            let parts = refused_call.take().expect("results answer a refused call");
            let mut messages = Vec::new();
            super::transport::append_pair(&mut messages, &parts, &text);
            let outputs = messages
                .iter()
                .flat_map(|message| message.blocks.iter())
                .filter_map(|block| match block {
                    lash_core::llm::types::LlmContentBlock::ToolResult {
                        call_id, content, ..
                    } => Some((call_id.clone(), content.clone())),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(outputs.len(), 2);
            assert_eq!(outputs[0].1, outputs[1].1);
            pairs.extend(outputs);
        }
    }
    assert_eq!(pairs.len(), 6);
}

#[test]
fn output_limit_prose_repairs() {
    {
        let mut machine = TurnMachine::new(
            config(lash_core::TerminationMode::Natural),
            Vec::new(),
            Default::default(),
            0,
        );
        let initial = drain(&mut machine);
        let effects = reply_with_reason(
            &mut machine,
            &initial,
            vec![text("partial answer")],
            lash_core::LlmTerminalReason::OutputLimit,
        );
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                Effect::Checkpoint {
                    checkpoint: lash_core::CheckpointKind::AfterWork,
                    ..
                }
            )),
            "truncated prose must repair, not finish"
        );
        let id = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Checkpoint { id, .. } => Some(*id),
                _ => None,
            })
            .unwrap();
        let saved =
            serde_json::from_str(&serde_json::to_string(&machine.checkpoint()).unwrap()).unwrap();
        machine = TurnMachine::restore_from_checkpoint(
            config(lash_core::TerminationMode::Natural),
            saved,
            None,
        )
        .expect("supported checkpoint");
        drain(&mut machine);
        machine.handle_response(Response::Checkpoint {
            id,
            delivery: Default::default(),
        });
        let effects = drain(&mut machine);
        let request = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::LlmCall { request, .. } => Some(request),
                _ => None,
            })
            .expect("repair requests another model response");
        let rendered = serde_json::to_string(&request.messages).unwrap();
        assert!(rendered.contains("partial answer"));
        assert!(rendered.contains("output limit"));
        insta::assert_snapshot!(
            "native_output_limit_prose",
            serde_json::to_string_pretty(&request.messages).unwrap()
        );
    }
}

#[test]
fn output_limit_calls_repair_without_execution_until_stall_budget() {
    for arguments in [r#"{"code":"finish(1);"}"#, r#"{"code":"finish"#] {
        let mut machine = TurnMachine::new(
            config(lash_core::TerminationMode::Natural),
            Vec::new(),
            Default::default(),
            0,
        );
        let mut effects = drain(&mut machine);
        let mut events = Vec::new();
        for attempt in 1..=3 {
            effects = reply_with_reason(
                &mut machine,
                &effects,
                vec![call("truncated", "execute_code", arguments)],
                lash_core::LlmTerminalReason::OutputLimit,
            );
            events.extend(delivered_events(&effects).cloned());
            assert!(
                !effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::ExecCode { .. })),
                "truncated code must never execute"
            );
            if attempt == 3 {
                assert!(
                    effects
                        .iter()
                        .any(|effect| matches!(effect, Effect::Done { .. }))
                );
                break;
            }
            let id = effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::Checkpoint { id, .. } => Some(*id),
                    _ => None,
                })
                .unwrap();
            let saved =
                serde_json::from_str(&serde_json::to_string(&machine.checkpoint()).unwrap())
                    .unwrap();
            machine = TurnMachine::restore_from_checkpoint(
                config(lash_core::TerminationMode::Natural),
                saved,
                None,
            )
            .expect("supported checkpoint");
            drain(&mut machine);
            machine.handle_response(Response::Checkpoint {
                id,
                delivery: Default::default(),
            });
            effects = drain(&mut machine);
            events.extend(delivered_events(&effects).cloned());
            let request = effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::LlmCall { request, .. } => Some(request),
                    _ => None,
                })
                .unwrap();
            let blocks = request
                .messages
                .iter()
                .flat_map(|message| message.blocks.iter())
                .collect::<Vec<_>>();
            assert!(blocks.iter().any(|block| matches!(block, lash_core::llm::types::LlmContentBlock::ToolCall { call_id, input_json, .. } if call_id == "truncated" && input_json == arguments)));
            assert!(blocks.iter().any(|block| matches!(block, lash_core::llm::types::LlmContentBlock::ToolResult { call_id, content, .. } if call_id == "truncated" && lash_llm_transport::tool_result_text(content).contains("output limit"))));
        }
        let decisions = events
            .iter()
            .filter_map(|record| match record {
                lash_core::SessionHistoryRecord::Protocol(event) => {
                    match crate::projection::decode_rlm_protocol_event(event)
                        .expect("valid history fixture")
                        .as_deref()
                    {
                        Some(RlmProtocolEvent::RlmDiagnostic(d))
                            if d.phase == lash_rlm_types::RlmDiagnosticPhase::NativeExtraction =>
                        {
                            Some(d.payload["decision"].clone())
                        }
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            decisions,
            vec![serde_json::json!("retry_output_limit_call"); 3]
        );
    }
}

#[test]
fn configured_prompt_is_instructions() {
    {
        for (prompt, expected) in [("configured prompt", Some("configured prompt")), ("", None)] {
            let mut machine = TurnMachine::new(
                config(lash_core::TerminationMode::Natural),
                Vec::new(),
                Default::default(),
                0,
            );
            let effects = drain(&mut machine);
            let projected = effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::LlmCall { request, .. } => Some(request),
                    _ => None,
                })
                .expect("initial provider request");
            assert_eq!(projected.instructions, None, "history only");
            // The call's admission lowers its composed prompt onto the
            // projected request.
            let mut request = projected.materialize();
            lash_core::sansio::place_prompt(
                &mut request,
                (!prompt.is_empty()).then(|| Arc::from(prompt)),
                None,
                true,
            );
            assert_eq!(request.instructions.as_deref(), expected, "native");
            assert!(
                request
                    .messages
                    .iter()
                    .all(|message| message.role != lash_core::llm::types::LlmRole::System),
                "configured prompt must not enter conversation history"
            );
        }
    }
}

fn native_extraction_payloads(machine: &TurnMachine) -> Vec<serde_json::Value> {
    machine
        .events()
        .iter()
        .filter_map(|event| {
            let lash_core::SessionHistoryRecord::Protocol(event) = event else {
                return None;
            };
            match crate::projection::decode_rlm_protocol_event(event)
                .expect("valid history fixture")
                .as_deref()
            {
                Some(RlmProtocolEvent::RlmDiagnostic(d))
                    if d.phase == lash_rlm_types::RlmDiagnosticPhase::NativeExtraction =>
                {
                    Some(d.payload.clone())
                }
                _ => None,
            }
        })
        .collect()
}

/// The reply fingerprint names "the reply a host would compare and not the
/// reasoning summary that varies between two identical answers"
/// (`protocol/stall.rs`). Two attempts answering with the same prose and the
/// same program are one repeated reply however the provider narrated it, so a
/// host reading its stall evidence must see one fingerprint twice.
#[test]
fn native_reasoning_does_not_move_the_stall_reply_fingerprint() {
    let fingerprint_for = |reasoning: &str| {
        let mut machine = TurnMachine::new(
            config(lash_core::TerminationMode::Natural),
            Vec::new(),
            Default::default(),
            0,
        );
        let initial = drain(&mut machine);
        reply(
            &mut machine,
            &initial,
            vec![
                LlmOutputPart::Reasoning {
                    text: reasoning.to_string(),
                    replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                        encrypted_content: Some(format!("{reasoning}-blob")),
                        ..Default::default()
                    }),
                },
                text("Ready."),
                call("call-1", "execute_code", r#"{"code":"finish(\"ok\")"}"#),
            ],
        );
        let payloads = native_extraction_payloads(&machine);
        assert_eq!(payloads.len(), 1, "one attempt, one diagnostic");
        payloads[0]["reply_fingerprint"]
            .as_str()
            .expect("every extraction diagnostic fingerprints its reply")
            .to_string()
    };

    assert_eq!(
        fingerprint_for("Plan A."),
        fingerprint_for("Plan B, at length."),
        "identical replies fingerprint identically"
    );
}

#[test]
fn native_user_stop_is_terminal_live_and_after_restore() {
    for restore in [false, true] {
        let mut machine = TurnMachine::new(
            config(lash_core::TerminationMode::Natural),
            Vec::new(),
            Default::default(),
            0,
        );
        let initial = drain(&mut machine);
        let mut effects = reply(
            &mut machine,
            &initial,
            vec![call("stop-call", "execute_code", r#"{"code":"print(1)"}"#)],
        );
        let mut history = delivered_events(&initial)
            .chain(delivered_events(&effects))
            .cloned()
            .collect::<Vec<_>>();
        if restore {
            let checkpoint =
                serde_json::from_slice(&serde_json::to_vec(&machine.checkpoint()).unwrap())
                    .unwrap();
            machine = TurnMachine::restore_from_checkpoint(
                config(lash_core::TerminationMode::Natural),
                checkpoint,
                None,
            )
            .unwrap();
            effects = drain(&mut machine);
            history.extend(delivered_events(&effects).cloned());
        }
        let id = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::ExecCode { id, .. } => Some(*id),
                _ => None,
            })
            .unwrap();
        machine.record_cancellation_evidence(lash_protocol_types::TurnCancellationEvidence {
            request_id: "user-stop".into(),
            origin: Some("host".into()),
            reason: Some("Stop".into()),
            undelivered: lash_protocol_types::TurnCancelUndeliveredInputPolicy::Defer,
            mode: lash_protocol_types::TurnCancelMode::Immediate,
            honoured_after_step: None,
        });
        machine.handle_response(Response::ExecResult {
            id,
            result: Ok(response(None)),
        });
        let mut terminal = drain(&mut machine);
        if let Some(id) = terminal.iter().find_map(|effect| match effect {
            Effect::Checkpoint { id, .. } => Some(*id),
            _ => None,
        }) {
            machine.handle_response(Response::Checkpoint {
                id,
                delivery: Default::default(),
            });
            terminal.extend(drain(&mut machine));
        }
        assert!(machine.is_done());
        assert!(terminal.iter().any(|effect| matches!(
            effect,
            Effect::Emit(lash_core::session_model::SessionStreamEvent::TurnOutcome {
                outcome: lash_core::facade_support::TurnOutcome::Stopped(
                    lash_core::facade_support::TurnStop::Cancelled { .. }
                )
            })
        )));
        assert!(
            !terminal
                .iter()
                .any(|effect| matches!(effect, Effect::LlmCall { .. } | Effect::ExecCode { .. }))
        );
        history.extend(delivered_events(&terminal).cloned());
        assert!(!history.iter().any(|event| matches!(event, lash_core::SessionHistoryRecord::Protocol(event) if matches!(crate::projection::decode_rlm_protocol_event(event).expect("valid history fixture").as_deref(), Some(RlmProtocolEvent::RlmTrajectoryEntry(_))))));
    }
}

#[test]
fn a_step_archive_survives_both_driver_checkpoint_paths() {
    let archive = lash_core::RetainedValue {
        output: lash_core::RetainedOutput {
            reference: lash_core::AttachmentRef {
                id: lash_core::attachments::content_id(&vec![b'p'; 100_000]),
                media_type: "application/json".parse().expect("media type"),
                byte_len: 100_000,
                type_metadata: None,
                label: None,
            },
            witness: "bounded preview".into(),
        },
        shape: lash_core::ValueShape::Array {
            length: 1,
            items: Some(Box::new(lash_core::ValueShape::String { bytes: 99_996 })),
        },
    };
    {
        let mut exec = response(Some(serde_json::json!(1)));
        exec.prints_retained = Some(archive.clone());
        let (_, steps) = run(lash_core::TerminationMode::Natural, None, Some(Ok(exec)));
        let [step] = steps.as_slice() else {
            panic!("one trajectory step: {steps:?}");
        };
        assert!(step.prints.is_empty());
        assert_eq!(step.prints_retained.as_ref(), Some(&archive));
    }
}

/// L19: a restored pending execution accounts its recorded tool answer once,
/// keeps the full terminal value, and spends no additional model usage.
#[test]
fn a_recorded_tool_terminal_keeps_its_payload_and_usage_across_both_checkpoints() {
    let payload =
        serde_json::json!({"terminal": "x".repeat(80_000), "nested": [1, {"complete": true}]});
    {
        let mut machine = TurnMachine::new(
            config(lash_core::TerminationMode::Natural),
            Vec::new(),
            Default::default(),
            0,
        );
        let initial = drain(&mut machine);
        let id = initial
            .iter()
            .find_map(|effect| match effect {
                Effect::LlmCall { id, .. } => Some(*id),
                _ => None,
            })
            .expect("one model call");
        let parts = {
            vec![call(
                "terminal",
                "execute_code",
                r#"{"code":"await tools.appLookup({});"}"#,
            )]
        };
        machine.handle_response(Response::LlmComplete {
            id,
            text_streamed: false,
            result: Ok(LlmResponse {
                parts,
                usage: lash_core::llm::types::LlmUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    ..Default::default()
                },
                ..Default::default()
            }),
        });
        drain(&mut machine);
        let saved = serde_json::to_value(machine.checkpoint()).unwrap();
        let usage = saved["checkpoint"]["cumulative_usage"].clone();
        machine = TurnMachine::restore_from_checkpoint(
            config(lash_core::TerminationMode::Natural),
            serde_json::from_value(saved).unwrap(),
            None,
        )
        .expect("restore pending execution");
        let replayed = drain(&mut machine);
        assert!(
            !replayed
                .iter()
                .any(|effect| matches!(effect, Effect::LlmCall { .. }))
        );
        let exec_id = replayed
            .iter()
            .find_map(|effect| match effect {
                Effect::ExecCode { id, .. } => Some(*id),
                _ => None,
            })
            .expect("redeliver the pending execution");
        let mut exec = response(None);
        exec.calls.push(lash_core::ExecutedCall {
            result_summary: None,
            operation: "tools.app_lookup".into(),
            outcome: lash_core::ExecutedCallOutcome::Ok {},
            call_id: Some(lash_core::ToolCallId::fixture("terminal-call")),
        });
        exec.tool_calls.push(lash_core::ToolCallRecord {
            call_id: lash_core::ToolCallId::fixture("terminal-call"),
            provider_call_id: None,
            tool: "app_lookup".into(),
            args: serde_json::json!({}),
            output: lash_core::ToolCallOutput::finish(payload.clone()),
        });
        exec.result = lash_core::CellOutcome::Controlled {
            tool_name: "app_lookup".into(),
            call_id: lash_core::ToolCallId::fixture("terminal-call"),
            control: lash_core::CellControl::Finish {
                value: payload.clone().into(),
            },
        };
        machine.handle_response(Response::ExecResult {
            id: exec_id,
            result: Ok(exec),
        });
        let accounted = drain(&mut machine);
        let outputs = accounted
            .iter()
            .filter_map(|effect| match effect {
                Effect::Emit(lash_core::session_model::SessionStreamEvent::ToolCall {
                    output,
                    ..
                }) => Some(output),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(outputs.len(), 1, "one recorded tool accounting event");
        assert_eq!(
            *outputs[0],
            lash_core::ToolCallOutput::finish(payload.clone()),
            "the accounting event retains the complete terminal payload"
        );
        let checkpoint_id = accounted
            .iter()
            .find_map(|effect| match effect {
                Effect::Checkpoint { id, .. } => Some(*id),
                _ => None,
            })
            .expect("terminal checkpoint");
        let saved = serde_json::to_value(machine.checkpoint()).unwrap();
        assert_eq!(saved["checkpoint"]["cumulative_usage"], usage);
        machine = TurnMachine::restore_from_checkpoint(
            config(lash_core::TerminationMode::Natural),
            serde_json::from_value(saved).unwrap(),
            None,
        )
        .expect("restore terminal checkpoint");
        let redelivered = drain(&mut machine);
        assert!(
            !redelivered
                .iter()
                .any(|effect| matches!(effect, Effect::LlmCall { .. } | Effect::ExecCode { .. }))
        );
        machine.handle_response(Response::Checkpoint {
            id: checkpoint_id,
            delivery: Default::default(),
        });
        let terminal = drain(&mut machine);
        let outcome = terminal
            .iter()
            .find_map(|effect| match effect {
                Effect::Emit(lash_core::session_model::SessionStreamEvent::TurnOutcome {
                    outcome,
                }) => Some(outcome),
                _ => None,
            })
            .expect("terminal outcome");
        assert_eq!(
            *outcome,
            lash_core::facade_support::TurnOutcome::Finished(
                lash_core::facade_support::TurnFinish::Finished {
                    tool_name: "app_lookup".into(),
                    value: payload.clone()
                },
            )
        );
        assert_eq!(
            serde_json::to_value(machine.checkpoint()).unwrap()["checkpoint"]["cumulative_usage"],
            usage
        );
        assert!(
            terminal
                .iter()
                .any(|effect| matches!(effect, Effect::Done { .. }))
        );
    }
}
