use lash_core::sansio::Response;
use lash_core::{Effect, LlmOutputPart, LlmResponse, TurnMachine, TurnMachineConfig};
use lash_rlm_types::{RlmProtocolEvent, RlmTermination, RlmTurnOptions};
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use std::collections::VecDeque;
use std::sync::Arc;

fn config(native: bool, termination: RlmTermination) -> TurnMachineConfig {
    let factory = crate::RlmProtocolPluginFactory::new(
        crate::RlmProtocolPluginConfig::builder()
            .channel(if native {
                crate::RlmChannel::NativeTool
            } else {
                crate::RlmChannel::Cell
            })
            .instruction_limit(crate::InstructionBound::instructions(1000))
            .wall_clock(crate::WallClockBound::secs(1))
            .memory_limit(crate::MemoryBound::mebibytes(1))
            .build(),
        lashlang::global_in_memory_lashlang_artifact_store(),
    )
    .with_process_lifecycle(false);
    let host = lash_core::facade_support::PluginHost::new(vec![Arc::new(factory)]);
    let session = host.build_session("parity").unwrap();
    let preamble = session
        .protocol_driver()
        .build_preamble(lash_core::ProtocolBuildInput {
            tool_catalog: session
                .resolved_tool_catalog(&SessionId::from("parity"))
                .unwrap(),
            plugin_extensions: Default::default(),
            trigger_events: Default::default(),
            extra_prompt_contributions: Vec::new(),
        });
    TurnMachineConfig {
        protocol_driver: preamble.config.protocol,
        projector: preamble.config.projector,
        sync_execution_environment: false,
        model: "scripted".to_string(),
        max_context_tokens: None,
        turn_budget: lash_core::TurnBudget::bounded(4),
        no_progress_budget: lash_core::NoProgressBudget::bounded(3),
        model_variant: Default::default(),
        model_capability: Default::default(),
        generation: Default::default(),
        autonomous: false,
        tool_specs: Arc::new(Vec::new()),
        system_prompt: Arc::from(""),
        session_id: SessionId::from("parity"),
        agent_frame_id: "parity-frame".to_string(),
        turn_id: TurnId::from("parity-turn"),
        emit_llm_trace: false,
        termination: lash_core::ProtocolTurnOptions::typed(RlmTurnOptions {
            termination: Some(termination),
            final_answer_format: None,
        })
        .unwrap(),
    }
}

