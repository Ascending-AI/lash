use super::*;

/// The Agent family's fact-spec rows. `spec` is resolved from the imported
/// contract table at compile time; the source scenario the metadata table used
/// to copy is now just `spec.test_name`.
pub(super) const AGENT_CONTRACT_FACT_SPECS: &[ContractFactSpec] = &[
    ContractFactSpec {
        spec: contract_spec(
            AGENT_SCENARIO_CONTRACTS,
            "agent.foreground_tool_call_round_trip",
        ),
        fact: "agent_foreground_tool_call_round_trip_execution",
        assertion: "Agent facade executes app_lookup and returns its concrete tool value as the final value",
        check: check_agent_foreground_tool_call_round_trip,
        extras_before: &[],
        extras_after: &[ExtraFact::ToolReentry {
            fact: "agent_foreground_tool_call_round_trip",
            require_provider_event_release: false,
        }],
    },
    ContractFactSpec {
        spec: contract_spec(
            AGENT_SCENARIO_CONTRACTS,
            "agent.started_process_tool_call_graph",
        ),
        fact: "agent_started_process_tool_call_graph_execution",
        assertion: "Agent facade starts a Lash VM process that executes app_lookup and records its completed `tools.app_lookup` effect on the process graph",
        check: check_agent_started_process_tool_call_graph,
        extras_before: &[],
        extras_after: &[ExtraFact::ToolReentry {
            fact: "agent_started_process_tool_call",
            require_provider_event_release: false,
        }],
    },
    ContractFactSpec {
        spec: contract_spec(
            AGENT_SCENARIO_CONTRACTS,
            "agent.durable_input_suspension_resolution",
        ),
        fact: "agent_durable_input_suspension_resolution_execution",
        assertion: "Agent facade suspends a durable input process before external resolution and resumes to a concrete final value",
        check: check_agent_durable_input_suspension_resolution,
        extras_before: &[],
        extras_after: &[
            ExtraFact::DurableReplay("agent_durable_input_first_and_replay"),
            ExtraFact::ObserverReconnect("agent_durable_input_observer_reconnect"),
        ],
    },
    ContractFactSpec {
        spec: contract_spec(
            AGENT_SCENARIO_CONTRACTS,
            "agent.started_process_child_spawn",
        ),
        fact: "agent_started_process_child_spawn_execution",
        assertion: "Agent facade starts a Lash VM process that spawns a default subagent, preserves the child-session graph of its `agents.spawn` effect, and returns the typed child value",
        check: check_agent_started_process_child_spawn,
        extras_before: &[],
        extras_after: &[],
    },
    ContractFactSpec {
        spec: contract_spec(AGENT_SCENARIO_CONTRACTS, "agent.session_turn_process_child"),
        fact: "agent_session_turn_process_child_execution",
        assertion: "Agent facade starts and awaits a child process to produce a concrete final value",
        check: check_agent_session_turn_process_child,
        extras_before: &[],
        extras_after: &[],
    },
    ContractFactSpec {
        spec: contract_spec(AGENT_SCENARIO_CONTRACTS, "agent.nested_process_start_await"),
        fact: "agent_nested_process_start_await_execution",
        assertion: "Agent facade starts a parent Lash VM process that starts and awaits a child process with connected graph evidence",
        check: check_agent_nested_process_start_await,
        extras_before: &[],
        extras_after: &[],
    },
    ContractFactSpec {
        spec: contract_spec(
            AGENT_SCENARIO_CONTRACTS,
            "agent.failed_child_preserves_failure_graph",
        ),
        fact: "agent_failed_child_preserves_failure_graph_execution",
        assertion: "Agent facade preserves a failed subagent task graph, terminal process state, and task.fail reason without provider exhaustion or false final value",
        check: check_agent_failed_child_preserves_failure_graph,
        extras_before: &[],
        extras_after: &[ExtraFact::BackendRetry("agent_failed_child_backend_graph")],
    },
    ContractFactSpec {
        spec: contract_spec(AGENT_SCENARIO_CONTRACTS, "agent.parallel_spawn_and_join"),
        fact: "agent_parallel_spawn_and_join_execution",
        assertion: "Agent facade starts two child processes and joins their concrete final values in order",
        check: check_agent_parallel_spawn_and_join,
        extras_before: &[],
        extras_after: &[],
    },
    ContractFactSpec {
        spec: contract_spec(
            AGENT_SCENARIO_CONTRACTS,
            "agent.tuple_values_finish_as_json_arrays",
        ),
        fact: "agent_tuple_values_finish_json_arrays_execution",
        assertion: "Agent facade preserves Lash VM tuple projections as JSON arrays in final-value and runtime outcome evidence",
        check: check_agent_tuple_values_finish_as_json_arrays,
        extras_before: &[],
        extras_after: &[],
    },
];

