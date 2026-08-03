use super::*;

pub(super) fn rlm_protocol_contract_executions() -> Result<Vec<Value>, FixedScriptRunnerError> {
    Ok(vec![
        rlm_natural_prose_finalizes_execution()?,
        rlm_typed_prose_requires_finish_execution()?,
        rlm_finish_required_max_turn_stop_execution()?,
        rlm_exec_error_max_turn_stop_execution()?,
        rlm_typed_finish_emits_outcome_and_done_execution()?,
        rlm_finish_required_diagnostic_counts_execution()?,
        rlm_natural_diagnostic_counts_execution()?,
        rlm_cell_diagnostic_counts_execution()?,
        rlm_retired_marker_plain_lashlang_text_execution()?,
        rlm_lashlang_cell_exec_continues_execution()?,
        rlm_streamed_lashlang_cell_exec_persists_trajectory_execution()?,
        rlm_empty_options_natural_default_execution()?,
        rlm_exec_result_no_tool_call_replay_execution()?,
        rlm_exec_tool_control_frame_switch_terminal_execution()?,
        rlm_exec_tool_control_fail_terminal_execution()?,
        rlm_natural_allows_finish_value_execution()?,
        rlm_typed_schema_mismatch_repair_loop_execution()?,
        rlm_typed_schema_any_of_mismatch_execution()?,
    ])
}

fn rlm_natural_prose_finalizes_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_rlm_protocol_contract(
        "rlm natural prose finalizes",
        "hello",
        RlmTermination::Natural,
        None,
        None,
        vec![
            RlmContractStep::Llm(vec![rlm_text_part("Hello there!")]),
            RlmContractStep::Checkpoint,
        ],
    )?;
    contract_execution_payload(
        "rlm.natural_prose_finalizes",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_prose_only_response_finishes_by_default",
        result,
    )
}

fn rlm_typed_prose_requires_finish_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_rlm_protocol_contract(
        "rlm typed prose requires finish",
        "hello",
        RlmTermination::FinishRequired { schema: None },
        None,
        None,
        vec![
            RlmContractStep::Llm(vec![rlm_text_part("Hello there!")]),
            RlmContractStep::Checkpoint,
        ],
    )?;
    contract_execution_payload(
        "rlm.typed_prose_requires_finish",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_typed_prose_only_response_requests_finish",
        result,
    )
}

fn rlm_finish_required_max_turn_stop_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_rlm_protocol_contract(
        "rlm finish-required prose max-turn stop",
        "hello",
        RlmTermination::FinishRequired { schema: None },
        Some(1),
        None,
        vec![RlmContractStep::Llm(vec![rlm_text_part(
            "plain prose cannot finish finish-required RLM",
        )])],
    )?;
    contract_execution_payload(
        "rlm.finish_required_max_turn_stop",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_finish_required_prose_at_max_turns_stops_without_retry_prompt",
        result,
    )
}

fn rlm_exec_error_max_turn_stop_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_rlm_protocol_contract(
        "rlm finish-required exec error max-turn stop",
        "run bad code",
        RlmTermination::FinishRequired { schema: None },
        Some(1),
        None,
        vec![
            RlmContractStep::Llm(vec![rlm_text_part(&rlm_lashlang_block("missing_name"))]),
            RlmContractStep::Exec(rlm_exec_response(
                &[],
                Some("unknown variable `missing_name`"),
                None,
            )),
        ],
    )?;
    contract_execution_payload(
        "rlm.exec_error_max_turn_stop",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_finish_required_exec_error_at_max_turns_stops_without_retry",
        result,
    )
}

fn rlm_typed_finish_emits_outcome_and_done_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_rlm_protocol_contract(
        "rlm typed finish emits outcome and done",
        "return typed data",
        RlmTermination::FinishRequired {
            schema: Some(json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" }
                },
                "required": ["ok"],
                "additionalProperties": false
            })),
        },
        None,
        None,
        vec![
            RlmContractStep::Llm(vec![rlm_text_part(&rlm_lashlang_block(
                "finish { ok: true }",
            ))]),
            RlmContractStep::Exec(rlm_exec_response(&[], None, Some(json!({ "ok": true })))),
            RlmContractStep::Checkpoint,
        ],
    )?;
    contract_execution_payload(
        "rlm.typed_finish_emits_outcome_and_done",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_typed_finish_emits_turn_outcome_and_done",
        result,
    )
}

