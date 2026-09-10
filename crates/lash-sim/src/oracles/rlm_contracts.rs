use super::*;

pub(super) fn rlm_protocol_execution_fact(
    events: &[DeliveredBoundary],
    contract: &'static str,
) -> Result<ScenarioContractGeneratedFact, String> {
    let (scenario, fact, assertion) = rlm_protocol_contract_metadata(contract)?;
    let proof_event = contract_execution_event(events, contract)?;
    let execution = contract_execution_payload_matches_observed(proof_event, contract, scenario)?;
    let result = execution
        .get("result")
        .ok_or_else(|| format!("{contract} execution missing result"))?;
    let contract_observed = match contract {
        "rlm.natural_prose_finalizes" => {
            require_rlm_bool(result, "/done", true, contract)?;
            require_rlm_u64(result, "/llm_call_count", 1, contract)?;
            require_rlm_bool(result, "/initial_request_tools_empty", true, contract)?;
            require_rlm_bool(result, "/final_message_event", false, contract)?;
            require_rlm_bool(result, "/assistant_conversation_progress", false, contract)?;
            require_rlm_checkpoint(result, "before_completion", contract)?;
            require_rlm_turn_outcome_contains(result, "finished", "AssistantMessage", contract)?;
            require_rlm_diagnostic(result, "finish_prose", "natural", contract)?;
            json!({
                "mode": "natural",
                "decision": "finish_prose",
                "done": true,
                "turn_outcome": "assistant_message",
                "llm_call_count": 1,
                "assistant_conversation_progress": false,
            })
        }
        "rlm.typed_prose_requires_finish" => {
            require_rlm_bool(result, "/done", false, contract)?;
            require_rlm_u64(result, "/llm_call_count", 2, contract)?;
            require_rlm_checkpoint(result, "after_work", contract)?;
            require_rlm_system_contains(result, "No code from that response executed.", contract)?;
            require_rlm_system_contains(result, "finish <value>", contract)?;
            require_rlm_system_omits(result, "required output schema", contract)?;
            require_rlm_diagnostic(result, "request_finish", "finish_required", contract)?;
            json!({
                "mode": "finish_required",
                "decision": "request_finish",
                "done": false,
                "repair_prompt_contains": ["No code from that response executed.", "finish <value>"],
                "llm_call_count": 2,
            })
        }
        "rlm.finish_required_max_turn_stop" => {
            require_rlm_bool(result, "/done", true, contract)?;
            require_rlm_u64(result, "/llm_call_count", 1, contract)?;
            require_rlm_stopped_max_turns(result, contract)?;
            require_rlm_system_omits(result, "No code from that response executed.", contract)?;
            require_rlm_system_omits(result, "finish <value>", contract)?;
            json!({
                "mode": "finish_required",
                "done": true,
                "stop_reason": "max_turns",
                "llm_call_count": 1,
                "retry_prompt_after_max_turn": false,
            })
        }
        "rlm.exec_error_max_turn_stop" => {
            require_rlm_bool(result, "/done", true, contract)?;
            require_rlm_u64(result, "/llm_call_count", 1, contract)?;
            require_rlm_exec_code(result, "missing_name", contract)?;
            require_rlm_stopped_max_turns(result, contract)?;
            require_rlm_trajectory_error(
                result,
                Some("unknown variable `missing_name`"),
                contract,
            )?;
            json!({
                "mode": "finish_required",
                "done": true,
                "stop_reason": "max_turns",
                "exec_code": "missing_name",
                "trajectory_error": "unknown variable `missing_name`",
            })
        }
        "rlm.typed_finish_emits_outcome_and_done" => {
            require_rlm_bool(result, "/done", true, contract)?;
            require_rlm_u64(result, "/llm_call_count", 1, contract)?;
            require_rlm_exec_code(result, "finish { ok: true }", contract)?;
            require_rlm_checkpoint(result, "before_completion", contract)?;
            require_rlm_final_value(result, &json!({ "ok": true }), contract)?;
            require_rlm_bool(result, "/final_message_event", false, contract)?;
            require_rlm_trajectory_error(result, None, contract)?;
            json!({
                "mode": "finish_required_schema",
                "done": true,
                "final_value": { "ok": true },
                "exec_code": "finish { ok: true }",
                "final_message_event": false,
                "checkpoint": "before_completion",
            })
        }
        "rlm.finish_required_diagnostic_counts" => {
            let diagnostic =
                require_rlm_diagnostic(result, "request_finish", "finish_required", contract)?;
            require_rlm_count(diagnostic, "full_text_chars", 12, contract)?;
            require_rlm_count(diagnostic, "prose_chars", 12, contract)?;
            require_rlm_count(diagnostic, "code_chars", 0, contract)?;
            require_rlm_count(diagnostic, "reasoning_chars", 0, contract)?;
            require_rlm_count(diagnostic, "lashlang_cell_count", 0, contract)?;
            require_rlm_checkpoint(result, "after_work", contract)?;
            json!({
                "decision": "request_finish",
                "termination": "finish_required",
                "counts": diagnostic.get("counts").cloned().unwrap_or(Value::Null),
            })
        }
        "rlm.natural_diagnostic_counts" => {
            let diagnostic = require_rlm_diagnostic(result, "finish_prose", "natural", contract)?;
            require_rlm_count(diagnostic, "full_text_chars", 12, contract)?;
            require_rlm_count(diagnostic, "prose_chars", 12, contract)?;
            require_rlm_count(diagnostic, "code_chars", 0, contract)?;
            require_rlm_count(diagnostic, "reasoning_chars", 0, contract)?;
            require_rlm_count(diagnostic, "lashlang_cell_count", 0, contract)?;
            require_rlm_checkpoint(result, "before_completion", contract)?;
            json!({
                "decision": "finish_prose",
                "termination": "natural",
                "counts": diagnostic.get("counts").cloned().unwrap_or(Value::Null),
            })
        }
        "rlm.cell_diagnostic_counts" => {
            let diagnostic =
                require_rlm_diagnostic(result, "execute_lashlang", "natural", contract)?;
            require_rlm_count(diagnostic, "lashlang_cell_count", 1, contract)?;
            require_rlm_count(diagnostic, "code_chars", 10, contract)?;
            require_rlm_exec_code(result, "print \"hi\"", contract)?;
            require_rlm_trajectory_error(result, None, contract)?;
            json!({
                "decision": "execute_lashlang",
                "counts": diagnostic.get("counts").cloned().unwrap_or(Value::Null),
                "exec_code": "print \"hi\"",
                "trajectory_last": rlm_trajectory_last(result).cloned().unwrap_or(Value::Null),
            })
        }
        "rlm.retired_marker_plain_lashlang_text" => {
            let code = "text = \"%%lashlang is just source here\"\nprint text";
            let diagnostic =
                require_rlm_diagnostic(result, "execute_lashlang", "natural", contract)?;
            require_rlm_count(diagnostic, "lashlang_cell_count", 1, contract)?;
            require_rlm_exec_code(result, code, contract)?;
            require_rlm_no_tool_call_event(result, contract)?;
            json!({
                "decision": "execute_lashlang",
                "exec_code": code,
                "retired_marker_interpreted_as_source_text": true,
                "tool_call_event": false,
            })
        }
        "rlm.lashlang_cell_exec_continues" => {
            require_rlm_bool(result, "/done", false, contract)?;
            require_rlm_u64(result, "/llm_call_count", 2, contract)?;
            require_rlm_exec_code(result, "print \"hi\"", contract)?;
            require_rlm_checkpoint(result, "after_work", contract)?;
            require_rlm_trajectory_error(result, None, contract)?;
            require_rlm_trajectory_output_contains(result, "hi\n", contract)?;
            json!({
                "done": false,
                "llm_call_count": 2,
                "exec_code": "print \"hi\"",
                "checkpoint": "after_work",
                "trajectory_output": "hi\n",
            })
        }
        "rlm.streamed_lashlang_cell_exec_persists_trajectory" => {
            require_rlm_bool(result, "/done", false, contract)?;
            require_rlm_u64(result, "/llm_call_count", 2, contract)?;
            require_rlm_response_text_streamed(result, 0, true, contract)?;
            require_rlm_exec_code(result, "print \"streamed\"", contract)?;
            require_rlm_checkpoint(result, "after_work", contract)?;
            require_rlm_trajectory_error(result, None, contract)?;
            require_rlm_trajectory_output_contains(result, "streamed\n", contract)?;
            json!({
                "done": false,
                "llm_call_count": 2,
                "text_streamed": true,
                "exec_code": "print \"streamed\"",
                "checkpoint": "after_work",
                "trajectory_output": "streamed\n",
            })
        }
        "rlm.empty_options_natural_default" => {
            require_rlm_bool(result, "/done", true, contract)?;
            require_rlm_final_value(result, &json!("done"), contract)?;
            require_rlm_exec_code(result, "finish \"done\"", contract)?;
            require_rlm_checkpoint(result, "before_completion", contract)?;
            if result.pointer("/termination/kind").and_then(Value::as_str)
                != Some("empty_protocol_turn_options")
            {
                return Err(format!(
                    "{contract} did not execute with empty protocol turn options"
                ));
            }
            json!({
                "mode": "empty_options_default",
                "natural_default": true,
                "final_value": "done",
                "exec_code": "finish \"done\"",
            })
        }
        "rlm.exec_result_no_tool_call_replay" => {
            require_rlm_exec_code(
                result,
                "x = await tools.read_file({ path: \"foo\" })?",
                contract,
            )?;
            require_rlm_checkpoint(result, "after_work", contract)?;
            require_rlm_tool_call_event(result, contract)?;
            require_rlm_trajectory_omits(result, "rlm-call-1", contract)?;
            require_rlm_trajectory_error(result, None, contract)?;
            json!({
                "exec_code": "x = await tools.read_file({ path: \"foo\" })?",
                "checkpoint": "after_work",
                "tool_call_event": true,
                "trajectory_omits_tool_call_id": "rlm-call-1",
            })
        }
        "rlm.exec_tool_control_frame_switch_terminal" => {
            require_rlm_bool(result, "/done", true, contract)?;
            require_rlm_exec_code(result, "x = await tools.custom_frame_switch({})?", contract)?;
            require_rlm_checkpoint(result, "before_completion", contract)?;
            require_rlm_tool_call_event(result, contract)?;
            require_rlm_agent_frame_switch(result, "next-frame", "continue", 1, contract)?;
            require_rlm_trajectory_error(result, None, contract)?;
            json!({
                "done": true,
                "exec_code": "x = await tools.custom_frame_switch({})?",
                "checkpoint": "before_completion",
                "agent_frame_switch": {
                    "frame_key_material": "next-frame",
                    "task": "continue",
                    "initial_node_count": 1,
                },
                "tool_call_event": true,
            })
        }
        "rlm.exec_tool_control_fail_terminal" => {
            require_rlm_bool(result, "/done", true, contract)?;
            require_rlm_exec_code(result, "x = await tools.custom_fail({})?", contract)?;
            require_rlm_checkpoint(result, "before_completion", contract)?;
            require_rlm_tool_call_event(result, contract)?;
            require_rlm_tool_error(result, "custom_fail", "no valid result", contract)?;
            require_rlm_trajectory_error(result, None, contract)?;
            json!({
                "done": true,
                "exec_code": "x = await tools.custom_fail({})?",
                "checkpoint": "before_completion",
                "tool_error": { "tool_name": "custom_fail", "message": "no valid result" },
                "tool_call_event": true,
            })
        }
        "rlm.natural_allows_finish_value" => {
            require_rlm_bool(result, "/done", true, contract)?;
            require_rlm_final_value(result, &json!({ "ok": true }), contract)?;
            require_rlm_exec_code(result, "finish { ok: true }", contract)?;
            require_rlm_checkpoint(result, "before_completion", contract)?;
            json!({
                "mode": "natural",
                "final_value": { "ok": true },
                "exec_code": "finish { ok: true }",
            })
        }
        "rlm.typed_schema_mismatch_repair_loop" => {
            require_rlm_u64(result, "/llm_call_count", 2, contract)?;
            require_rlm_checkpoint(result, "after_work", contract)?;
            require_rlm_exec_code(result, "finish { missing: true }", contract)?;
            require_rlm_system_contains(
                result,
                "didn't match the required output schema",
                contract,
            )?;
            require_rlm_trajectory_error(result, Some("\"ok\" is a required property"), contract)?;
            json!({
                "mode": "finish_required_schema",
                "schema_feedback": "required property",
                "repair_loop_next_llm_call": true,
                "llm_call_count": 2,
            })
        }
        "rlm.typed_schema_any_of_mismatch" => {
            require_rlm_checkpoint(result, "after_work", contract)?;
            require_rlm_exec_code(result, "finish true", contract)?;
            require_rlm_system_contains(
                result,
                "didn't match the required output schema",
                contract,
            )?;
            require_rlm_trajectory_error(
                result,
                Some("true is not valid under any of the schemas listed in the 'anyOf' keyword"),
                contract,
            )?;
            json!({
                "mode": "finish_required_schema",
                "schema_feedback": "anyOf",
                "exec_code": "finish true",
            })
        }
        other => {
            return Err(format!(
                "RLM protocol contract execution fact has no checker for `{other}`"
            ));
        }
    };
    generated_fact(
        fact,
        assertion,
        vec![proof_event],
        json!({
            "contract_execution_boundary": proof_event.boundary_id,
            "contract": contract,
            "observed": contract_observed,
            "source": execution.get("source").cloned().unwrap_or(Value::Null),
        }),
    )
}

