use super::*;

/// The RLM family's fact-spec rows, in spec-table order. `spec` is resolved
/// from the imported contract table at compile time; the source scenario the
/// metadata table used to copy is now just `spec.test_name`.
pub(super) const RLM_CONTRACT_FACT_SPECS: &[ContractFactSpec] = &[
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.natural_prose_finalizes",
        ),
        fact: "rlm_natural_prose_finalizes",
        assertion: "natural RLM prose-only response finishes as an assistant-message outcome with clean natural diagnostics",
        check: check_rlm_natural_prose_finalizes,
        extras_before: &[],
        extras_after: &[],
    },
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.typed_prose_requires_finish",
        ),
        fact: "rlm_typed_prose_requires_finish",
        assertion: "finish-required prose-only response stays unfinished and emits explicit finish repair feedback",
        check: check_rlm_typed_prose_requires_finish,
        extras_before: &[],
        extras_after: &[],
    },
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.finish_required_max_turn_stop",
        ),
        fact: "rlm_finish_required_max_turn_stop",
        assertion: "finish-required prose at max turns stops with TurnStop::MaxTurns and no extra retry prompt",
        check: check_rlm_finish_required_max_turn_stop,
        extras_before: &[],
        extras_after: &[],
    },
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.exec_error_max_turn_stop",
        ),
        fact: "rlm_exec_error_max_turn_stop",
        assertion: "finish-required exec error at max turns records the concrete exec failure and stops with TurnStop::MaxTurns",
        check: check_rlm_exec_error_max_turn_stop,
        extras_before: &[],
        extras_after: &[ExtraFact::Exec {
            fact: "rlm_exec_error_max_turn_stop",
            requirement: ExecFactRequirement::RuntimeOutcome,
        }],
    },
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.finish_required_diagnostic_counts",
        ),
        fact: "rlm_finish_required_diagnostic_counts",
        assertion: "finish-required prose diagnostic records exact prose/code/reasoning/lashlang counts",
        check: check_rlm_finish_required_diagnostic_counts,
        extras_before: &[],
        extras_after: &[],
    },
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.natural_diagnostic_counts",
        ),
        fact: "rlm_natural_diagnostic_counts",
        assertion: "natural prose diagnostic records exact prose/code/reasoning/lashlang counts",
        check: check_rlm_natural_diagnostic_counts,
        extras_before: &[],
        extras_after: &[],
    },
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.cell_diagnostic_counts",
        ),
        fact: "rlm_cell_diagnostic_counts",
        assertion: "mixed reasoning/prose/lashlang diagnostic records one cell and the concrete executed code",
        check: check_rlm_cell_diagnostic_counts,
        extras_before: &[],
        extras_after: &[ExtraFact::Exec {
            fact: "rlm_cell_diagnostic_exec_counts",
            requirement: ExecFactRequirement::RuntimeOutcome,
        }],
    },
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.retired_marker_plain_lashlang_text",
        ),
        fact: "rlm_retired_marker_plain_lashlang_text",
        assertion: "retired percent LashLang marker inside a source block remains plain source text and executes as one cell",
        check: check_rlm_retired_marker_plain_lashlang_text,
        extras_before: &[],
        extras_after: &[ExtraFact::Exec {
            fact: "rlm_retired_marker_plain_lashlang_text",
            requirement: ExecFactRequirement::NoToolCallReplay,
        }],
    },
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.lashlang_cell_exec_continues",
        ),
        fact: "rlm_lashlang_cell_exec_continues",
        assertion: "LashLang cell execution records concrete output, checkpoints after work, and re-enters the model loop",
        check: check_rlm_lashlang_cell_exec_continues,
        extras_before: &[],
        extras_after: &[ExtraFact::Exec {
            fact: "rlm_lashlang_cell_exec_continues",
            requirement: ExecFactRequirement::ReentersProvider,
        }],
    },
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.streamed_lashlang_cell_exec_persists_trajectory",
        ),
        fact: "rlm_streamed_lashlang_cell_exec_persists_trajectory",
        assertion: "streamed LashLang cell execution records concrete output, checkpoints after work, and preserves trajectory evidence before re-entering the model loop",
        check: check_rlm_streamed_lashlang_cell_exec_persists_trajectory,
        extras_before: &[],
        extras_after: &[ExtraFact::Exec {
            fact: "rlm_streamed_lashlang_cell_exec_persists_trajectory",
            requirement: ExecFactRequirement::ReentersProvider,
        }],
    },
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.empty_options_natural_default",
        ),
        fact: "rlm_empty_options_natural_default",
        assertion: "empty RLM turn options default to natural mode and accept explicit finish value",
        check: check_rlm_empty_options_natural_default,
        extras_before: &[],
        extras_after: &[],
    },
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.exec_result_no_tool_call_replay",
        ),
        fact: "rlm_exec_result_no_tool_call_replay",
        assertion: "exec results emit standard tool-call accounting events without storing tool-call ids in RLM trajectory",
        check: check_rlm_exec_result_no_tool_call_replay,
        extras_before: &[],
        extras_after: &[ExtraFact::Exec {
            fact: "rlm_exec_result_no_tool_call_replay",
            requirement: ExecFactRequirement::NoToolCallReplay,
        }],
    },
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.exec_tool_control_frame_switch_terminal",
        ),
        fact: "rlm_exec_tool_control_frame_switch_terminal",
        assertion: "exec result tool control frame switch terminalizes as a concrete AgentFrameSwitch outcome",
        check: check_rlm_exec_tool_control_frame_switch_terminal,
        extras_before: &[],
        extras_after: &[
            ExtraFact::Exec {
                fact: "rlm_exec_tool_control_frame_switch_terminal",
                requirement: ExecFactRequirement::RuntimeOutcome,
            },
            ExtraFact::TriggerThenProvider("rlm_exec_tool_control_frame_switch_trigger"),
        ],
    },
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.exec_tool_control_fail_terminal",
        ),
        fact: "rlm_exec_tool_control_fail_terminal",
        assertion: "exec result tool control failure terminalizes as a concrete ToolError outcome",
        check: check_rlm_exec_tool_control_fail_terminal,
        extras_before: &[],
        extras_after: &[
            ExtraFact::Exec {
                fact: "rlm_exec_tool_control_fail_terminal",
                requirement: ExecFactRequirement::RuntimeOutcome,
            },
            ExtraFact::BackendRetry("rlm_exec_tool_control_fail_backend"),
        ],
    },
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.typed_finish_emits_outcome_and_done",
        ),
        fact: "rlm_typed_finish_emits_outcome_and_done",
        assertion: "typed RLM finish executes LashLang, emits a concrete final-value TurnOutcome, and marks the turn done without a final message event",
        check: check_rlm_typed_finish_emits_outcome_and_done,
        extras_before: &[],
        extras_after: &[],
    },
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.natural_allows_finish_value",
        ),
        fact: "rlm_natural_allows_finish_value",
        assertion: "natural RLM mode accepts explicit finish value as a final-value outcome",
        check: check_rlm_natural_allows_finish_value,
        extras_before: &[],
        extras_after: &[],
    },
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.typed_schema_mismatch_repair_loop",
        ),
        fact: "rlm_typed_schema_mismatch_repair_loop",
        assertion: "typed schema mismatch emits concrete required-property feedback and re-enters the LLM loop",
        check: check_rlm_typed_schema_mismatch_repair_loop,
        extras_before: &[ExtraFact::ProviderMutation {
            mutation: "malformed_sse_chunk",
            fact: "rlm_typed_schema_mismatch_feedback",
            assertion: "typed schema mismatch repair uses generated malformed provider payload feedback",
        }],
        extras_after: &[],
    },
    ContractFactSpec {
        spec: contract_spec(
            RLM_PROTOCOL_SCENARIO_CONTRACTS,
            "rlm.typed_schema_any_of_mismatch",
        ),
        fact: "rlm_typed_schema_anyof_mismatch",
        assertion: "typed schema mismatch checks anyOf and records the concrete validation error",
        check: check_rlm_typed_schema_any_of_mismatch,
        extras_before: &[ExtraFact::ProviderMutation {
            mutation: "rate_limit_error_envelope",
            fact: "rlm_typed_schema_anyof_feedback",
            assertion: "typed anyOf mismatch package carries generated parser-classified feedback",
        }],
        extras_after: &[],
    },
];