fn rlm_finish_required_diagnostic_counts_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_rlm_protocol_contract(
        "rlm finish-required diagnostic counts",
        "hello",
        RlmTermination::FinishRequired { schema: None },
        None,
        None,
        vec![RlmContractStep::Llm(vec![rlm_text_part("Hello there!")])],
    )?;
    contract_execution_payload(
        "rlm.finish_required_diagnostic_counts",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_finish_required_prose_only_diagnostic_has_clean_counts",
        result,
    )
}

fn rlm_natural_diagnostic_counts_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_rlm_protocol_contract(
        "rlm natural diagnostic counts",
        "hello",
        RlmTermination::Natural,
        None,
        None,
        vec![RlmContractStep::Llm(vec![rlm_text_part("Hello there!")])],
    )?;
    contract_execution_payload(
        "rlm.natural_diagnostic_counts",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_natural_prose_only_diagnostic_has_clean_counts",
        result,
    )
}

fn rlm_cell_diagnostic_counts_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_rlm_protocol_contract(
        "rlm cell diagnostic counts",
        "run some code",
        RlmTermination::Natural,
        None,
        None,
        vec![
            RlmContractStep::Llm(vec![
                rlm_reasoning_part("Checking state."),
                rlm_text_part(&rlm_lashlang_block_with_prose("Ready.", "print \"hi\"")),
            ]),
            RlmContractStep::Exec(rlm_exec_response(&["hi\n"], None, None)),
        ],
    )?;
    contract_execution_payload(
        "rlm.cell_diagnostic_counts",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_cell_reasoning_prose_code_diagnostic_has_clean_counts",
        result,
    )
}

fn rlm_retired_marker_plain_lashlang_text_execution() -> Result<Value, FixedScriptRunnerError> {
    let assistant_prose = "First.";
    let code = "text = \"%%lashlang is just source here\"\nprint text";
    let result = run_rlm_protocol_contract(
        "rlm retired marker plain LashLang text",
        "run some code",
        RlmTermination::Natural,
        None,
        None,
        vec![RlmContractStep::Llm(vec![rlm_text_part(
            &rlm_lashlang_block_with_prose(assistant_prose, code),
        )])],
    )?;
    contract_execution_payload(
        "rlm.retired_marker_plain_lashlang_text",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_retired_percent_marker_inside_source_is_plain_lashlang_text",
        result,
    )
}

fn rlm_lashlang_cell_exec_continues_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_rlm_protocol_contract(
        "rlm LashLang cell exec continues",
        "run some code",
        RlmTermination::Natural,
        None,
        None,
        vec![
            RlmContractStep::Llm(vec![rlm_text_part(&rlm_lashlang_block_with_prose(
                "Quick check.\n",
                "print \"hi\"",
            ))]),
            RlmContractStep::Exec(rlm_exec_response(&["hi\n"], None, None)),
            RlmContractStep::Checkpoint,
        ],
    )?;
    contract_execution_payload(
        "rlm.lashlang_cell_exec_continues",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_lashlang_cell_runs_exec_and_continues",
        result,
    )
}

fn rlm_streamed_lashlang_cell_exec_persists_trajectory_execution()
-> Result<Value, FixedScriptRunnerError> {
    let result = run_rlm_protocol_contract(
        "rlm streamed LashLang cell exec persists trajectory",
        "stream and run some code",
        RlmTermination::Natural,
        None,
        None,
        vec![
            RlmContractStep::StreamedLlm(vec![rlm_text_part(&rlm_lashlang_block_with_prose(
                "Streaming check.\n",
                "print \"streamed\"",
            ))]),
            RlmContractStep::Exec(rlm_exec_response(&["streamed\n"], None, None)),
            RlmContractStep::Checkpoint,
        ],
    )?;
    contract_execution_payload(
        "rlm.streamed_lashlang_cell_exec_persists_trajectory",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_streamed_lashlang_cell_runs_exec_and_persists_trajectory",
        result,
    )
}