pub(super) fn rlm_protocol_contract_metadata(
    contract: &str,
) -> Result<(&'static str, &'static str, &'static str), String> {
    match contract {
        "rlm.natural_prose_finalizes" => Ok((
            "rlm_protocol_scenario_prose_only_response_finishes_by_default",
            "rlm_natural_prose_finalizes",
            "natural RLM prose-only response finishes as an assistant-message outcome with clean natural diagnostics",
        )),
        "rlm.typed_prose_requires_finish" => Ok((
            "rlm_protocol_scenario_typed_prose_only_response_requests_finish",
            "rlm_typed_prose_requires_finish",
            "finish-required prose-only response stays unfinished and emits explicit finish repair feedback",
        )),
        "rlm.finish_required_max_turn_stop" => Ok((
            "rlm_protocol_scenario_finish_required_prose_at_max_turns_stops_without_retry_prompt",
            "rlm_finish_required_max_turn_stop",
            "finish-required prose at max turns stops with TurnStop::MaxTurns and no extra retry prompt",
        )),
        "rlm.exec_error_max_turn_stop" => Ok((
            "rlm_protocol_scenario_finish_required_exec_error_at_max_turns_stops_without_retry",
            "rlm_exec_error_max_turn_stop",
            "finish-required exec error at max turns records the concrete exec failure and stops with TurnStop::MaxTurns",
        )),
        "rlm.typed_finish_emits_outcome_and_done" => Ok((
            "rlm_protocol_scenario_typed_finish_emits_turn_outcome_and_done",
            "rlm_typed_finish_emits_outcome_and_done",
            "typed RLM finish executes LashLang, emits a concrete final-value TurnOutcome, and marks the turn done without a final message event",
        )),
        "rlm.finish_required_diagnostic_counts" => Ok((
            "rlm_protocol_scenario_finish_required_prose_only_diagnostic_has_clean_counts",
            "rlm_finish_required_diagnostic_counts",
            "finish-required prose diagnostic records exact prose/code/reasoning/lashlang counts",
        )),
        "rlm.natural_diagnostic_counts" => Ok((
            "rlm_protocol_scenario_natural_prose_only_diagnostic_has_clean_counts",
            "rlm_natural_diagnostic_counts",
            "natural prose diagnostic records exact prose/code/reasoning/lashlang counts",
        )),
        "rlm.cell_diagnostic_counts" => Ok((
            "rlm_protocol_scenario_cell_reasoning_prose_code_diagnostic_has_clean_counts",
            "rlm_cell_diagnostic_counts",
            "mixed reasoning/prose/lashlang diagnostic records one cell and the concrete executed code",
        )),
        "rlm.retired_marker_plain_lashlang_text" => Ok((
            "rlm_protocol_scenario_retired_percent_marker_inside_source_is_plain_lashlang_text",
            "rlm_retired_marker_plain_lashlang_text",
            "retired percent LashLang marker inside a source block remains plain source text and executes as one cell",
        )),
        "rlm.lashlang_cell_exec_continues" => Ok((
            "rlm_protocol_scenario_lashlang_cell_runs_exec_and_continues",
            "rlm_lashlang_cell_exec_continues",
            "LashLang cell execution records concrete output, checkpoints after work, and re-enters the model loop",
        )),
        "rlm.streamed_lashlang_cell_exec_persists_trajectory" => Ok((
            "rlm_protocol_scenario_streamed_lashlang_cell_runs_exec_and_persists_trajectory",
            "rlm_streamed_lashlang_cell_exec_persists_trajectory",
            "streamed LashLang cell execution records concrete output, checkpoints after work, and preserves trajectory evidence before re-entering the model loop",
        )),
        "rlm.empty_options_natural_default" => Ok((
            "rlm_protocol_scenario_empty_turn_options_use_natural_default",
            "rlm_empty_options_natural_default",
            "empty RLM turn options default to natural mode and accept explicit finish value",
        )),
        "rlm.exec_result_no_tool_call_replay" => Ok((
            "rlm_protocol_scenario_exec_result_emits_accounting_without_storing_tool_call_ids",
            "rlm_exec_result_no_tool_call_replay",
            "exec results emit standard tool-call accounting events without storing tool-call ids in RLM trajectory",
        )),
        "rlm.exec_tool_control_frame_switch_terminal" => Ok((
            "rlm_protocol_scenario_exec_any_tool_control_frame_switch_is_terminal",
            "rlm_exec_tool_control_frame_switch_terminal",
            "exec result tool control frame switch terminalizes as a concrete AgentFrameSwitch outcome",
        )),
        "rlm.exec_tool_control_fail_terminal" => Ok((
            "rlm_protocol_scenario_exec_any_tool_control_fail_is_terminal_error",
            "rlm_exec_tool_control_fail_terminal",
            "exec result tool control failure terminalizes as a concrete ToolError outcome",
        )),
        "rlm.natural_allows_finish_value" => Ok((
            "rlm_protocol_scenario_natural_allows_finish_value",
            "rlm_natural_allows_finish_value",
            "natural RLM mode accepts explicit finish value as a final-value outcome",
        )),
        "rlm.typed_schema_mismatch_repair_loop" => Ok((
            "rlm_protocol_scenario_typed_schema_mismatch_loops_with_feedback",
            "rlm_typed_schema_mismatch_repair_loop",
            "typed schema mismatch emits concrete required-property feedback and re-enters the LLM loop",
        )),
        "rlm.typed_schema_any_of_mismatch" => Ok((
            "rlm_protocol_scenario_typed_schema_mismatch_checks_any_of",
            "rlm_typed_schema_anyof_mismatch",
            "typed schema mismatch checks anyOf and records the concrete validation error",
        )),
        other => Err(format!("no RLM protocol metadata registered for `{other}`")),
    }
}