pub(super) fn rlm_protocol_execution_fact(
    events: &[DeliveredBoundary],
    row: &'static ContractFactSpec,
    memo: &ScenarioFactMemo,
) -> Result<ScenarioContractGeneratedFact, String> {
    let contract = row.spec.semantic_oracle;
    let proof_event = contract_execution_event(events, contract)?;
    // Same shape as the agent facts: which event proves the contract depends on
    // the candidate, what the fact says about that event does not.
    let boundary_id = proof_event.boundary_id.clone();
    memo.fact_from_proof_event(contract, &boundary_id, || {
        rlm_protocol_execution_fact_from_proof_event(proof_event, row)
    })
}

fn rlm_protocol_execution_fact_from_proof_event(
    proof_event: &DeliveredBoundary,
    row: &'static ContractFactSpec,
) -> Result<ScenarioContractGeneratedFact, String> {
    let contract = row.spec.semantic_oracle;
    let execution =
        contract_execution_payload_matches_observed(proof_event, contract, row.spec.test_name)?;
    let result = execution
        .get("result")
        .ok_or_else(|| format!("{contract} execution missing result"))?;
    let contract_observed = (row.check)(result, contract)?;
    generated_fact(
        row.fact,
        row.assertion,
        vec![proof_event],
        json!({
            "contract_execution_boundary": proof_event.boundary_id,
            "contract": contract,
            "observed": contract_observed,
            "source": execution.get("source").cloned().unwrap_or(Value::Null),
        }),
    )
}