fn rlm_empty_options_natural_default_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_rlm_protocol_contract(
        "rlm empty options natural default",
        "finish",
        RlmTermination::Natural,
        None,
        Some(lash_core::ProtocolTurnOptions::empty()),
        vec![
            RlmContractStep::Llm(vec![rlm_text_part(&rlm_lashlang_block("finish \"done\""))]),
            RlmContractStep::Exec(rlm_exec_response(&[], None, Some(json!("done")))),
            RlmContractStep::Checkpoint,
        ],
    )?;
    contract_execution_payload(
        "rlm.empty_options_natural_default",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_empty_turn_options_use_natural_default",
        result,
    )
}

fn rlm_exec_result_no_tool_call_replay_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_rlm_protocol_contract(
        "rlm exec result no tool-call replay",
        "run a tool",
        RlmTermination::Natural,
        None,
        None,
        vec![
            RlmContractStep::Llm(vec![rlm_text_part(&rlm_lashlang_block(
                "x = await tools.read_file({ path: \"foo\" })?",
            ))]),
            RlmContractStep::Exec(rlm_exec_response_with_tool_calls(
                &[],
                None,
                None,
                vec![rlm_tool_call_record(
                    "rlm-call-1",
                    "read_file",
                    json!({ "path": "foo" }),
                    lash_core::ToolCallOutput::success(json!("contents")),
                    7,
                )],
                7,
            )),
        ],
    )?;
    contract_execution_payload(
        "rlm.exec_result_no_tool_call_replay",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_exec_result_emits_accounting_without_storing_tool_call_ids",
        result,
    )
}

fn rlm_exec_tool_control_frame_switch_terminal_execution() -> Result<Value, FixedScriptRunnerError>
{
    let initial_nodes = vec![lash_core::SessionAppendNode::message(
        lash_core::PluginMessage::text(lash_core::MessageRole::User, "seed"),
    )];
    let result = run_rlm_protocol_contract(
        "rlm exec tool-control frame switch terminal",
        "run a custom frame-switch tool",
        RlmTermination::Natural,
        None,
        None,
        vec![
            RlmContractStep::Llm(vec![rlm_text_part(&rlm_lashlang_block(
                "x = await tools.custom_frame_switch({})?",
            ))]),
            RlmContractStep::Exec(rlm_exec_response_with_tool_calls(
                &[],
                None,
                None,
                vec![rlm_tool_call_record(
                    "custom-call-1",
                    "custom_frame_switch",
                    json!({}),
                    lash_core::ToolCallOutput::success(json!({ "ok": true })).with_control(
                        lash_core::ToolControl::SwitchAgentFrame {
                            frame_id: "next-frame".to_string(),
                            initial_nodes,
                            task: Some("continue".to_string()),
                        },
                    ),
                    3,
                )],
                3,
            )),
            RlmContractStep::Checkpoint,
        ],
    )?;
    contract_execution_payload(
        "rlm.exec_tool_control_frame_switch_terminal",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_exec_any_tool_control_frame_switch_is_terminal",
        result,
    )
}

fn rlm_exec_tool_control_fail_terminal_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_rlm_protocol_contract(
        "rlm exec tool-control fail terminal",
        "run a custom failure tool",
        RlmTermination::Natural,
        None,
        None,
        vec![
            RlmContractStep::Llm(vec![rlm_text_part(&rlm_lashlang_block(
                "x = await tools.custom_fail({})?",
            ))]),
            RlmContractStep::Exec(rlm_exec_response_with_tool_calls(
                &[],
                None,
                None,
                vec![rlm_tool_call_record(
                    "custom-call-1",
                    "custom_fail",
                    json!({}),
                    lash_core::ToolCallOutput::success(json!({ "ok": true })).with_control(
                        lash_core::ToolControl::Fail {
                            failure: lash_core::ToolFailure::tool(
                                lash_core::ToolFailureClass::Execution,
                                "custom_fail",
                                "no valid result",
                            ),
                        },
                    ),
                    3,
                )],
                3,
            )),
            RlmContractStep::Checkpoint,
        ],
    )?;
    contract_execution_payload(
        "rlm.exec_tool_control_fail_terminal",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_exec_any_tool_control_fail_is_terminal_error",
        result,
    )
}