pub(super) fn require_rlm_bool(
    result: &Value,
    pointer: &str,
    expected: bool,
    contract: &str,
) -> Result<(), String> {
    if result.pointer(pointer).and_then(Value::as_bool) == Some(expected) {
        Ok(())
    } else {
        Err(format!("{contract} expected {pointer}={expected}"))
    }
}

pub(super) fn require_rlm_u64(
    result: &Value,
    pointer: &str,
    expected: u64,
    contract: &str,
) -> Result<(), String> {
    if result.pointer(pointer).and_then(Value::as_u64) == Some(expected) {
        Ok(())
    } else {
        Err(format!("{contract} expected {pointer}={expected}"))
    }
}

pub(super) fn require_rlm_checkpoint(
    result: &Value,
    checkpoint: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("checkpoints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|value| value.as_str() == Some(checkpoint))
    {
        Ok(())
    } else {
        Err(format!("{contract} missing checkpoint `{checkpoint}`"))
    }
}

pub(super) fn require_rlm_response_text_streamed(
    result: &Value,
    index: usize,
    expected: bool,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("llm_response_text_streamed")
        .and_then(Value::as_array)
        .and_then(|values| values.get(index))
        .and_then(Value::as_bool)
        == Some(expected)
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} expected llm_response_text_streamed[{index}]={expected}"
        ))
    }
}