#[test]
fn rlm_catalog_distinguishes_ambient_from_restricted_empty_access() {
    let build = |session_id: &str, tool_access: lash_core::SessionToolAccess| {
        let factory = crate::RlmProtocolPluginFactory::new(
            crate::RlmProtocolPluginConfig::builder()
                .channel(crate::RlmChannel::Cell)
                .instruction_limit(crate::InstructionBound::instructions(1000))
                .wall_clock(crate::WallClockBound::secs(1))
                .memory_limit(crate::MemoryBound::mebibytes(1))
                .build(),
            lashlang::global_in_memory_lashlang_artifact_store(),
        )
        .with_process_lifecycle(false);
        lash_core::facade_support::PluginHost::new(vec![Arc::new(factory)])
            .build_session_with_parent(
                session_id,
                None,
                lash_core::plugin::SessionCreationConfig {
                    authority: lash_core::plugin::SessionAuthorityContext {
                        tool_access,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .expect("RLM protocol session")
    };

    let ambient = build("rlm-ambient", lash_core::SessionToolAccess::ambient());
    assert!(
        ambient
            .resolved_tool_catalog(&SessionId::from("rlm-ambient"))
            .expect("ambient RLM catalog")
            .has_callable_tool("continue_as")
    );

    let restricted = build(
        "rlm-restricted-empty",
        lash_core::SessionToolAccess::restricted([]).expect("restricted empty is valid"),
    );
    let catalog = restricted
        .resolved_tool_catalog(&SessionId::from("rlm-restricted-empty"))
        .expect("restricted-empty RLM catalog");
    assert!(catalog.tools.is_empty());
    assert!(
        crate::tool_catalog::rlm_prompt_tool_docs(
            &catalog,
            &crate::dialect::lashlang_test_dialect(),
            crate::protocol::RlmPromptFeatures::default(),
        )
        .is_empty()
    );
}

fn text(text: &str) -> LlmOutputPart {
    LlmOutputPart::Text {
        text: text.to_string(),
        response_meta: None,
    }
}
fn phased_text(phase: &str, text: &str) -> LlmOutputPart {
    LlmOutputPart::Text {
        text: text.to_string(),
        response_meta: Some(lash_core::llm::types::ResponseTextMeta {
            phase: Some(phase.to_string()),
            ..Default::default()
        }),
    }
}
fn typescript_cell_config(termination: RlmTermination) -> TurnMachineConfig {
    let mut config = config(false, termination);
    config.protocol_driver = Arc::new(crate::protocol::RlmDriver::for_language("typescript"));
    config
}
fn lashlang_cell_config(termination: RlmTermination) -> TurnMachineConfig {
    let mut config = config(false, termination);
    config.protocol_driver = Arc::new(crate::protocol::RlmDriver::for_language("lashlang"));
    config
}
fn call(id: &str, name: &str, args: &str) -> LlmOutputPart {
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
fn drain(machine: &mut TurnMachine) -> Vec<Effect> {
    let mut effects = Vec::new();
    while let Some(effect) = machine.poll_effect() {
        effects.push(effect);
    }
    effects
}
fn reply(machine: &mut TurnMachine, effects: &[Effect], parts: Vec<LlmOutputPart>) -> Vec<Effect> {
    reply_with_reason(machine, effects, parts, Default::default())
}
fn reply_with_reason(
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
    native: bool,
    termination: RlmTermination,
    prose: Option<&str>,
    exec: Option<Result<lash_core::ExecResponse, String>>,
) -> (
    Vec<serde_json::Value>,
    Vec<lash_rlm_types::RlmTrajectoryEntry>,
) {
    let mut machine = TurnMachine::new(
        config(native, termination.clone()),
        Vec::new(),
        Arc::new(Vec::new()),
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
        None if native => vec![call(
            "provider-id",
            "execute_code",
            r#"{"code":"finish 1"}"#,
        )],
        None => vec![text("<lashlang>\nfinish 1\n</lashlang>")],
    };
    let attempted_finish =
        matches!(&exec, Some(Ok(response)) if response.terminal_finish.is_some());
    let mut effects = reply(&mut machine, &initial, parts);
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
        machine = TurnMachine::restore_from_checkpoint(config(native, termination.clone()), saved)
            .expect("supported checkpoint");
        drain(&mut machine);
        machine.handle_response(Response::ExecResult { id, result });
        effects = drain(&mut machine);
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
        machine = TurnMachine::restore_from_checkpoint(config(native, termination.clone()), saved)
            .expect("supported checkpoint");
        drain(&mut machine);
        machine.handle_response(Response::Checkpoint {
            id,
            delivery: lash_sansio::CheckpointDelivery::default(),
        });
        effects = drain(&mut machine);
    }
    let trajectory: Vec<lash_rlm_types::RlmTrajectoryEntry> = machine
        .events()
        .iter()
        .filter_map(|record| {
            let lash_core::SessionHistoryRecord::Protocol(event) = record else {
                return None;
            };
            match crate::projection::decode_rlm_protocol_event(event) {
                Some(RlmProtocolEvent::RlmTrajectoryEntry(step)) => Some(step),
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
                    if native { "native" } else { "cell" },
                    match termination {
                        RlmTermination::Natural => "natural",
                        RlmTermination::FinishRequired { schema: None } => "finish",
                        _ => "schema",
                    },
                    if prose.is_some() {
                        "prose"
                    } else if trajectory
                        .iter()
                        .any(|step: &lash_rlm_types::RlmTrajectoryEntry| step.error.is_some())
                    {
                        if attempted_finish {
                            "schema_mismatch"
                        } else {
                            "error"
                        }
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
fn response(finish: Option<serde_json::Value>) -> lash_core::ExecResponse {
    lash_core::ExecResponse {
        observations: Vec::new(),
        calls: Vec::new(),
        printed_images: Vec::new(),
        error: None,
        duration_ms: 0,
        degraded_bindings: Vec::new(),
        terminal_finish: finish,
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
    native: bool,
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
    let mut turn_config = config(native, RlmTermination::Natural);
    turn_config.turn_budget = lash_core::TurnBudget::bounded(1);
    let mut machine = TurnMachine::new(turn_config, Vec::new(), Arc::new(Vec::new()), 0);
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
        true,
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

#[test]
fn cell_driver_stops_at_budget_before_queued_provider_response() {
    assert_driver_stops_before_queued_provider_response(
        false,
        vec![text("<lashlang>\nprint \"cell-allowed\"\n</lashlang>")],
        vec![text(
            "<lashlang>\nprint \"cell-forbidden-iteration-one\"\n</lashlang>",
        )],
        r#"print "cell-allowed""#,
        "cell-forbidden-iteration-one",
    );
}

fn assert_simultaneous_turn_and_no_progress_exhaustion_prefers_silent_turn_stop(native: bool) {
    let mut turn_config = config(native, RlmTermination::FinishRequired { schema: None });
    turn_config.turn_budget = lash_core::TurnBudget::bounded(1);
    turn_config.no_progress_budget = lash_core::NoProgressBudget::bounded(1);
    let mut machine = TurnMachine::new(turn_config, Vec::new(), Arc::new(Vec::new()), 0);

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
        machine
            .events()
            .iter()
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
    assert_simultaneous_turn_and_no_progress_exhaustion_prefers_silent_turn_stop(true);
}

#[test]
fn cell_protocol_simultaneous_turn_and_no_progress_exhaustion_prefers_silent_turn_stop() {
    assert_simultaneous_turn_and_no_progress_exhaustion_prefers_silent_turn_stop(false);
}

#[test]
fn termination_and_trajectory_parity() {
    for termination in [
        RlmTermination::Natural,
        RlmTermination::FinishRequired { schema: None },
        RlmTermination::FinishRequired {
            schema: Some(serde_json::json!({"type":"string"})),
        },
    ] {
        for prose in ["answer", ""] {
            assert_eq!(
                run(false, termination.clone(), Some(prose), None),
                run(true, termination.clone(), Some(prose), None)
            );
        }
        for result in [
            Ok(response(Some(serde_json::json!(1)))),
            Err("runtime error".to_string()),
            Ok(response(None)),
        ] {
            assert_eq!(
                run(false, termination.clone(), None, Some(result.clone())),
                run(true, termination.clone(), None, Some(result))
            );
        }
    }
}
#[test]
fn native_rejects_malformed_calls_without_execution() {
    let cases = [
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
                call("a", "execute_code", "{}"),
            ],
            "retry_duplicate_call_id",
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
            config(true, RlmTermination::Natural),
            Vec::new(),
            Arc::new(Vec::new()),
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
                match crate::projection::decode_rlm_protocol_event(event) {
                    Some(RlmProtocolEvent::RlmDiagnostic(d)) if d.phase == "native_extraction" => {
                        Some(d.payload["decision"].clone())
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(decisions, vec![serde_json::json!(decision)]);
    }
}
#[test]
fn native_reasoning_only_is_provider_error() {
    let mut machine = TurnMachine::new(
        config(true, RlmTermination::Natural),
        Vec::new(),
        Arc::new(Vec::new()),
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
            .channel(crate::RlmChannel::NativeTool)
            .instruction_limit(crate::InstructionBound::instructions(1000))
            .wall_clock(crate::WallClockBound::secs(1))
            .memory_limit(crate::MemoryBound::mebibytes(1))
            .build(),
        lashlang::global_in_memory_lashlang_artifact_store(),
    )
    .with_process_lifecycle(false);
    let host = lash_core::facade_support::PluginHost::new(vec![Arc::new(factory)]);
    let session = host.build_session("native-plugin").unwrap();
    let catalog = session
        .resolved_tool_catalog(&SessionId::from("native-plugin"))
        .unwrap();
    let preamble = session
        .protocol_driver()
        .build_preamble(lash_core::ProtocolBuildInput {
            tool_catalog: catalog,
            plugin_extensions: Default::default(),
            trigger_events: Default::default(),
            extra_prompt_contributions: Vec::new(),
        });
    assert_eq!(preamble.tool_specs.len(), 1);
    assert_eq!(preamble.tool_specs[0].name, "execute_code");
    let schema = preamble.tool_specs[0].input_schema.canonical();
    assert_eq!(schema["required"], serde_json::json!(["code"]));
    assert_eq!(schema["additionalProperties"], false);
    assert!(!preamble.execution_prompt.contains("### Response shape"));
    assert!(!preamble.execution_prompt.contains("<lashlang>"));
    let response = LlmResponse {
        parts: vec![call("native-id", "execute_code", r#"{"code":"finish 1"}"#)],
        ..Default::default()
    };
    let transforms = session
        .transform_assistant_response(&SessionId::from("native-plugin"), response)
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
    assert_eq!(names, ["rlm_lashlang_cell_start", "rlm_lashlang_cell_end"]);
}

#[test]
fn multiple_calls_spend_one_stall_attempt_and_answer_every_id() {
    let mut machine = TurnMachine::new(
        config(true, RlmTermination::Natural),
        Vec::new(),
        Arc::new(Vec::new()),
        0,
    );
    let mut effects = drain(&mut machine);
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
        } else {
            assert!(
                effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::Done { .. }))
            );
        }
    }
    let mut pairs = Vec::new();
    for event in machine.events().iter() {
        if let lash_core::SessionHistoryRecord::Protocol(event) = event
            && let Some((parts, text)) = super::transport::repair_parts(event).unwrap()
        {
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
fn output_limit_prose_repairs_on_both_plugins() {
    for native in [false, true] {
        let mut machine = TurnMachine::new(
            config(native, RlmTermination::Natural),
            Vec::new(),
            Arc::new(Vec::new()),
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
            "native={native}: truncated prose must repair, not finish"
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
        machine =
            TurnMachine::restore_from_checkpoint(config(native, RlmTermination::Natural), saved)
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
            if native {
                "native_output_limit_prose"
            } else {
                "cell_output_limit_prose"
            },
            serde_json::to_string_pretty(&request.messages).unwrap()
        );
    }
}

#[test]
fn output_limit_calls_repair_without_execution_until_stall_budget() {
    for arguments in [r#"{"code":"finish 1"}"#, r#"{"code":"finish"#] {
        let mut machine = TurnMachine::new(
            config(true, RlmTermination::Natural),
            Vec::new(),
            Arc::new(Vec::new()),
            0,
        );
        let mut effects = drain(&mut machine);
        for attempt in 1..=3 {
            effects = reply_with_reason(
                &mut machine,
                &effects,
                vec![call("truncated", "execute_code", arguments)],
                lash_core::LlmTerminalReason::OutputLimit,
            );
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
            machine =
                TurnMachine::restore_from_checkpoint(config(true, RlmTermination::Natural), saved)
                    .expect("supported checkpoint");
            drain(&mut machine);
            machine.handle_response(Response::Checkpoint {
                id,
                delivery: Default::default(),
            });
            effects = drain(&mut machine);
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
            assert!(blocks.iter().any(|block| matches!(block, lash_core::llm::types::LlmContentBlock::ToolResult { call_id, content, .. } if call_id == "truncated" && content.contains("output limit"))));
        }
        let decisions = machine
            .events()
            .iter()
            .filter_map(|record| match record {
                lash_core::SessionHistoryRecord::Protocol(event) => {
                    match crate::projection::decode_rlm_protocol_event(event) {
                        Some(RlmProtocolEvent::RlmDiagnostic(d))
                            if d.phase == "native_extraction" =>
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
fn configured_prompt_is_instructions_on_both_channels() {
    for native in [false, true] {
        for (prompt, expected) in [
            ("  configured prompt\n", Some("configured prompt")),
            ("", None),
            (" \n\t", None),
        ] {
            let mut config = config(native, RlmTermination::Natural);
            config.system_prompt = Arc::from(prompt);
            let mut machine = TurnMachine::new(config, Vec::new(), Arc::new(Vec::new()), 0);
            let effects = drain(&mut machine);
            let request = effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::LlmCall { request, .. } => Some(request),
                    _ => None,
                })
                .expect("initial provider request");
            assert_eq!(request.instructions.as_deref(), expected, "native={native}");
            assert!(
                request
                    .messages
                    .iter()
                    .all(|message| message.role != lash_core::llm::types::LlmRole::System),
                "native={native}: configured prompt must not enter conversation history"
            );
        }
    }
}

#[test]
fn multipart_response_preserves_executable_cell() {
    let mut machine = TurnMachine::new(
        typescript_cell_config(RlmTermination::Natural),
        Vec::new(),
        Arc::new(Vec::new()),
        0,
    );
    let initial = drain(&mut machine);

    let effects = reply(
        &mut machine,
        &initial,
        vec![
            phased_text(
                "commentary",
                "Creating the artifact.\n<typescript>\nfinish(\"created\");\n</typescript>",
            ),
            phased_text("final_answer", "The artifact is ready."),
        ],
    );

    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::ExecCode { language, code, .. }
            if language == "typescript" && code.trim() == "finish(\"created\");"
    )));
}

#[test]
fn multipart_response_preserves_lashlang_executable_cell() {
    let mut machine = TurnMachine::new(
        lashlang_cell_config(RlmTermination::Natural),
        Vec::new(),
        Arc::new(Vec::new()),
        0,
    );
    let initial = drain(&mut machine);

    let effects = reply(
        &mut machine,
        &initial,
        vec![
            phased_text(
                "commentary",
                "Creating the artifact.\n<lashlang>\nfinish \"created\"\n</lashlang>",
            ),
            phased_text("final_answer", "The artifact is ready."),
        ],
    );

    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::ExecCode { language, code, .. }
            if language == "lashlang" && code.trim() == "finish \"created\""
    )));
}

#[test]
fn commentary_only_cell_still_executes() {
    let mut machine = TurnMachine::new(
        typescript_cell_config(RlmTermination::Natural),
        Vec::new(),
        Arc::new(Vec::new()),
        0,
    );
    let initial = drain(&mut machine);

    let effects = reply(
        &mut machine,
        &initial,
        vec![phased_text(
            "commentary",
            "Creating the artifact.\n<typescript>\nfinish(\"created\");\n</typescript>",
        )],
    );

    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::ExecCode { language, code, .. }
            if language == "typescript" && code.trim() == "finish(\"created\");"
    )));
}

#[test]
fn no_cell_multipart_response_finishes_with_final_answer_prose() {
    let mut machine = TurnMachine::new(
        typescript_cell_config(RlmTermination::Natural),
        Vec::new(),
        Arc::new(Vec::new()),
        0,
    );
    let initial = drain(&mut machine);

    let effects = reply(
        &mut machine,
        &initial,
        vec![
            phased_text("commentary", "Internal progress."),
            phased_text("final_answer", "Visible answer."),
        ],
    );

    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::ExecCode { .. }))
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(lash_core::session_model::SessionStreamEvent::LlmResponse { content, .. })
            if content == "Visible answer."
    )));

    let checkpoint_id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Checkpoint { id, .. } => Some(*id),
            _ => None,
        })
        .expect("prose-only response reaches completion checkpoint");
    machine.handle_response(Response::Checkpoint {
        id: checkpoint_id,
        delivery: Default::default(),
    });
    let completed = drain(&mut machine);
    assert!(completed.iter().any(|effect| matches!(
        effect,
        Effect::Emit(lash_core::session_model::SessionStreamEvent::TurnOutcome {
            outcome: lash_core::facade_support::TurnOutcome::Finished(
                lash_core::facade_support::TurnFinish::AssistantMessage { text }
            )
        }) if text == "Visible answer."
    )));
}