fn rlm_natural_allows_finish_value_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_rlm_protocol_contract(
        "rlm natural allows finish value",
        "return typed data",
        RlmTermination::Natural,
        None,
        None,
        vec![
            RlmContractStep::Llm(vec![rlm_text_part(&rlm_lashlang_block(
                "finish { ok: true }",
            ))]),
            RlmContractStep::Exec(rlm_exec_response(&[], None, Some(json!({ "ok": true })))),
            RlmContractStep::Checkpoint,
        ],
    )?;
    contract_execution_payload(
        "rlm.natural_allows_finish_value",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_natural_allows_finish_value",
        result,
    )
}

fn rlm_typed_schema_mismatch_repair_loop_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_rlm_protocol_contract(
        "rlm typed schema mismatch repair loop",
        "return typed data",
        RlmTermination::FinishRequired {
            schema: Some(json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" }
                },
                "required": ["ok"]
            })),
        },
        None,
        None,
        vec![
            RlmContractStep::Llm(vec![rlm_text_part(&rlm_lashlang_block(
                "finish { missing: true }",
            ))]),
            RlmContractStep::Exec(rlm_exec_response(
                &[],
                None,
                Some(json!({ "missing": true })),
            )),
            RlmContractStep::Checkpoint,
        ],
    )?;
    contract_execution_payload(
        "rlm.typed_schema_mismatch_repair_loop",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_typed_schema_mismatch_loops_with_feedback",
        result,
    )
}

fn rlm_typed_schema_any_of_mismatch_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_rlm_protocol_contract(
        "rlm typed schema anyOf mismatch",
        "return typed data",
        RlmTermination::FinishRequired {
            schema: Some(json!({
                "anyOf": [
                    { "type": "string" },
                    { "type": "integer" }
                ]
            })),
        },
        None,
        None,
        vec![
            RlmContractStep::Llm(vec![rlm_text_part(&rlm_lashlang_block("finish true"))]),
            RlmContractStep::Exec(rlm_exec_response(&[], None, Some(json!(true)))),
        ],
    )?;
    contract_execution_payload(
        "rlm.typed_schema_any_of_mismatch",
        "crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs",
        "rlm_protocol_scenario_typed_schema_mismatch_checks_any_of",
        result,
    )
}

#[derive(Clone)]
pub(super) enum RlmContractStep {
    Llm(Vec<LlmOutputPart>),
    StreamedLlm(Vec<LlmOutputPart>),
    Exec(lash_core::ExecResponse),
    Checkpoint,
}

#[derive(Default)]
struct RlmContractObserved {
    initial_request_tools_empty: Option<bool>,
    exec_codes: Vec<String>,
    checkpoints: Vec<&'static str>,
    llm_response_full_texts: Vec<String>,
    llm_response_part_counts: Vec<usize>,
    llm_response_parts: Vec<Vec<Value>>,
    llm_response_text_streamed: Vec<bool>,
    llm_call_count: usize,
    turn_outcomes: Vec<lash_core::facade_support::TurnOutcome>,
    final_message_event: bool,
    tool_call_event: bool,
    assistant_conversation_progress: bool,
}

impl RlmContractObserved {
    fn record(&mut self, effects: &[lash_core::Effect]) {
        for effect in effects {
            match effect {
                lash_core::Effect::LlmCall { request, .. } => {
                    if self.initial_request_tools_empty.is_none() {
                        self.initial_request_tools_empty = Some(request.tools.is_empty());
                    }
                    self.llm_call_count += 1;
                }
                lash_core::Effect::ExecCode { code, .. } => {
                    self.exec_codes.push(code.clone());
                }
                lash_core::Effect::Checkpoint { checkpoint, .. } => {
                    self.checkpoints.push(checkpoint_kind_name(*checkpoint));
                }
                lash_core::Effect::Emit(
                    lash_core::facade_support::SessionStreamEvent::TurnOutcome { outcome },
                ) => {
                    self.turn_outcomes.push(outcome.clone());
                }
                lash_core::Effect::Emit(
                    lash_core::facade_support::SessionStreamEvent::Message { kind, .. },
                ) if kind == "final" => {
                    self.final_message_event = true;
                }
                lash_core::Effect::Emit(
                    lash_core::facade_support::SessionStreamEvent::ToolCall { .. },
                ) => {
                    self.tool_call_event = true;
                }
                lash_core::Effect::Progress { event_delta, .. } => {
                    self.assistant_conversation_progress |= event_delta.iter().any(|event| {
                        matches!(
                            event,
                            lash_core::SessionHistoryRecord::Conversation(record)
                                if record.to_message().role == lash_core::MessageRole::Assistant
                        )
                    });
                }
                _ => {}
            }
        }
    }
}