pub(super) fn require_rlm_turn_outcome_contains(
    result: &Value,
    kind: &str,
    needle: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("turn_outcomes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|outcome| {
            outcome.get("kind").and_then(Value::as_str) == Some(kind)
                && outcome
                    .get("finish")
                    .or_else(|| outcome.get("stop_reason"))
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.contains(needle))
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing turn outcome `{kind}` containing `{needle}`"
        ))
    }
}

pub(super) fn require_rlm_stopped_max_turns(result: &Value, contract: &str) -> Result<(), String> {
    if result
        .get("turn_outcomes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|outcome| {
            outcome.get("kind").and_then(Value::as_str) == Some("stopped")
                && outcome.get("stop_reason").and_then(Value::as_str) == Some("max_turns")
        })
    {
        Ok(())
    } else {
        Err(format!("{contract} missing TurnStop::MaxTurns outcome"))
    }
}

pub(super) fn require_rlm_diagnostic<'a>(
    result: &'a Value,
    decision: &str,
    termination: &str,
    contract: &str,
) -> Result<&'a Value, String> {
    let Some(diagnostic) = result
        .get("llm_extraction_diagnostics")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|diagnostic| {
            diagnostic.get("decision").and_then(Value::as_str) == Some(decision)
                && diagnostic.get("termination").and_then(Value::as_str) == Some(termination)
        })
    else {
        return Err(format!(
            "{contract} missing llm_extraction diagnostic decision={decision} termination={termination}"
        ));
    };
    Ok(diagnostic)
}