pub(super) fn agent_contract_execution_fact(
    events: &[DeliveredBoundary],
    row: &'static ContractFactSpec,
    memo: &ScenarioFactMemo,
) -> Result<ScenarioContractGeneratedFact, String> {
    let contract = row.spec.semantic_oracle;
    let proof_event = contract_execution_event(events, contract)?;
    // Which event proves the contract depends on the candidate's event list, so
    // the lookup always runs; what the fact says about that event does not, so
    // it is derived once per proof event for the life of the memo.
    let boundary_id = proof_event.boundary_id.clone();
    memo.fact_from_proof_event(contract, &boundary_id, || {
        agent_contract_execution_fact_from_proof_event(proof_event, row)
    })
}

fn agent_contract_execution_fact_from_proof_event(
    proof_event: &DeliveredBoundary,
    row: &'static ContractFactSpec,
) -> Result<ScenarioContractGeneratedFact, String> {
    let contract = row.spec.semantic_oracle;
    let execution =
        contract_execution_payload_matches_observed(proof_event, contract, row.spec.test_name)?;
    let result = execution
        .get("result")
        .ok_or_else(|| format!("{contract} execution missing result"))?;
    require_bool(result, "/done", true, contract)?;
    require_str(result, "/execution_api", "lash::LashCore facade", contract)?;
    let observed = (row.check)(result, contract)?;
    generated_fact(
        row.fact,
        row.assertion,
        vec![proof_event],
        json!({
            "contract_execution_boundary": proof_event.boundary_id,
            "contract": contract,
            "observed": observed,
            "source": execution.get("source").cloned().unwrap_or(Value::Null),
        }),
    )
}

fn check_agent_foreground_tool_call_round_trip(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_agent_final_value(result, &json!({ "ok": true }), contract)?;
    require_u64(result, "/tool_completed_count", 2, contract)?;
    require_agent_tool_output(result, "app_lookup", &json!({ "ok": true }), contract)?;
    require_agent_tool_output(result, "finish", &Value::Null, contract)?;
    Ok(json!({
        "final_value": { "ok": true },
        "tool_completed_count": 2,
        "tool_name": "app_lookup",
    }))
}

fn check_agent_started_process_tool_call_graph(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_agent_final_value(result, &json!({ "ok": true }), contract)?;
    require_agent_document_entry_processes(result, 1, contract)?;
    require_agent_completed_effect(result, "tools.app_lookup", contract)?;
    Ok(json!({
        "final_value": { "ok": true },
        "completed_document_entry_processes": 1,
        "completed_effect": "tools.app_lookup",
    }))
}