pub(super) fn run_rlm_protocol_contract(
    scenario_name: &'static str,
    user_message: &'static str,
    termination: RlmTermination,
    max_turns: Option<usize>,
    protocol_turn_options: Option<lash_core::ProtocolTurnOptions>,
    steps: Vec<RlmContractStep>,
) -> Result<Value, FixedScriptRunnerError> {
    let termination_declared = if protocol_turn_options.is_some() {
        json!({ "kind": "empty_protocol_turn_options" })
    } else {
        serde_json::to_value(&termination)?
    };
    let mut config = match protocol_turn_options {
        Some(options) => rlm_contract_config_with_turn_options(options),
        None => rlm_contract_config(termination),
    }?;
    config.max_turns = max_turns;
    let mut machine = lash_core::TurnMachine::new(
        config,
        vec![contract_user_message(user_message)],
        Arc::new(Vec::new()),
        0,
    );
    let mut observed = RlmContractObserved::default();
    let mut effects = drain_rlm_contract_effects(&mut machine);
    observed.record(&effects);
    for step in steps {
        match step {
            step @ (RlmContractStep::Llm(_) | RlmContractStep::StreamedLlm(_)) => {
                let text_streamed = matches!(&step, RlmContractStep::StreamedLlm(_));
                let parts = match step {
                    RlmContractStep::Llm(parts) | RlmContractStep::StreamedLlm(parts) => parts,
                    RlmContractStep::Exec(_) | RlmContractStep::Checkpoint => unreachable!(),
                };
                let llm_id = *find_contract_llm_call(&effects).ok_or_else(|| {
                    FixedScriptRunnerError::Assertion(format!(
                        "{scenario_name} expected a pending LLM call"
                    ))
                })?;
                let expected_parts = parts.clone();
                let expected_full_text = rlm_full_text(&expected_parts);
                let expected_part_summary = llm_output_parts_contract_summary(&expected_parts);
                let response = llm_response_with_parts(expected_full_text.clone(), parts);
                require(
                    response.full_text == expected_full_text,
                    format!(
                        "{scenario_name} provider response full_text changed: expected {:?}, got {:?}",
                        expected_full_text, response.full_text
                    ),
                )?;
                require(
                    response.parts.len() == expected_parts.len() && !response.parts.is_empty(),
                    format!(
                        "{scenario_name} provider response parts changed: expected {} parts, got {}",
                        expected_parts.len(),
                        response.parts.len()
                    ),
                )?;
                let response_part_summary = llm_output_parts_contract_summary(&response.parts);
                require(
                    response_part_summary == expected_part_summary,
                    format!(
                        "{scenario_name} provider response parts changed: expected {:?}, got {:?}",
                        expected_part_summary, response_part_summary
                    ),
                )?;
                observed
                    .llm_response_full_texts
                    .push(response.full_text.clone());
                observed.llm_response_part_counts.push(response.parts.len());
                observed.llm_response_parts.push(response_part_summary);
                observed.llm_response_text_streamed.push(text_streamed);
                machine.handle_response(lash_core::sansio::Response::LlmComplete {
                    id: llm_id,
                    text_streamed,
                    result: Ok(response),
                });
            }
            RlmContractStep::Exec(result) => {
                let exec_id = effects
                    .iter()
                    .find_map(|effect| match effect {
                        lash_core::Effect::ExecCode { id, .. } => Some(*id),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        FixedScriptRunnerError::Assertion(format!(
                            "{scenario_name} expected pending exec code"
                        ))
                    })?;
                machine.handle_response(lash_core::sansio::Response::ExecResult {
                    id: exec_id,
                    result: Ok(result),
                });
            }
            RlmContractStep::Checkpoint => {
                let checkpoint_id = effects
                    .iter()
                    .find_map(|effect| match effect {
                        lash_core::Effect::Checkpoint { id, .. } => Some(*id),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        FixedScriptRunnerError::Assertion(format!(
                            "{scenario_name} expected pending checkpoint"
                        ))
                    })?;
                machine.handle_response(lash_core::sansio::Response::Checkpoint {
                    id: checkpoint_id,
                    delivery: lash_core::sansio::CheckpointDelivery::default(),
                });
            }
        }
        effects = drain_rlm_contract_effects(&mut machine);
        observed.record(&effects);
    }
    Ok(json!({
        "execution_api": "lash_core::sansio::TurnMachine",
        "driver": "lash_protocol_rlm::RlmDriver",
        "scenario_name": scenario_name,
        "user_message": user_message,
        "termination": termination_declared,
        "max_turns": max_turns,
        "initial_request_tools_empty": observed.initial_request_tools_empty,
        "llm_call_count": observed.llm_call_count,
        "llm_response_full_texts": observed.llm_response_full_texts,
        "llm_response_part_counts": observed.llm_response_part_counts,
        "llm_response_parts": observed.llm_response_parts,
        "llm_response_text_streamed": observed.llm_response_text_streamed,
        "done": machine.is_done(),
        "checkpoints": observed.checkpoints,
        "exec_codes": observed.exec_codes,
        "turn_outcomes": observed.turn_outcomes.iter().map(turn_outcome_contract_json).collect::<Vec<_>>(),
        "final_message_event": observed.final_message_event,
        "tool_call_event": observed.tool_call_event,
        "assistant_conversation_progress": observed.assistant_conversation_progress,
        "llm_extraction_diagnostics": rlm_contract_llm_extraction_diagnostics(&machine),
        "trajectory": rlm_contract_trajectory(&machine),
        "system_messages": rlm_contract_system_messages(&machine),
    }))
}