pub(super) fn require_rlm_count(
    diagnostic: &Value,
    count_name: &str,
    expected: u64,
    contract: &str,
) -> Result<(), String> {
    if diagnostic
        .pointer(&format!("/counts/{count_name}"))
        .and_then(Value::as_u64)
        == Some(expected)
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} diagnostic count `{count_name}` changed"
        ))
    }
}

pub(super) fn require_rlm_system_contains(
    result: &Value,
    needle: &str,
    contract: &str,
) -> Result<(), String> {
    if rlm_system_messages(result)
        .iter()
        .any(|message| message.contains(needle))
    {
        Ok(())
    } else {
        Err(format!("{contract} missing system feedback `{needle}`"))
    }
}

pub(super) fn require_rlm_system_omits(
    result: &Value,
    needle: &str,
    contract: &str,
) -> Result<(), String> {
    if rlm_system_messages(result)
        .iter()
        .all(|message| !message.contains(needle))
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} unexpectedly emitted system feedback `{needle}`"
        ))
    }
}

pub(super) fn rlm_system_messages(result: &Value) -> Vec<&str> {
    result
        .get("system_messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

pub(super) fn require_rlm_exec_code(
    result: &Value,
    expected: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("exec_codes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|value| value.as_str() == Some(expected))
    {
        Ok(())
    } else {
        Err(format!("{contract} missing exec code `{expected}`"))
    }
}

pub(super) fn require_rlm_final_value(
    result: &Value,
    expected: &Value,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("turn_outcomes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|outcome| {
            outcome.get("kind").and_then(Value::as_str) == Some("final_value")
                && outcome.get("value") == Some(expected)
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing final-value outcome `{expected}`"
        ))
    }
}

pub(super) fn require_rlm_trajectory_error(
    result: &Value,
    expected: Option<&str>,
    contract: &str,
) -> Result<(), String> {
    let Some(last) = rlm_trajectory_last(result) else {
        return Err(format!("{contract} missing RLM trajectory entry"));
    };
    match expected {
        Some(needle)
            if last
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|error| error.contains(needle)) =>
        {
            Ok(())
        }
        Some(needle) => Err(format!(
            "{contract} trajectory error did not contain `{needle}`"
        )),
        None if last.get("error").is_none() => Ok(()),
        None => Err(format!("{contract} trajectory unexpectedly had an error")),
    }
}