fn check_agent_durable_input_suspension_resolution(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_agent_final_value(result, &json!("approved"), contract)?;
    require_bool(
        result,
        "/durable_input/suspended_before_resolution",
        true,
        contract,
    )?;
    require_bool(result, "/durable_input/resolve_accepted", true, contract)?;
    require_u64(
        result,
        "/durable_input/completed_event_count_before_resolution",
        0,
        contract,
    )?;
    require_u64(result, "/durable_input/atomic_attempt_count", 1, contract)?;
    require_bool(
        result,
        "/durable_input/await_tool_call_id_present",
        true,
        contract,
    )?;
    require_agent_document_entry_processes(result, 1, contract)?;
    require_agent_process_event(
        result,
        "process.waiting",
        "/payload/wait/kind/kind",
        "call",
        contract,
    )?;
    require_agent_call_wait_pair(result, contract)?;
    Ok(json!({
        "final_value": "approved",
        "await_tool_call_id_present": true,
        "suspended_before_resolution": true,
        "completed_document_entry_processes": 1,
        "process_event": "process.waiting",
    }))
}

fn check_agent_started_process_child_spawn(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_agent_final_value(result, &json!({ "len": 2 }), contract)?;
    require_agent_document_entry_processes(result, 1, contract)?;
    require_agent_completed_process_entry(result, "spawn", contract)?;
    require_agent_completed_effect(result, "agents.spawn", contract)?;
    require_agent_min_u64(
        result,
        "/graph_facts/child_session_exec_completed_count",
        1,
        contract,
    )?;
    Ok(json!({
        "final_value": { "len": 2 },
        "completed_document_entry_processes": 1,
        "completed_process": "spawn",
        "completed_effect": "agents.spawn",
        "child_session_exec_completed_count": result.pointer("/graph_facts/child_session_exec_completed_count").cloned().unwrap_or(Value::Null),
    }))
}

fn check_agent_session_turn_process_child(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_agent_final_value(result, &json!({ "child": "done" }), contract)?;
    Ok(json!({
        "final_value": { "child": "done" },
        "process_child_awaited": true,
    }))
}

fn check_agent_nested_process_start_await(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_agent_final_value(result, &json!({ "parent": "done" }), contract)?;
    require_agent_document_entry_processes(result, 2, contract)?;
    require_agent_completed_effect(result, "processes.start", contract)?;
    require_agent_min_u64(result, "/process_facts/process_count", 2, contract)?;
    require_agent_min_u64(
        result,
        "/process_facts/completed_lash_vm_process_count",
        2,
        contract,
    )?;
    Ok(json!({
        "final_value": { "parent": "done" },
        "completed_document_entry_processes": 2,
        "completed_effect": "processes.start",
    }))
}

fn check_agent_failed_child_preserves_failure_graph(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    require_agent_no_final_value(result, contract)?;
    require_bool(result, "/process_facts/all_terminal", true, contract)?;
    require_agent_failed_effect(result, "agents.spawn", contract)?;
    require_agent_min_u64(
        result,
        "/graph_facts/child_session_exec_completed_count",
        1,
        contract,
    )?;
    require_bool(result, "/failure/turn_success", false, contract)?;
    require_bool(result, "/failure/final_value_present", false, contract)?;
    require_u64(result, "/failure/final_value_event_count", 0, contract)?;
    require_agent_min_u64(result, "/failure/failed_code_block_count", 1, contract)?;
    require_bool(
        result,
        "/failure/provider_exhaustion_observed",
        false,
        contract,
    )?;
    require_bool(
        result,
        "/failure/child_task_fail_reason_observed",
        true,
        contract,
    )?;
    Ok(json!({
        "final_value_present": false,
        "failed_effect": "agents.spawn",
        "child_task_fail_reason": "child boom",
        "child_session_exec_completed_count": result.pointer("/graph_facts/child_session_exec_completed_count").cloned().unwrap_or(Value::Null),
        "all_processes_terminal": true,
    }))
}

fn check_agent_parallel_spawn_and_join(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
    let expected = json!({ "joined": ["left", "right"] });
    require_agent_final_value(result, &expected, contract)?;
    Ok(json!({
        "final_value": expected,
        "joined": ["left", "right"],
    }))
}

