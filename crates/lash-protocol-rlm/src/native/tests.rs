use lash_core::plugin::PluginSessionRequest;
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
            .memory_limit(crate::MemoryBound::mebibytes(1))
            .build(),
        std::sync::Arc::new(crate::TypescriptDialect),
        &crate::testing::sqlite_recording_backend_blocking().clone(),
    )
    .with_process_lifecycle(false);
    let host = lash_core::facade_support::PluginHost::new(vec![Arc::new(factory)]);
    let session = host
        .build_session(PluginSessionRequest::creation("parity", Default::default()))
        .unwrap();
    let preamble = session
        .protocol_driver()
        .build_preamble(lash_core::ProtocolBuildInput {
            tool_catalog: session.resolved_tool_catalog().unwrap(),
            plugin_extensions: Default::default(),
            trigger_events: Default::default(),
            writer_formats: lash_core::build_newest_writer_formats(),
        });
    TurnMachineConfig {
        model_tool_calls: lash_core::sansio::ModelToolCalls::fixture(),
        protocol_driver: preamble.config.protocol,
        projector: preamble.config.projector,
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::builder("scripted".to_string())
                    .context_window_tokens(128_000)
                    .capability(Default::default())
                    .extra_body(Default::default())
                    .request_defaults(Default::default())
                    .build()
                    .expect("valid profile"),
            ),
        )
        .with_reasoning(Default::default()),
        turn_budget: lash_core::TurnBudget::bounded(4),
        no_progress_budget: lash_core::NoProgressBudget::bounded(3),
        attachment_acceptance: Default::default(),
        generation: Default::default(),
        autonomous: false,
        session_id: SessionId::from("parity"),
        agent_frame_id: "parity-frame".to_string(),
        turn_id: TurnId::from("parity-turn"),
        emit_llm_trace: false,
        writer_formats: lash_core::build_newest_writer_formats(),
        termination: crate::plugin::RlmRecordedConfig::for_testing(RlmTurnOptions {
            termination: Some(termination),
            final_answer_format: None,
            render: None,
        }),
    }
}