fn check_rlm_natural_prose_finalizes(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_bool(result, "/done", true, contract)?;
    require_u64(result, "/llm_call_count", 1, contract)?;
    require_bool(result, "/initial_request_tools_empty", true, contract)?;
    require_bool(result, "/final_message_event", false, contract)?;
    require_bool(result, "/assistant_conversation_progress", false, contract)?;
    require_checkpoint(result, "before_completion", contract)?;
    require_rlm_turn_outcome_contains(result, "finished", "AssistantMessage", contract)?;
    require_rlm_diagnostic(result, "finish_prose", "natural", contract)?;
    Ok(json!({
        "mode": "natural",
        "decision": "finish_prose",
        "done": true,
        "turn_outcome": "assistant_message",
        "llm_call_count": 1,
        "assistant_conversation_progress": false,
    }))
}

fn check_rlm_typed_prose_requires_finish(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_bool(result, "/done", false, contract)?;
    require_u64(result, "/llm_call_count", 2, contract)?;
    require_checkpoint(result, "after_work", contract)?;
    require_rlm_system_contains(result, "No code from that response executed.", contract)?;
    require_rlm_system_contains(result, "finish(value)", contract)?;
    require_rlm_system_omits(result, "required output schema", contract)?;
    require_rlm_diagnostic(result, "request_finish", "finish_required", contract)?;
    Ok(json!({
        "mode": "finish_required",
        "decision": "request_finish",
        "done": false,
        "repair_prompt_contains": ["No code from that response executed.", "finish(value)"],
        "llm_call_count": 2,
    }))
}

fn check_rlm_finish_required_max_turn_stop(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_bool(result, "/done", true, contract)?;
    require_u64(result, "/llm_call_count", 1, contract)?;
    require_rlm_stopped_max_turns(result, contract)?;
    require_rlm_system_omits(result, "No code from that response executed.", contract)?;
    require_rlm_system_omits(result, "finish(value)", contract)?;
    Ok(json!({
        "mode": "finish_required",
        "done": true,
        "stop_reason": "max_turns",
        "llm_call_count": 1,
        "retry_prompt_after_max_turn": false,
    }))
}

fn check_rlm_exec_error_max_turn_stop(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_bool(result, "/done", true, contract)?;
    require_u64(result, "/llm_call_count", 1, contract)?;
    require_rlm_exec_code(result, "missing_name", contract)?;
    require_rlm_stopped_max_turns(result, contract)?;
    require_rlm_trajectory_error(result, Some("unknown binding `missing_name`"), contract)?;
    Ok(json!({
        "mode": "finish_required",
        "done": true,
        "stop_reason": "max_turns",
        "exec_code": "missing_name",
        "trajectory_error": "unknown binding `missing_name`",
    }))
}

fn check_rlm_finish_required_diagnostic_counts(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    let diagnostic = require_rlm_diagnostic(result, "request_finish", "finish_required", contract)?;
    require_rlm_count(diagnostic, "full_text_chars", 12, contract)?;
    require_rlm_count(diagnostic, "prose_chars", 12, contract)?;
    require_rlm_count(diagnostic, "code_chars", 0, contract)?;
    require_rlm_count(diagnostic, "reasoning_chars", 0, contract)?;
    require_rlm_count(diagnostic, "typescript_cell_count", 0, contract)?;
    require_checkpoint(result, "after_work", contract)?;
    Ok(json!({
        "decision": "request_finish",
        "termination": "finish_required",
        "counts": diagnostic.get("counts").cloned().unwrap_or(Value::Null),
    }))
}

