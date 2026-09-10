use super::*;

pub(super) fn agent_contract_execution_fact(
    events: &[DeliveredBoundary],
    contract: &'static str,
) -> Result<ScenarioContractGeneratedFact, String> {
    let (scenario, fact, assertion) = agent_contract_metadata(contract)?;
    let proof_event = contract_execution_event(events, contract)?;
    let execution = contract_execution_payload_matches_observed(proof_event, contract, scenario)?;
    let result = execution
        .get("result")
        .ok_or_else(|| format!("{contract} execution missing result"))?;
    require_agent_bool(result, "/done", true, contract)?;
    require_agent_str(result, "/execution_api", "lash::LashCore facade", contract)?;
    let observed = match contract {
        "agent.foreground_tool_call_round_trip" => {
            require_agent_final_value(result, &json!({ "ok": true }), contract)?;
            require_agent_u64(result, "/tool_completed_count", 1, contract)?;
            require_agent_tool_output(result, "app_lookup", &json!({ "ok": true }), contract)?;
            json!({
                "final_value": { "ok": true },
                "tool_completed_count": 1,
                "tool_name": "app_lookup",
            })
        }
        "agent.started_process_tool_call_graph" => {
            require_agent_final_value(result, &json!({ "ok": true }), contract)?;
            require_agent_completed_process_entry(result, "lookup", contract)?;
            require_agent_completed_labeled_resource(
                result,
                "Lookup app state in process",
                contract,
            )?;
            json!({
                "final_value": { "ok": true },
                "completed_process": "lookup",
                "labeled_resource": "Lookup app state in process",
            })
        }
        "agent.durable_input_suspension_resolution" => {
            require_agent_final_value(result, &json!("approved"), contract)?;
            require_agent_bool(
                result,
                "/durable_input/suspended_before_resolution",
                true,
                contract,
            )?;
            require_agent_bool(result, "/durable_input/resolve_accepted", true, contract)?;
            require_agent_u64(
                result,
                "/durable_input/completed_event_count_before_resolution",
                0,
                contract,
            )?;
            require_agent_u64(result, "/durable_input/atomic_attempt_count", 1, contract)?;
            require_agent_bool(
                result,
                "/durable_input/await_tool_call_id_present",
                true,
                contract,
            )?;
            require_agent_completed_process_entry(result, "request_answer", contract)?;
            require_agent_process_event(
                result,
                "process.yield",
                "/payload/type",
                "work.input_request.opened",
                contract,
            )?;
            require_agent_no_process_event(result, "process.waiting", contract)?;
            json!({
                "final_value": "approved",
                "await_tool_call_id_present": true,
                "suspended_before_resolution": true,
                "completed_process": "request_answer",
                "process_event": "work.input_request.opened",
            })
        }
        "agent.shell_results_are_data" => {
            let expected = json!({
                "pipe_exit": 0,
                "pipe_output": "line\nline\nline\n",
                "missing_exit": 1,
                "missing_status": "completed"
            });
            require_agent_final_value(result, &expected, contract)?;
            json!({
                "final_value": expected,
                "nonzero_shell_exit_is_data": true,
                "pipeline_output": "line\nline\nline\n",
            })
        }
        "agent.shell_output_print_projection_survives" => {
            let expected = json!({
                "chars": 60000,
                "tail": "x\nx\n",
                "has_full_output_path": true
            });
            require_agent_final_value(result, &expected, contract)?;
            json!({
                "final_value": expected,
                "shell_output_chars": 60000,
                "projection_tail": "x\nx\n",
            })
        }
        "agent.started_process_subagent_spawn" => {
            require_agent_final_value(result, &json!({ "len": 2 }), contract)?;
            require_agent_completed_process_entry(result, "spawn_child", contract)?;
            require_agent_completed_labeled_resource(
                result,
                "Spawn subagent with web search",
                contract,
            )?;
            require_agent_min_u64(
                result,
                "/graph_facts/child_session_exec_completed_count",
                1,
                contract,
            )?;
            json!({
                "final_value": { "len": 2 },
                "completed_process": "spawn_child",
                "labeled_resource": "Spawn subagent with web search",
                "child_session_exec_completed_count": result.pointer("/graph_facts/child_session_exec_completed_count").cloned().unwrap_or(Value::Null),
            })
        }
        "agent.session_turn_process_child" => {
            require_agent_final_value(result, &json!({ "child": "done" }), contract)?;
            json!({
                "final_value": { "child": "done" },
                "process_child_awaited": true,
            })
        }
        "agent.nested_process_start_await" => {
            require_agent_final_value(result, &json!({ "parent": "done" }), contract)?;
            require_agent_completed_process_entry(result, "parent", contract)?;
            require_agent_completed_process_entry(result, "child", contract)?;
            require_agent_completed_labeled_node(result, "Start nested child process", contract)?;
            require_agent_min_u64(result, "/process_facts/process_count", 2, contract)?;
            require_agent_min_u64(
                result,
                "/process_facts/completed_lashlang_process_count",
                2,
                contract,
            )?;
            json!({
                "final_value": { "parent": "done" },
                "completed_processes": ["child", "parent"],
                "labeled_node": "Start nested child process",
            })
        }
        "agent.failed_child_preserves_failure_graph" => {
            require_agent_no_final_value(result, contract)?;
            require_agent_bool(result, "/process_facts/all_terminal", true, contract)?;
            require_agent_failed_labeled_resource(result, "Spawn failing subagent", contract)?;
            require_agent_min_u64(
                result,
                "/graph_facts/child_session_exec_completed_count",
                1,
                contract,
            )?;
            require_agent_bool(result, "/failure/turn_success", false, contract)?;
            require_agent_bool(result, "/failure/final_value_present", false, contract)?;
            require_agent_u64(result, "/failure/final_value_event_count", 0, contract)?;
            require_agent_min_u64(result, "/failure/failed_code_block_count", 1, contract)?;
            require_agent_bool(
                result,
                "/failure/provider_exhaustion_observed",
                false,
                contract,
            )?;
            require_agent_bool(
                result,
                "/failure/child_task_fail_reason_observed",
                true,
                contract,
            )?;
            json!({
                "final_value_present": false,
                "failed_labeled_resource": "Spawn failing subagent",
                "child_task_fail_reason": "child boom",
                "child_session_exec_completed_count": result.pointer("/graph_facts/child_session_exec_completed_count").cloned().unwrap_or(Value::Null),
                "all_processes_terminal": true,
            })
        }
        "agent.parallel_spawn_and_join" => {
            let expected = json!({ "joined": ["left", "right"] });
            require_agent_final_value(result, &expected, contract)?;
            json!({
                "final_value": expected,
                "joined": ["left", "right"],
            })
        }
        "agent.tuple_values_finish_as_json_arrays" => {
            let expected = json!({
                "first": "left",
                "tail": ["right"],
                "seen": ["left", "right"],
                "tuple": ["left", "right"],
                "nested": { "pair": ["left", "right"] }
            });
            require_agent_final_value(result, &expected, contract)?;
            let final_value = result
                .get("final_value")
                .ok_or_else(|| format!("{contract} missing concrete final value"))?;
            if !(json_array_equals(final_value.pointer("/tuple"), &["left", "right"])
                && json_array_equals(final_value.pointer("/tail"), &["right"])
                && json_array_equals(final_value.pointer("/seen"), &["left", "right"])
                && json_array_equals(final_value.pointer("/nested/pair"), &["left", "right"]))
            {
                return Err(format!(
                    "{contract} did not preserve tuple/tail/seen/nested tuple values as JSON arrays"
                ));
            }
            json!({
                "final_value": expected,
                "tuple": final_value.pointer("/tuple").cloned().unwrap_or(Value::Null),
                "tail": final_value.pointer("/tail").cloned().unwrap_or(Value::Null),
                "seen": final_value.pointer("/seen").cloned().unwrap_or(Value::Null),
                "nested_pair": final_value.pointer("/nested/pair").cloned().unwrap_or(Value::Null),
            })
        }
        other => {
            return Err(format!(
                "Agent contract execution fact has no checker for `{other}`"
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
            "observed": observed,
            "source": execution.get("source").cloned().unwrap_or(Value::Null),
        }),
    )
}

pub(super) fn agent_contract_metadata(
    contract: &str,
) -> Result<(&'static str, &'static str, &'static str), String> {
    match contract {
        "agent.foreground_tool_call_round_trip" => Ok((
            "agent_scenario_foreground_labeled_tool_call",
            "agent_foreground_tool_call_round_trip_execution",
            "Agent facade executes app_lookup and returns its concrete tool value as the final value",
        )),
        "agent.started_process_tool_call_graph" => Ok((
            "agent_scenario_started_process_labeled_tool_call",
            "agent_started_process_tool_call_graph_execution",
            "Agent facade starts a Lashlang process that executes app_lookup and records a completed labeled process graph",
        )),
        "agent.durable_input_suspension_resolution" => Ok((
            "agent_scenario_process_durable_input_request_tool",
            "agent_durable_input_suspension_resolution_execution",
            "Agent facade suspends a durable input process before external resolution and resumes to a concrete final value",
        )),
        "agent.shell_results_are_data" => Ok((
            "agent_scenario_shell_nonzero_and_pipeline_results_are_data",
            "agent_shell_results_are_data_execution",
            "Agent facade preserves shell pipeline output and nonzero shell status as final-value data",
        )),
        "agent.shell_output_print_projection_survives" => Ok((
            "agent_scenario_shell_output_survives_print_projection_in_variable",
            "agent_shell_output_print_projection_execution",
            "Agent facade keeps large shell output addressable after print projection and finishes retained metadata",
        )),
        "agent.started_process_subagent_spawn" => Ok((
            "agent_scenario_started_process_labeled_subagent_spawn",
            "agent_started_process_subagent_spawn_execution",
            "Agent facade starts a Lashlang process that spawns a default subagent, preserves the labeled child-session graph, and returns the typed child value",
        )),
        "agent.session_turn_process_child" => Ok((
            "agent_scenario_session_turn_process_child",
            "agent_session_turn_process_child_execution",
            "Agent facade starts and awaits a child process to produce a concrete final value",
        )),
        "agent.nested_process_start_await" => Ok((
            "agent_scenario_nested_process_start_await",
            "agent_nested_process_start_await_execution",
            "Agent facade starts a parent Lashlang process that starts and awaits a child process with connected graph evidence",
        )),
        "agent.failed_child_preserves_failure_graph" => Ok((
            "agent_scenario_failed_child_preserves_failure_graph",
            "agent_failed_child_preserves_failure_graph_execution",
            "Agent facade preserves a failed subagent task graph, terminal process state, and task.fail reason without provider exhaustion or false final value",
        )),
        "agent.parallel_spawn_and_join" => Ok((
            "agent_scenario_parallel_spawn_and_join",
            "agent_parallel_spawn_and_join_execution",
            "Agent facade starts two child processes and joins their concrete final values in order",
        )),
        "agent.tuple_values_finish_as_json_arrays" => Ok((
            "agent_scenario_tuple_values_finish_as_json_arrays",
            "agent_tuple_values_finish_json_arrays_execution",
            "Agent facade preserves Lashlang tuple projections as JSON arrays in final-value and runtime outcome evidence",
        )),
        other => Err(format!(
            "no Agent contract metadata registered for `{other}`"
        )),
    }
}

pub(super) fn require_agent_bool(
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

pub(super) fn require_agent_u64(
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

pub(super) fn require_agent_min_u64(
    result: &Value,
    pointer: &str,
    minimum: u64,
    contract: &str,
) -> Result<(), String> {
    if result
        .pointer(pointer)
        .and_then(Value::as_u64)
        .is_some_and(|value| value >= minimum)
    {
        Ok(())
    } else {
        Err(format!("{contract} expected {pointer}>={minimum}"))
    }
}

pub(super) fn require_agent_str(
    result: &Value,
    pointer: &str,
    expected: &str,
    contract: &str,
) -> Result<(), String> {
    if result.pointer(pointer).and_then(Value::as_str) == Some(expected) {
        Ok(())
    } else {
        Err(format!("{contract} expected {pointer}=`{expected}`"))
    }
}

pub(super) fn require_agent_final_value(
    result: &Value,
    expected: &Value,
    contract: &str,
) -> Result<(), String> {
    if result.get("final_value") == Some(expected)
        && result.pointer("/runtime_final_value_facts/semantic_value") == Some(expected)
        && result
            .pointer("/runtime_final_value_facts/outcome_kind")
            .and_then(Value::as_str)
            == Some("final_value")
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing concrete facade final value `{expected}`"
        ))
    }
}