#[test]
fn markdown_fenced_finish_requests_an_explicit_no_execution_repair() {
    for dialect in [
        Arc::new(crate::dialect::typescript_test_dialect()) as Arc<dyn crate::dialect::RlmDialect>,
        Arc::new(crate::dialect::lashlang_test_dialect()),
    ] {
        for schema in [None, Some(serde_json::json!({"type": "number"}))] {
            let mut config = config(
                false,
                RlmTermination::FinishRequired {
                    schema: schema.clone(),
                },
            );
            config.protocol_driver = Arc::new(crate::protocol::RlmDriver::with_dialect(
                Arc::clone(&dialect),
            ));
            let mut machine = TurnMachine::new(config, Vec::new(), Arc::new(Vec::new()), 0);
            let initial = drain(&mut machine);
            let effects = reply(
                &mut machine,
                &initial,
                vec![text("```typescript\nfinish(1)\n```")],
            );
            assert!(
                !effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::ExecCode { .. }))
            );
            let events = serde_json::to_string(&machine.events()).unwrap();
            assert!(events.contains("request_finish"), "{events}");
            // The repair is durably appended before the continuation checkpoint.
            let continuation = events;
            assert!(
                continuation.contains(
                    "No code from that response executed. Markdown code fences do not execute here."
                ),
                "{continuation}"
            );
            let tags = dialect.cell_tags();
            assert!(continuation.contains(&format!("Resend the needed program between `{}` and `{}` on their own lines, without backticks.", tags.open, tags.close)), "{continuation}");
            if schema.is_some() {
                assert!(
                    continuation.contains("matching the required output schema"),
                    "{continuation}"
                );
            }
        }
    }
}