fn check_rlm_natural_diagnostic_counts(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    let diagnostic = require_rlm_diagnostic(result, "finish_prose", "natural", contract)?;
    require_rlm_count(diagnostic, "full_text_chars", 12, contract)?;
    require_rlm_count(diagnostic, "prose_chars", 12, contract)?;
    require_rlm_count(diagnostic, "code_chars", 0, contract)?;
    require_rlm_count(diagnostic, "reasoning_chars", 0, contract)?;
    require_rlm_count(diagnostic, "typescript_cell_count", 0, contract)?;
    require_checkpoint(result, "before_completion", contract)?;
    Ok(json!({
        "decision": "finish_prose",
        "termination": "natural",
        "counts": diagnostic.get("counts").cloned().unwrap_or(Value::Null),
    }))
}

fn check_rlm_cell_diagnostic_counts(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    let diagnostic = require_rlm_diagnostic(result, "execute_typescript", "natural", contract)?;
    require_rlm_count(diagnostic, "typescript_cell_count", 1, contract)?;
    require_rlm_count(diagnostic, "code_chars", 12, contract)?;
    require_rlm_exec_code(result, "print(\"hi\");", contract)?;
    require_rlm_trajectory_error(result, None, contract)?;
    Ok(json!({
        "decision": "execute_typescript",
        "counts": diagnostic.get("counts").cloned().unwrap_or(Value::Null),
        "exec_code": "print(\"hi\");",
        "trajectory_last": rlm_trajectory_last(result).cloned().unwrap_or(Value::Null),
    }))
}

fn check_rlm_retired_marker_plain_lashlang_text(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    let code = "const text = \"%%lashlang is just source here\";\nprint(text);";
    let diagnostic = require_rlm_diagnostic(result, "execute_typescript", "natural", contract)?;
    require_rlm_count(diagnostic, "typescript_cell_count", 1, contract)?;
    require_rlm_exec_code(result, code, contract)?;
    require_rlm_no_tool_call_event(result, contract)?;
    Ok(json!({
        "decision": "execute_typescript",
        "exec_code": code,
        "retired_marker_interpreted_as_source_text": true,
        "tool_call_event": false,
    }))
}

fn check_rlm_lashlang_cell_exec_continues(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_bool(result, "/done", false, contract)?;
    require_u64(result, "/llm_call_count", 2, contract)?;
    require_rlm_exec_code(result, "print(\"hi\");", contract)?;
    require_checkpoint(result, "after_work", contract)?;
    require_rlm_trajectory_error(result, None, contract)?;
    require_rlm_trajectory_output_contains(result, "hi\n", contract)?;
    Ok(json!({
        "done": false,
        "llm_call_count": 2,
        "exec_code": "print(\"hi\");",
        "checkpoint": "after_work",
        "trajectory_output": "hi\n",
    }))
}

fn check_rlm_streamed_lashlang_cell_exec_persists_trajectory(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_bool(result, "/done", false, contract)?;
    require_u64(result, "/llm_call_count", 2, contract)?;
    require_rlm_response_text_streamed(result, 0, true, contract)?;
    require_rlm_exec_code(result, "print(\"streamed\");", contract)?;
    require_checkpoint(result, "after_work", contract)?;
    require_rlm_trajectory_error(result, None, contract)?;
    require_rlm_trajectory_output_contains(result, "streamed\n", contract)?;
    Ok(json!({
        "done": false,
        "llm_call_count": 2,
        "text_streamed": true,
        "exec_code": "print(\"streamed\");",
        "checkpoint": "after_work",
        "trajectory_output": "streamed\n",
    }))
}

fn check_rlm_empty_options_natural_default(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_bool(result, "/done", true, contract)?;
    require_rlm_final_value(result, &json!("done"), contract)?;
    require_rlm_exec_code(result, "finish(\"done\");", contract)?;
    require_checkpoint(result, "before_completion", contract)?;
    if result.pointer("/termination/kind").and_then(Value::as_str)
        != Some("empty_protocol_turn_options")
    {
        return Err(format!(
            "{contract} did not execute with empty protocol turn options"
        ));
    }
    Ok(json!({
        "mode": "empty_options_default",
        "natural_default": true,
        "final_value": "done",
        "exec_code": "finish(\"done\");",
    }))
}