pub(super) fn require_rlm_trajectory_output_contains(
    result: &Value,
    expected: &str,
    contract: &str,
) -> Result<(), String> {
    let Some(last) = rlm_trajectory_last(result) else {
        return Err(format!("{contract} missing RLM trajectory entry"));
    };
    if last
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|value| value.as_str() == Some(expected))
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} trajectory output did not contain `{expected}`"
        ))
    }
}

pub(super) fn require_rlm_trajectory_omits(
    result: &Value,
    forbidden: &str,
    contract: &str,
) -> Result<(), String> {
    let trajectory = result.get("trajectory").cloned().unwrap_or(Value::Null);
    if !trajectory.to_string().contains(forbidden) {
        Ok(())
    } else {
        Err(format!(
            "{contract} trajectory unexpectedly retained `{forbidden}`"
        ))
    }
}

pub(super) fn rlm_trajectory_last(result: &Value) -> Option<&Value> {
    result
        .get("trajectory")
        .and_then(Value::as_array)
        .and_then(|values| values.last())
}

pub(super) fn require_rlm_no_tool_call_event(result: &Value, contract: &str) -> Result<(), String> {
    if result.get("tool_call_event").and_then(Value::as_bool) == Some(false) {
        Ok(())
    } else {
        Err(format!("{contract} unexpectedly emitted a tool-call event"))
    }
}