pub(super) fn require_agent_no_final_value(result: &Value, contract: &str) -> Result<(), String> {
    if result.get("final_value") == Some(&Value::Null)
        && result
            .pointer("/failure/final_value_present")
            .and_then(Value::as_bool)
            == Some(false)
        && result
            .pointer("/failure/final_value_event_count")
            .and_then(Value::as_u64)
            == Some(0)
    {
        Ok(())
    } else {
        Err(format!("{contract} unexpectedly recorded a final value"))
    }
}

pub(super) fn require_agent_tool_output(
    result: &Value,
    name: &str,
    expected: &Value,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("tool_completed_outputs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|entry| {
            entry.get("name").and_then(Value::as_str) == Some(name)
                && entry.get("value") == Some(expected)
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing tool output `{name}` `{expected}`"
        ))
    }
}

pub(super) fn require_agent_completed_process_entry(
    result: &Value,
    entry_name: &str,
    contract: &str,
) -> Result<(), String> {
    require_agent_array_str_contains(
        result,
        "/process_facts/completed_entries",
        entry_name,
        contract,
    )
}

pub(super) fn require_agent_completed_labeled_resource(
    result: &Value,
    title: &str,
    contract: &str,
) -> Result<(), String> {
    require_agent_array_str_contains(
        result,
        "/graph_facts/completed_labeled_resources",
        title,
        contract,
    )
}