fn rlm_contract_config(
    termination: RlmTermination,
) -> Result<lash_core::TurnMachineConfig, FixedScriptRunnerError> {
    let options = lash_core::ProtocolTurnOptions::typed(RlmCreateExtras {
        termination,
        final_answer_format: None,
    })
    .map_err(|err| FixedScriptRunnerError::Assertion(err.to_string()))?;
    rlm_contract_config_with_turn_options(options)
}

fn rlm_contract_config_with_turn_options(
    termination: lash_core::ProtocolTurnOptions,
) -> Result<lash_core::TurnMachineConfig, FixedScriptRunnerError> {
    let protocol_driver: Arc<
        dyn lash_core::sansio::ProtocolDriverHandle<lash_core::HostTurnProtocol>,
    > = Arc::new(lash_protocol_rlm::RlmDriver);
    Ok(lash_core::TurnMachineConfig {
        protocol_driver,
        projector: Arc::new(lash_core::sansio::ChatContextProjector),
        sync_execution_environment: true,
        model: "rlm-contract".to_string(),
        max_context_tokens: None,
        max_turns: None,
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        autonomous: false,
        tool_specs: Vec::new().into(),
        system_prompt: std::sync::Arc::from(""),
        session_id: "rlm-contract".to_string(),
        turn_id: "rlm-contract-turn".to_string(),
        emit_llm_trace: false,
        termination,
        turn_limit_final_message: Arc::new(contract_turn_limit_final_message),
    })
}

fn drain_rlm_contract_effects(machine: &mut lash_core::TurnMachine) -> Vec<lash_core::Effect> {
    let mut effects = Vec::new();
    while let Some(effect) = machine.poll_effect() {
        if let lash_core::Effect::SyncExecutionEnvironment { id, .. } = effect {
            effects.push(effect);
            machine.handle_response(lash_core::sansio::Response::ExecutionEnvironmentSynced {
                id,
                result: Ok(Some(lash_core::sansio::ExecutionEnvironmentSync {
                    system_prompt: std::sync::Arc::from(""),
                    tool_specs: Arc::new(Vec::new()),
                })),
            });
            continue;
        }
        effects.push(effect);
    }
    effects
}