fn check_rlm_exec_result_no_tool_call_replay(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_rlm_exec_code(
        result,
        "const x = await tools.read_file({ path: \"foo\" });",
        contract,
    )?;
    require_checkpoint(result, "after_work", contract)?;
    require_rlm_tool_call_event(result, contract)?;
    require_rlm_trajectory_omits(result, "rlm-call-1", contract)?;
    require_rlm_trajectory_error(result, None, contract)?;
    Ok(json!({
        "exec_code": "const x = await tools.read_file({ path: \"foo\" });",
        "checkpoint": "after_work",
        "tool_call_event": true,
        "trajectory_omits_tool_call_id": "rlm-call-1",
    }))
}

fn check_rlm_exec_tool_control_frame_switch_terminal(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_bool(result, "/done", true, contract)?;
    require_rlm_exec_code(
        result,
        "const x = await tools.custom_frame_switch({});",
        contract,
    )?;
    require_checkpoint(result, "before_completion", contract)?;
    require_rlm_tool_call_event(result, contract)?;
    require_rlm_agent_frame_switch(result, "next-frame", "continue", 1, contract)?;
    require_rlm_trajectory_error(result, None, contract)?;
    Ok(json!({
        "done": true,
        "exec_code": "const x = await tools.custom_frame_switch({});",
        "checkpoint": "before_completion",
        "agent_frame_switch": {
            "frame_key_material": "next-frame",
            "task": "continue",
            "initial_node_count": 1,
        },
        "tool_call_event": true,
    }))
}

fn check_rlm_exec_tool_control_fail_terminal(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_bool(result, "/done", true, contract)?;
    require_rlm_exec_code(result, "const x = await tools.custom_fail({});", contract)?;
    require_checkpoint(result, "before_completion", contract)?;
    require_rlm_tool_call_event(result, contract)?;
    require_rlm_tool_error(result, "custom_fail", "no valid result", contract)?;
    require_rlm_trajectory_error(result, None, contract)?;
    Ok(json!({
        "done": true,
        "exec_code": "const x = await tools.custom_fail({});",
        "checkpoint": "before_completion",
        "tool_error": { "tool_name": "custom_fail", "message": "no valid result" },
        "tool_call_event": true,
    }))
}

fn check_rlm_typed_finish_emits_outcome_and_done(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_bool(result, "/done", true, contract)?;
    require_u64(result, "/llm_call_count", 1, contract)?;
    require_rlm_exec_code(result, "finish({ ok: true });", contract)?;
    require_checkpoint(result, "before_completion", contract)?;
    require_rlm_final_value(result, &json!({ "ok": true }), contract)?;
    require_bool(result, "/final_message_event", false, contract)?;
    require_rlm_trajectory_error(result, None, contract)?;
    Ok(json!({
        "mode": "finish_required_schema",
        "done": true,
        "final_value": { "ok": true },
        "exec_code": "finish({ ok: true });",
        "final_message_event": false,
        "checkpoint": "before_completion",
    }))
}

fn check_rlm_natural_allows_finish_value(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_bool(result, "/done", true, contract)?;
    require_rlm_final_value(result, &json!({ "ok": true }), contract)?;
    require_rlm_exec_code(result, "finish({ ok: true });", contract)?;
    require_checkpoint(result, "before_completion", contract)?;
    Ok(json!({
        "mode": "natural",
        "final_value": { "ok": true },
        "exec_code": "finish({ ok: true });",
    }))
}

fn check_rlm_typed_schema_mismatch_repair_loop(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_u64(result, "/llm_call_count", 2, contract)?;
    require_checkpoint(result, "after_work", contract)?;
    require_rlm_exec_code(result, "finish({ missing: true });", contract)?;
    require_rlm_system_contains(result, "did not match the required output schema", contract)?;
    require_rlm_trajectory_error(result, Some("\"ok\" is a required property"), contract)?;
    Ok(json!({
        "mode": "finish_required_schema",
        "schema_feedback": "required property",
        "repair_loop_next_llm_call": true,
        "llm_call_count": 2,
    }))
}

fn check_rlm_typed_schema_any_of_mismatch(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_checkpoint(result, "after_work", contract)?;
    require_rlm_exec_code(result, "finish(true);", contract)?;
    require_rlm_system_contains(result, "did not match the required output schema", contract)?;
    require_rlm_trajectory_error(
        result,
        Some("true is not valid under any of the schemas listed in the 'anyOf' keyword"),
        contract,
    )?;
    Ok(json!({
        "mode": "finish_required_schema",
        "schema_feedback": "anyOf",
        "exec_code": "finish(true);",
    }))
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

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
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