pub(super) fn require_agent_completed_labeled_node(
    result: &Value,
    title: &str,
    contract: &str,
) -> Result<(), String> {
    require_agent_array_str_contains(
        result,
        "/graph_facts/completed_labeled_nodes",
        title,
        contract,
    )
}

pub(super) fn require_agent_failed_labeled_resource(
    result: &Value,
    title: &str,
    contract: &str,
) -> Result<(), String> {
    require_agent_array_str_contains(
        result,
        "/graph_facts/failed_labeled_resources",
        title,
        contract,
    )
}

pub(super) fn require_agent_array_str_contains(
    result: &Value,
    pointer: &str,
    expected: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .pointer(pointer)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|value| value.as_str() == Some(expected))
    {
        Ok(())
    } else {
        Err(format!("{contract} missing `{expected}` in {pointer}"))
    }
}

pub(super) fn require_agent_process_event(
    result: &Value,
    event_type: &str,
    payload_pointer: &str,
    expected_payload: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("process_events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|event| {
            event.get("event_type").and_then(Value::as_str) == Some(event_type)
                && event.pointer(payload_pointer).and_then(Value::as_str) == Some(expected_payload)
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing process event `{event_type}` with {payload_pointer}=`{expected_payload}`"
        ))
    }
}

pub(super) fn require_agent_no_process_event(
    result: &Value,
    event_type: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("process_events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .all(|event| event.get("event_type").and_then(Value::as_str) != Some(event_type))
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} unexpectedly recorded process event `{event_type}`"
        ))
    }
}