pub(super) fn rlm_text_part(text: &str) -> LlmOutputPart {
    LlmOutputPart::Text {
        text: text.to_string(),
        response_meta: None,
    }
}

fn rlm_reasoning_part(text: &str) -> LlmOutputPart {
    LlmOutputPart::Reasoning {
        text: text.to_string(),
        replay: None,
    }
}

fn rlm_full_text(parts: &[LlmOutputPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            LlmOutputPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn rlm_lashlang_block(code: &str) -> String {
    format!("<lashlang>\n{code}\n</lashlang>")
}

fn rlm_lashlang_block_with_prose(prose: &str, code: &str) -> String {
    format!("{prose}\n{}", rlm_lashlang_block(code))
}

fn rlm_exec_response(
    output: &[&str],
    error: Option<&str>,
    terminal_finish: Option<Value>,
) -> lash_core::ExecResponse {
    lash_core::ExecResponse {
        observations: output.iter().map(|value| (*value).to_string()).collect(),
        observation_truncation: Vec::new(),
        tool_calls: Vec::new(),
        images: Vec::new(),
        printed_images: Vec::new(),
        error: error.map(str::to_string),
        duration_ms: 1,
        terminal_finish,
    }
}

fn rlm_exec_response_with_tool_calls(
    output: &[&str],
    error: Option<&str>,
    terminal_finish: Option<Value>,
    tool_calls: Vec<lash_core::ToolCallRecord>,
    duration_ms: u64,
) -> lash_core::ExecResponse {
    lash_core::ExecResponse {
        observations: output.iter().map(|value| (*value).to_string()).collect(),
        observation_truncation: Vec::new(),
        tool_calls,
        images: Vec::new(),
        printed_images: Vec::new(),
        error: error.map(str::to_string),
        duration_ms,
        terminal_finish,
    }
}

fn rlm_tool_call_record(
    call_id: &str,
    tool: &str,
    args: Value,
    output: lash_core::ToolCallOutput,
    duration_ms: u64,
) -> lash_core::ToolCallRecord {
    lash_core::ToolCallRecord {
        call_id: Some(call_id.to_string()),
        tool: tool.to_string(),
        args,
        output,
        duration_ms,
    }
}

pub(super) fn checkpoint_kind_name(checkpoint: lash_core::CheckpointKind) -> &'static str {
    match checkpoint {
        lash_core::CheckpointKind::BeforeCompletion => "before_completion",
        lash_core::CheckpointKind::AfterWork => "after_work",
    }
}

fn rlm_contract_llm_extraction_diagnostics(machine: &lash_core::TurnMachine) -> Vec<Value> {
    machine
        .events()
        .iter()
        .filter_map(|event| match event {
            lash_core::SessionHistoryRecord::Protocol(event) => {
                match lash_protocol_rlm::decode_rlm_protocol_event(event) {
                    Some(RlmProtocolEvent::RlmDiagnostic(diagnostic))
                        if diagnostic.phase == "llm_extraction" =>
                    {
                        Some(diagnostic.payload)
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .collect()
}

fn rlm_contract_trajectory(machine: &lash_core::TurnMachine) -> Vec<Value> {
    machine
        .events()
        .iter()
        .filter_map(|event| match event {
            lash_core::SessionHistoryRecord::Protocol(event) => {
                match lash_protocol_rlm::decode_rlm_protocol_event(event) {
                    Some(RlmProtocolEvent::RlmTrajectoryEntry(entry)) => {
                        serde_json::to_value(entry).ok()
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .collect()
}

fn rlm_contract_system_messages(machine: &lash_core::TurnMachine) -> Vec<String> {
    machine
        .messages()
        .iter()
        .filter(|message| message.role == lash_core::MessageRole::System)
        .flat_map(|message| message.parts.iter().map(|part| part.content.clone()))
        .collect()
}
