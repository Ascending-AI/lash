use lash_core::sansio::{ChatContextProjector, ProtocolDriverHandle, Response};
use lash_core::{Effect, LlmOutputPart, LlmResponse, TurnMachine, TurnMachineConfig};
use lash_rlm_types::{RlmProtocolEvent, RlmTermination, RlmTurnOptions};
use std::sync::Arc;

fn config(native: bool, termination: RlmTermination) -> TurnMachineConfig {
    let dialect: Arc<dyn crate::dialect::RlmDialect> =
        Arc::new(crate::dialect::LashlangDialect::prompt_only(
            lash_lashlang_runtime::LashlangSurface::default(),
        ));
    let driver: Arc<dyn ProtocolDriverHandle<lash_core::HostTurnProtocol>> = if native {
        Arc::new(super::driver::NativeDriver::with_dialect(dialect))
    } else {
        Arc::new(crate::protocol::RlmDriver::with_dialect(dialect))
    };
    TurnMachineConfig {
        protocol_driver: driver,
        projector: Arc::new(ChatContextProjector),
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
        session_id: "parity".to_string(),
        turn_id: "parity-turn".to_string(),
        emit_llm_trace: false,
        termination: lash_core::ProtocolTurnOptions::typed(RlmTurnOptions {
            termination: Some(termination),
            final_answer_format: None,
        })
        .unwrap(),
        turn_limit_final_message: Arc::new(|id, _| lash_core::Message {
            id,
            role: lash_core::MessageRole::System,
            parts: Vec::new().into(),
            origin: None,
        }),
    }
}
fn text(text: &str) -> LlmOutputPart {
    LlmOutputPart::Text {
        text: text.to_string(),
        response_meta: None,
    }
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
        machine = TurnMachine::restore_from_checkpoint(config(native, termination.clone()), saved);
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
        machine.handle_response(Response::Checkpoint {
            id,
            delivery: lash_sansio::CheckpointDelivery::default(),
        });
        effects = drain(&mut machine);
    }
    let trajectory = machine
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
            Effect::LlmCall { .. } => checkpoints.push(serde_json::json!("request_next")),
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
            .instruction_limit(crate::InstructionBound::instructions(1000))
            .wall_clock(crate::WallClockBound::secs(1))
            .memory_limit(crate::MemoryBound::mebibytes(1))
            .build()
            .with_channel(crate::RlmChannel::NativeTool),
        lashlang::global_in_memory_lashlang_artifact_store(),
    )
    .with_process_lifecycle(false);
    let host = lash_core::facade_support::PluginHost::new(vec![Arc::new(factory)]);
    let session = host.build_session("native-plugin").unwrap();
    let catalog = session.resolved_tool_catalog("native-plugin").unwrap();
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
        .transform_assistant_response("native-plugin", response)
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
            && let Some((parts, text)) = super::transport::repair_parts(event)
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