fn check_agent_tuple_values_finish_as_json_arrays(
    result: &Value,
    contract: &'static str,
) -> Result<Value, String> {
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
    Ok(json!({
        "final_value": expected,
        "tuple": final_value.pointer("/tuple").cloned().unwrap_or(Value::Null),
        "tail": final_value.pointer("/tail").cloned().unwrap_or(Value::Null),
        "seen": final_value.pointer("/seen").cloned().unwrap_or(Value::Null),
        "nested_pair": final_value.pointer("/nested/pair").cloned().unwrap_or(Value::Null),
    }))
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
            == Some("finished")
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

/// A process a cell creates carries no author-chosen name, and a host may
/// replace even the runtime's derived label with its own display label:
/// neither spelling is evidence. The count a contract asserts is of the
/// completed processes whose definition reads as an entry of an admitted
/// kernel document, aggregated as `completed_document_entry_process_count`.
/// The identifying evidence alongside it is the effects the executed
/// document performs at its completed sites.
pub(crate) fn require_agent_document_entry_processes(
    result: &Value,
    expected: usize,
    contract: &str,
) -> Result<(), String> {
    let observed = result
        .pointer("/process_facts/completed_document_entry_process_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            format!("{contract} missing /process_facts/completed_document_entry_process_count")
        })?;
    if observed == expected as u64 {
        Ok(())
    } else {
        Err(format!(
            "{contract} expected {expected} completed document-entry processes, observed {observed}"
        ))
    }
}

/// The executed document performs `effect` at a site its overlay saw
/// complete. The name is the document's, never the overlay's.
pub(super) fn require_agent_completed_effect(
    result: &Value,
    effect: &str,
    contract: &str,
) -> Result<(), String> {
    require_agent_array_str_contains(result, "/graph_facts/completed_effects", effect, contract)
}

pub(super) fn require_agent_failed_effect(
    result: &Value,
    effect: &str,
    contract: &str,
) -> Result<(), String> {
    require_agent_array_str_contains(result, "/graph_facts/failed_effects", effect, contract)
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

/// The input call parks once, then resumes that same descriptor on the same
/// process. Missing, duplicated, mismatched or reversed facts are failures.
pub(super) fn require_agent_call_wait_pair(result: &Value, contract: &str) -> Result<(), String> {
    let events = result
        .get("process_events")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{contract} missing process events"))?;
    let waits: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                event.get("event_type").and_then(Value::as_str),
                Some("process.waiting" | "process.resumed")
            )
        })
        .collect();
    let [waiting, resumed] = waits.as_slice() else {
        return Err(format!("{contract} requires one call wait and one resume"));
    };
    let descriptor = waiting.pointer("/payload/wait");
    if waiting.get("event_type").and_then(Value::as_str) != Some("process.waiting")
        || resumed.get("event_type").and_then(Value::as_str) != Some("process.resumed")
        || waiting
            .pointer("/payload/wait/kind/kind")
            .and_then(Value::as_str)
            != Some("call")
        || waiting
            .pointer("/payload/wait/kind/tool_id")
            .and_then(Value::as_str)
            != Some("tool:mock_input_request")
        || !waiting
            .pointer("/payload/wait/kind/call_id")
            .and_then(Value::as_str)
            .is_some_and(|call| !call.is_empty())
        || waiting
            .pointer("/payload/wait/since_ms")
            .and_then(Value::as_u64)
            .is_none()
        || descriptor != resumed.pointer("/payload/wait")
        || !waiting
            .get("process_ref")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
        || waiting.get("process_ref") != resumed.get("process_ref")
        || !waiting
            .get("sequence")
            .and_then(Value::as_u64)
            .zip(resumed.get("sequence").and_then(Value::as_u64))
            .is_some_and(|(first, second)| first < second)
    {
        return Err(format!(
            "{contract} requires an ordered matching input call wait/resume pair"
        ));
    }
    Ok(())
}