#[test]
fn rlm_catalog_distinguishes_ambient_from_restricted_empty_access() {
    let build = |session_id: &str, tool_access: lash_core::SessionToolAccess| {
        let factory = crate::RlmProtocolPluginFactory::new(
            crate::RlmProtocolPluginConfig::builder()
                .channel(crate::RlmChannel::Cell)
                .instruction_limit(crate::InstructionBound::instructions(1000))
                .memory_limit(crate::MemoryBound::mebibytes(1))
                .build(),
            std::sync::Arc::new(crate::TypescriptDialect),
            &crate::testing::sqlite_recording_backend_blocking().clone(),
        )
        .with_process_lifecycle(false);
        lash_core::facade_support::PluginHost::new(vec![Arc::new(factory)])
            .build_session(PluginSessionRequest::creation(
                lash_core::SessionId::fixture(session_id),
                lash_core::plugin::SessionAuthorityContext {
                    tool_access,
                    ..Default::default()
                },
            ))
            .expect("RLM protocol session")
    };

    let ambient = build("rlm-ambient", lash_core::SessionToolAccess::ambient());
    assert!(
        ambient
            .resolved_tool_catalog()
            .expect("ambient RLM catalog")
            .has_callable_tool("continue_as")
    );

    let restricted = build(
        "rlm-restricted-empty",
        lash_core::SessionToolAccess::restricted([]).expect("restricted empty is valid"),
    );
    let catalog = restricted
        .resolved_tool_catalog()
        .expect("restricted-empty RLM catalog");
    assert!(catalog.tools.is_empty());
    assert!(
        crate::tool_catalog::rlm_prompt_tool_docs(
            &catalog,
            &crate::dialect::typescript_test_dialect(),
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
            phase: Some(
                lash_core::llm::types::ResponsePhase::from_provider_wire(phase)
                    .expect("test phase vocabulary"),
            ),
            ..Default::default()
        }),
    }
}
fn typescript_cell_config(termination: RlmTermination) -> TurnMachineConfig {
    let mut config = config(false, termination);
    config.protocol_driver = Arc::new(crate::protocol::RlmDriver::new(Arc::new(
        crate::dialect::TypescriptDialect,
    )));
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
/// Every ready effect, with each execution-environment sync answered by an
/// empty environment on the way.
fn drain(machine: &mut TurnMachine) -> Vec<Effect> {
    let mut effects = Vec::new();
    while let Some(effect) = machine.poll_effect() {
        if let Effect::SyncExecutionEnvironment { id } = effect {
            machine.handle_response(lash_core::sansio::Response::ExecutionEnvironmentSynced {
                id,
                result: Ok(lash_core::sansio::ExecutionEnvironmentSync::default()),
            });
            continue;
        }
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
    exec: Option<Result<lash_core::ExecResponse, lash_core::ExecCodeFailure>>,
) -> (
    Vec<serde_json::Value>,
    Vec<lash_rlm_types::RlmTrajectoryEntry>,
) {
    let mut machine = TurnMachine::new(
        config(native, termination.clone()),
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
        None if native => vec![call(
            "provider-id",
            "execute_code",
            r#"{"code":"finish(1);"}"#,
        )],
        None => vec![text("<typescript>\nfinish(1);\n</typescript>")],
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
        machine =
            TurnMachine::restore_from_checkpoint(config(native, termination.clone()), saved, None)
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
        machine =
            TurnMachine::restore_from_checkpoint(config(native, termination.clone()), saved, None)
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
            match crate::projection::decode_rlm_protocol_event(event)
                .expect("valid history fixture")
            {
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
                        RlmTermination::Natural { schema: None } => "natural",
                        RlmTermination::Natural { schema: Some(_) } => "natural_schema",
                        RlmTermination::FinishRequired { schema: None } => "finish",
                        RlmTermination::FinishRequired { schema: Some(_) } => "schema",
                    },
                    if prose.is_some() {
                        "prose"
                    } else if trajectory
                        .iter()
                        .any(|step: &lash_rlm_types::RlmTrajectoryEntry| {
                            step.outcome.is_failed()
                        })
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
        output_archive: None,
        observations: Vec::new(),
        calls: Vec::new(),
        printed_images: Vec::new(),
        error: None,
        degraded_bindings: Vec::new(),
        terminal_finish: finish,
        terminal_finish_retained: None,
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
    let mut turn_config = config(native, RlmTermination::Natural { schema: None });
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
        vec![text(
            "<typescript>\nprint(\"cell-allowed\");\n</typescript>",
        )],
        vec![text(
            "<typescript>\nprint(\"cell-forbidden-iteration-one\");\n</typescript>",
        )],
        r#"print("cell-allowed");"#,
        "cell-forbidden-iteration-one",
    );
}

fn assert_simultaneous_turn_and_no_progress_exhaustion_prefers_silent_turn_stop(native: bool) {
    let mut turn_config = config(native, RlmTermination::FinishRequired { schema: None });
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
        RlmTermination::Natural { schema: None },
        natural_text_schema(),
        RlmTermination::FinishRequired { schema: None },
        RlmTermination::FinishRequired {
            schema: Some(
                lash_sansio::JsonSchema::admit(serde_json::json!({"type":"string"}))
                    .expect("valid finish schema"),
            ),
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
            Err(lash_core::ExecCodeFailure::new(
                lash_core::ExecCodeFailureReason::RuntimeStopped,
                "runtime error",
            )),
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
            config(true, RlmTermination::Natural { schema: None }),
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
                {
                    Some(RlmProtocolEvent::RlmDiagnostic(d)) if d.phase == "native_extraction" => {
                        Some(d.payload["decision"].clone())
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(decisions, vec![serde_json::json!(decision)]);
    }
    let mut machine = TurnMachine::new(
        config(true, RlmTermination::Natural { schema: None }),
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
        config(true, RlmTermination::Natural { schema: None }),
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
                lash_core::facade_support::TurnStop::ProviderError
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
            .channel(crate::RlmChannel::NativeTool)
            .instruction_limit(crate::InstructionBound::instructions(1000))
            .memory_limit(crate::MemoryBound::mebibytes(1))
            .build(),
        std::sync::Arc::new(crate::TypescriptDialect),
        &crate::testing::sqlite_recording_backend().await,
    )
    .with_process_lifecycle(false);
    let host = lash_core::facade_support::PluginHost::new(vec![Arc::new(factory)]);
    let session = host
        .build_session(PluginSessionRequest::creation(
            "native-plugin",
            Default::default(),
        ))
        .unwrap();
    let catalog = session.resolved_tool_catalog().unwrap();
    let preamble = session
        .protocol_driver()
        .build_preamble(lash_core::ProtocolBuildInput {
            tool_catalog: catalog,
            plugin_extensions: Default::default(),
            trigger_events: Default::default(),
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
            &SessionId::from("native-plugin"),
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
        config(true, RlmTermination::Natural { schema: None }),
        Vec::new(),
        Default::default(),
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
            config(native, RlmTermination::Natural { schema: None }),
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
        machine = TurnMachine::restore_from_checkpoint(
            config(native, RlmTermination::Natural { schema: None }),
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
    for arguments in [r#"{"code":"finish(1);"}"#, r#"{"code":"finish"#] {
        let mut machine = TurnMachine::new(
            config(true, RlmTermination::Natural { schema: None }),
            Vec::new(),
            Default::default(),
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
            machine = TurnMachine::restore_from_checkpoint(
                config(true, RlmTermination::Natural { schema: None }),
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
            assert!(blocks.iter().any(|block| matches!(block, lash_core::llm::types::LlmContentBlock::ToolResult { call_id, content, .. } if call_id == "truncated" && lash_core::facade_support::tool_result_text(content).contains("output limit"))));
        }
        let decisions = machine
            .events()
            .iter()
            .filter_map(|record| match record {
                lash_core::SessionHistoryRecord::Protocol(event) => {
                    match crate::projection::decode_rlm_protocol_event(event)
                        .expect("valid history fixture")
                    {
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

/// FIG-2777: a provider tool call on the cell channel — whose request declares
/// no tools — is malformed provider output, repaired like a reply with no
/// usable cell, not a terminal runtime error. The chunk is glm-5.3-flash's
/// captured shape: the tool name is a stray `lashlang</arg_value>` and the
/// arguments hold the lashlang source.
#[test]
fn cell_channel_tool_call_on_a_tool_less_request_repairs_then_stops_on_budget() {
    let stray_tool_call = || {
        vec![call(
            "call_stray",
            "lashlang</arg_value>",
            r#"{"p":"await retail.customer(...)?"}"#,
        )]
    };
    let mut machine = TurnMachine::new(
        config(false, RlmTermination::Natural { schema: None }),
        Vec::new(),
        Default::default(),
        0,
    );
    let mut effects = drain(&mut machine);
    let request = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::LlmCall { request, .. } => Some(request),
            _ => None,
        })
        .expect("initial provider request");
    assert!(
        request.tools.is_empty(),
        "the cell channel declares no tools: {request:?}"
    );

    // Attempts 1 and 2 repair; attempt 3 exhausts the stall budget.
    for attempt in 1..=3 {
        effects = reply(&mut machine, &effects, stray_tool_call());
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::ExecCode { .. })),
            "a stray tool call must never reach execution"
        );
        let outcome = effects.iter().find_map(|effect| match effect {
            Effect::Emit(lash_core::session_model::SessionStreamEvent::TurnOutcome { outcome }) => {
                Some(outcome)
            }
            _ => None,
        });
        if attempt < 3 {
            assert!(
                outcome.is_none(),
                "attempt {attempt}: a repairable extraction failure must not finish the turn"
            );
        } else {
            assert!(
                matches!(
                    outcome,
                    Some(lash_core::facade_support::TurnOutcome::Stopped(
                        lash_core::facade_support::TurnStop::MaxTurns
                    ))
                ),
                "the typed failure lands only when the repair budget is exhausted: {outcome:?}"
            );
        }
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
            .expect("a repair round checkpoints before the next request");
        let saved =
            serde_json::from_str(&serde_json::to_string(&machine.checkpoint()).unwrap()).unwrap();
        machine = TurnMachine::restore_from_checkpoint(
            config(false, RlmTermination::Natural { schema: None }),
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
        let request = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::LlmCall { request, .. } => Some(request),
                _ => None,
            })
            .expect("a repair round issues another provider request");
        let repair_text = request
            .messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                lash_core::llm::types::LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            repair_text.contains("lashlang</arg_value>"),
            "the repair copy names the stray call: {repair_text}"
        );
        assert!(
            repair_text.contains("paired `<typescript>...</typescript>` block"),
            "the standard paired-block diagnostic rides the repair: {repair_text}"
        );
    }

    let decisions = machine
        .events()
        .iter()
        .filter_map(|record| match record {
            lash_core::SessionHistoryRecord::Protocol(event) => {
                match crate::projection::decode_rlm_protocol_event(event)
                    .expect("valid history fixture")
                {
                    Some(RlmProtocolEvent::RlmDiagnostic(d)) if d.phase == "llm_extraction" => {
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
        vec![serde_json::json!("retry_native_tool_call"); 3]
    );
}

/// The same stray call repaired once lets the turn finish normally when the
/// model's next reply carries a real cell.
#[test]
fn cell_channel_tool_call_repair_lets_the_next_cell_finish() {
    let mut machine = TurnMachine::new(
        typescript_cell_config(RlmTermination::Natural { schema: None }),
        Vec::new(),
        Default::default(),
        0,
    );
    let initial = drain(&mut machine);
    let mut effects = reply(
        &mut machine,
        &initial,
        vec![call(
            "call_stray",
            "native_lookup",
            r#"{"query":"forbidden"}"#,
        )],
    );
    let id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Checkpoint { id, .. } => Some(*id),
            _ => None,
        })
        .expect("a repair round checkpoints before the next request");
    machine.handle_response(Response::Checkpoint {
        id,
        delivery: Default::default(),
    });
    effects = drain(&mut machine);
    effects = reply(
        &mut machine,
        &effects,
        vec![text("<typescript>\nfinish(1);\n</typescript>")],
    );
    let id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::ExecCode { id, .. } => Some(*id),
            _ => None,
        })
        .expect("the repaired turn executes the next cell");
    machine.handle_response(Response::ExecResult {
        id,
        result: Ok(response(Some(serde_json::json!(1)))),
    });
    effects = drain(&mut machine);
    for _ in 0..4 {
        let Some(id) = effects.iter().find_map(|effect| match effect {
            Effect::Checkpoint { id, .. } => Some(*id),
            _ => None,
        }) else {
            break;
        };
        machine.handle_response(Response::Checkpoint {
            id,
            delivery: Default::default(),
        });
        effects = drain(&mut machine);
    }
    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::Emit(lash_core::session_model::SessionStreamEvent::TurnOutcome {
                outcome: lash_core::facade_support::TurnOutcome::Finished(
                    lash_core::facade_support::TurnFinish::FinalValue { .. }
                )
            })
        )),
        "a repaired stray call lets the next cell settle the turn: {effects:?}"
    );
}

#[test]
fn configured_prompt_is_instructions_on_both_channels() {
    for native in [false, true] {
        for (prompt, expected) in [("configured prompt", Some("configured prompt")), ("", None)] {
            let mut machine = TurnMachine::new(
                config(native, RlmTermination::Natural { schema: None }),
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
            assert_eq!(
                projected.instructions, None,
                "native={native}: history only"
            );
            // The call's admission lowers its composed prompt onto the
            // projected request.
            let mut request = (**projected).clone();
            lash_core::sansio::place_prompt(
                &mut request,
                (!prompt.is_empty()).then(|| Arc::from(prompt)),
                None,
                true,
            );
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
        typescript_cell_config(RlmTermination::Natural { schema: None }),
        Vec::new(),
        Default::default(),
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
fn no_cell_multipart_response_finishes_with_final_answer_prose() {
    let mut machine = TurnMachine::new(
        typescript_cell_config(RlmTermination::Natural { schema: None }),
        Vec::new(),
        Default::default(),
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
        Arc::new(crate::dialect::typescript_test_dialect()) as Arc<crate::dialect::SessionDialect>,
        Arc::new(crate::dialect::typescript_test_dialect()),
    ] {
        for schema in [None, Some(serde_json::json!({"type": "number"}))] {
            let mut config = config(
                false,
                RlmTermination::FinishRequired {
                    schema: schema
                        .clone()
                        .map(lash_sansio::JsonSchema::admit)
                        .transpose()
                        .expect("valid finish schema"),
                },
            );
            config.protocol_driver = Arc::new(crate::protocol::RlmDriver::with_dialect(
                Arc::clone(&dialect),
            ));
            let mut machine = TurnMachine::new(config, Vec::new(), Default::default(), 0);
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
            {
                Some(RlmProtocolEvent::RlmDiagnostic(d)) if d.phase == "native_extraction" => {
                    Some(d.payload)
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
            config(true, RlmTermination::Natural { schema: None }),
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
            config(true, RlmTermination::Natural { schema: None }),
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
        if restore {
            let checkpoint =
                serde_json::from_slice(&serde_json::to_vec(&machine.checkpoint()).unwrap())
                    .unwrap();
            machine = TurnMachine::restore_from_checkpoint(
                config(true, RlmTermination::Natural { schema: None }),
                checkpoint,
                None,
            )
            .unwrap();
            effects = drain(&mut machine);
        }
        let id = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::ExecCode { id, .. } => Some(*id),
                _ => None,
            })
            .unwrap();
        machine.record_cancellation_evidence(lash_sansio::TurnCancellationEvidence {
            request_id: "user-stop".into(),
            origin: Some("host".into()),
            reason: Some("Stop".into()),
            undelivered: lash_sansio::TurnCancelUndeliveredInputPolicy::Defer,
            mode: lash_sansio::TurnCancelMode::Immediate,
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
        assert!(!machine.events().iter().any(|event| matches!(event, lash_core::SessionHistoryRecord::Protocol(event) if matches!(crate::projection::decode_rlm_protocol_event(event).expect("valid history fixture"), Some(RlmProtocolEvent::RlmTrajectoryEntry(_))))));
    }
}

#[test]
fn a_step_archive_survives_both_driver_checkpoint_paths() {
    let archive = lash_core::RetainedOutput {
        reference: lash_core::AttachmentRef {
            id: "aggregate-prints".parse().expect("attachment id"),
            media_type: "application/json".parse().expect("media type"),
            byte_len: 100_000,
            type_metadata: None,
            label: None,
        },
        witness: "bounded preview".into(),
    };
    for native in [false, true] {
        let mut exec = response(Some(serde_json::json!(1)));
        exec.output_archive = Some(archive.clone());
        let (_, steps) = run(
            native,
            RlmTermination::Natural { schema: None },
            None,
            Some(Ok(exec)),
        );
        let [step] = steps.as_slice() else {
            panic!("one trajectory step: {steps:?}");
        };
        assert!(step.output.is_empty());
        assert_eq!(step.output_archive.as_deref(), Some(&archive));
    }
}

/// L19: a restored pending execution accounts its recorded tool answer once,
/// keeps the full terminal value, and spends no additional model usage.
#[test]
fn a_recorded_tool_terminal_keeps_its_payload_and_usage_across_both_checkpoints() {
    let payload =
        serde_json::json!({"terminal": "x".repeat(80_000), "nested": [1, {"complete": true}]});
    for native in [false, true] {
        let mut machine = TurnMachine::new(
            config(native, RlmTermination::Natural { schema: None }),
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
        let parts = if native {
            vec![call(
                "terminal",
                "execute_code",
                r#"{"code":"await tools.app_lookup({});"}"#,
            )]
        } else {
            vec![text(
                "<typescript>\nawait tools.app_lookup({});\n</typescript>",
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
            config(native, RlmTermination::Natural { schema: None }),
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
            operation: "tools.app_lookup".into(),
            outcome: lash_core::ExecutedCallOutcome::Ok,
            host_record: Some(lash_core::ToolCallRecord {
                call_id: lash_core::ToolCallId::fixture("terminal-call"),
                provider_call_id: None,
                tool: "app_lookup".into(),
                args: serde_json::json!({}),
                output: lash_core::ToolCallOutput::success(payload.clone()).with_control(
                    lash_core::ToolControl::Finish {
                        value: lash_core::ToolValue::untrusted_json(payload.clone()),
                    },
                ),
            }),
        });
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
        let output = serde_json::to_string(outputs[0]).unwrap();
        assert!(output.contains("omitted_bytes"));
        assert!(!output.contains(&"x".repeat(80_000)));
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
            config(native, RlmTermination::Natural { schema: None }),
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
                lash_core::facade_support::TurnFinish::ToolValue {
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

/// A chat turn's termination: prose ends it, and `finish` must carry text.
fn natural_text_schema() -> RlmTermination {
    RlmTermination::Natural {
        schema: Some(
            lash_sansio::JsonSchema::admit(serde_json::json!({"type": "string"}))
                .expect("valid finish schema"),
        ),
    }
}

/// Replies to the pending model call with one program that finishes, answers
/// its execution with `value` as the terminal finish, and settles every
/// checkpoint that follows.
fn finish_with(
    machine: &mut TurnMachine,
    pending: &[Effect],
    native: bool,
    code: &str,
    value: serde_json::Value,
) -> Vec<Effect> {
    let parts = if native {
        vec![call(
            "finish",
            "execute_code",
            &serde_json::json!({ "code": code }).to_string(),
        )]
    } else {
        vec![text(&format!("<typescript>\n{code}\n</typescript>"))]
    };
    let effects = reply(machine, pending, parts);
    let id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::ExecCode { id, .. } => Some(*id),
            _ => None,
        })
        .expect("the program executes");
    machine.handle_response(Response::ExecResult {
        id,
        result: Ok(response(Some(value))),
    });
    let mut effects = drain(machine);
    while let Some(id) = effects.iter().find_map(|effect| match effect {
        Effect::Checkpoint { id, .. } => Some(*id),
        _ => None,
    }) {
        machine.handle_response(Response::Checkpoint {
            id,
            delivery: Default::default(),
        });
        effects = drain(machine);
    }
    effects
}

fn turn_outcome(effects: &[Effect]) -> Option<&lash_core::facade_support::TurnOutcome> {
    effects.iter().find_map(|effect| match effect {
        Effect::Emit(lash_core::session_model::SessionStreamEvent::TurnOutcome { outcome }) => {
            Some(outcome)
        }
        _ => None,
    })
}

/// FIG-5104: a Natural turn with a text finish schema refuses
/// `finish(<tool record>)` as a program failure carrying the value mismatch,
/// asks the model to finish again with the mismatch copy, and then ends with
/// the text it finishes with. Both channels adjudicate it alike.
#[test]
fn a_natural_text_schema_refuses_a_record_finish_and_accepts_text() {
    let mismatch_copy = crate::dialect::typescript_test_dialect().finish_schema_mismatch_copy();
    for native in [false, true] {
        let mut machine = TurnMachine::new(
            config(native, natural_text_schema()),
            Vec::new(),
            Default::default(),
            0,
        );
        let initial = drain(&mut machine);
        let refused = finish_with(
            &mut machine,
            &initial,
            native,
            "finish(await tools.order_lookup({ id: 7 }));",
            serde_json::json!({ "id": 7, "status": "shipped" }),
        );
        assert_eq!(turn_outcome(&refused), None, "native={native}: {refused:?}");
        let retry = refused
            .iter()
            .find_map(|effect| match effect {
                Effect::LlmCall { request, .. } => Some(request),
                _ => None,
            })
            .unwrap_or_else(|| panic!("native={native}: the model is asked again: {refused:?}"));
        let rendered = serde_json::to_string(&retry.messages).expect("messages serialize");
        assert!(
            rendered.contains(&mismatch_copy),
            "native={native}: the retry carries the mismatch copy: {rendered}"
        );

        let finished = finish_with(
            &mut machine,
            &refused,
            native,
            r#"finish("Order 7 has shipped.");"#,
            serde_json::json!("Order 7 has shipped."),
        );
        assert_eq!(
            turn_outcome(&finished),
            Some(&lash_core::facade_support::TurnOutcome::Finished(
                lash_core::facade_support::TurnFinish::FinalValue {
                    value: serde_json::json!("Order 7 has shipped."),
                }
            )),
            "native={native}"
        );
        let steps: Vec<_> = machine
            .events()
            .iter()
            .filter_map(|record| {
                let lash_core::SessionHistoryRecord::Protocol(event) = record else {
                    return None;
                };
                match crate::projection::decode_rlm_protocol_event(event)
                    .expect("valid history fixture")
                {
                    Some(RlmProtocolEvent::RlmTrajectoryEntry(step)) => Some(step.outcome),
                    _ => None,
                }
            })
            .collect();
        let [refusal, accepted] = steps.as_slice() else {
            panic!("native={native}: two trajectory steps: {steps:?}");
        };
        let lash_rlm_types::CellOutcome::Failed(failure) = refusal else {
            panic!("native={native}: the record finish fails its cell: {refusal:?}");
        };
        assert_eq!(failure.kind, lash_core::CellFailureKind::Program);
        assert!(failure.value_mismatch.is_some(), "native={native}");
        assert_eq!(
            accepted,
            &lash_rlm_types::CellOutcome::Finished(lash_core::OutputValue::Inline(
                serde_json::json!("Order 7 has shipped.")
            )),
            "native={native}"
        );
    }
}

/// FIG-5104: a finish schema on a Natural turn leaves prose the answer. A
/// prose reply ends the turn exactly as it does with no schema, on both
/// channels.
#[test]
fn a_natural_finish_schema_still_lets_prose_end_the_turn() {
    for native in [false, true] {
        let (with_schema, _) = run(native, natural_text_schema(), Some("answer"), None);
        let (without, _) = run(
            native,
            RlmTermination::Natural { schema: None },
            Some("answer"),
            None,
        );
        assert_eq!(with_schema, without, "native={native}");
        assert!(
            with_schema.iter().any(|value| value
                == &serde_json::to_value(lash_core::facade_support::TurnOutcome::Finished(
                    lash_core::facade_support::TurnFinish::AssistantMessage {
                        text: "answer".to_string(),
                    }
                ))
                .expect("outcome serializes")),
            "native={native}: {with_schema:?}"
        );
    }
}