pub(super) fn require_rlm_tool_call_event(result: &Value, contract: &str) -> Result<(), String> {
    if result.get("tool_call_event").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(format!(
            "{contract} did not emit a tool-call accounting event"
        ))
    }
}

pub(super) fn require_rlm_agent_frame_switch(
    result: &Value,
    frame_key_material: &str,
    task: &str,
    initial_node_count: usize,
    contract: &str,
) -> Result<(), String> {
    let expected_frame_key = lash_core::FrameKey::from_caller_material(frame_key_material)
        .expect("non-empty frame key material");
    if result
        .get("turn_outcomes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|outcome| {
            outcome.get("kind").and_then(Value::as_str) == Some("agent_frame_switch")
                && outcome.get("frame_key").and_then(Value::as_str)
                    == Some(expected_frame_key.as_str())
                && outcome.get("task").and_then(Value::as_str) == Some(task)
                && outcome
                    .get("initial_nodes")
                    .and_then(Value::as_array)
                    .is_some_and(|nodes| nodes.len() == initial_node_count)
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing AgentFrameSwitch outcome for key material `{frame_key_material}` / `{task}` with {initial_node_count} seed nodes"
        ))
    }
}

pub(super) fn require_rlm_tool_error(
    result: &Value,
    tool_name: &str,
    message: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("turn_outcomes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|outcome| {
            outcome.get("kind").and_then(Value::as_str) == Some("stopped")
                && outcome
                    .get("stop_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| {
                        reason.contains("ToolError")
                            && reason.contains(tool_name)
                            && reason.contains(message)
                    })
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing ToolError outcome `{tool_name}` containing `{message}`"
        ))
    }
}

pub(super) fn reject_named_contract_proxy_facts(
    semantic_oracle: &str,
    facts: &[ScenarioContractGeneratedFact],
) -> Result<(), String> {
    if !matches!(
        semantic_oracle,
        "standard.max_turns_after_tool_result"
            | "rlm.typed_finish_emits_outcome_and_done"
            | "agent.tuple_values_finish_as_json_arrays"
    ) {
        return Ok(());
    }
    for fact in facts {
        if let Some(proxy_kind) = proxy_fact_kind(fact) {
            return Err(format!(
                "contract `{semantic_oracle}` attempted to use {proxy_kind} proxy fact `{}` instead of contract-owned evidence",
                fact.fact
            ));
        }
    }
    Ok(())
}

pub(super) fn proxy_fact_kind(fact: &ScenarioContractGeneratedFact) -> Option<&'static str> {
    if fact.observed.get("semantic_proof_boundary").is_some()
        || fact.observed.get("semantic_proof").is_some()
    {
        return Some("semantic-proof-only trigger");
    }
    if fact.fact == "generated_transition_evidence_present"
        || fact
            .assertion
            .contains("scenario contract selected generated trace events")
        || (fact.observed.get("selected_event_count").is_some()
            && fact.observed.get("boundary_kinds").is_some())
    {
        return Some("generic transition fallback");
    }
    if fact
        .assertion
        .contains("generated provider boundary completed successfully")
        || fact.observed.get("provider_boundary").is_some()
    {
        return Some("ProviderTerminalRequirement::AnySuccessful");
    }
    if fact
        .assertion
        .contains("one generated actor completed sequential")
        || fact.observed.get("provider_turns").is_some()
    {
        return Some("ProviderTerminalRequirement::SequentialTurns");
    }
    None
}
