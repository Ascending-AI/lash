use super::*;

#[derive(Clone, Copy)]
pub(super) enum ExecFactRequirement {
    RuntimeOutcome,
    NoToolCallReplay,
    ReentersProvider,
}

pub(super) fn generated_fact(
    fact: &'static str,
    assertion: &'static str,
    events: Vec<&DeliveredBoundary>,
    observed: Value,
) -> Result<ScenarioContractGeneratedFact, String> {
    if events.is_empty() {
        return Err(format!(
            "generated semantic fact `{fact}` had no backing boundary events"
        ));
    }
    Ok(ScenarioContractGeneratedFact {
        fact,
        assertion,
        boundary_ids: events
            .iter()
            .map(|event| event.boundary_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        observed,
    })
}

pub(super) fn successful_provider_events(events: &[DeliveredBoundary]) -> Vec<&DeliveredBoundary> {
    events
        .iter()
        .filter(|event| {
            event.kind == BoundaryKind::Provider
                && event.observed.get("success").and_then(Value::as_bool) == Some(true)
                && event
                    .observed
                    .pointer("/runtime_contract/status")
                    .and_then(Value::as_str)
                    == Some("passed")
                && event
                    .payload
                    .get("expected_provider_exchange_count")
                    .and_then(Value::as_u64)
                    == event
                        .observed
                        .get("provider_exchange_count")
                        .and_then(Value::as_u64)
        })
        .collect()
}

pub(super) fn initial_provider_projection_fact(
    events: &[DeliveredBoundary],
) -> Result<ScenarioContractGeneratedFact, String> {
    let Some(provider) = successful_provider_events(events)
        .into_iter()
        .find(|event| {
            event
                .payload
                .get("expected_provider_exchange_count")
                .and_then(Value::as_u64)
                == Some(1)
                && event.payload.get("text").and_then(Value::as_str).is_some()
        })
    else {
        return Err(
            "standard initial request projection did not find a successful exchange-1 provider boundary"
                .to_string(),
        );
    };
    let expected_text = provider
        .payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("");
    let output = provider
        .observed
        .get("provider_output")
        .and_then(Value::as_str)
        .unwrap_or("");
    if expected_text.is_empty() || expected_text != output {
        return Err(format!(
            "standard initial request projection expected provider output `{expected_text}`, got `{output}`"
        ));
    }
    generated_fact(
        "standard_initial_request_projection",
        "first generated provider boundary preserves projected request text and exchange index 1",
        vec![provider],
        json!({
            "provider_boundary": provider.boundary_id,
            "actor": provider.actor_alias,
            "expected_provider_exchange_count": 1,
            "provider_output": output,
            "projected_text": expected_text,
        }),
    )
}

pub(super) fn provider_mutation_semantic_fact(
    events: &[DeliveredBoundary],
    mutation: &'static str,
    fact: &'static str,
    assertion: &'static str,
) -> Result<ScenarioContractGeneratedFact, String> {
    let Some(event) = events.iter().find(|event| {
        event.kind == BoundaryKind::ProviderMutation
            && event
                .observed
                .get("mutation")
                .or_else(|| event.payload.get("mutation"))
                .and_then(Value::as_str)
                == Some(mutation)
            && event
                .payload
                .pointer("/runtime_completion/completion_family")
                .and_then(Value::as_str)
                == Some("provider_script_mutation")
            && event
                .observed
                .pointer("/provider_parser_matrix/matrix/real_provider_parser_execution")
                .and_then(Value::as_bool)
                == Some(true)
    }) else {
        return Err(format!(
            "provider mutation semantic fact `{fact}` did not find `{mutation}` parser evidence"
        ));
    };
    let providers = provider_mutation_providers(event);
    if !MIGRATED_RUNTIME_PROVIDER_KINDS
        .iter()
        .filter(|provider| mutation != "rate_limit_error_envelope" || **provider != "google_oauth")
        .all(|provider| providers.contains(provider))
    {
        return Err(format!(
            "provider mutation `{mutation}` did not cover every migrated provider parser: {:?}",
            providers
        ));
    }
    if mutation == "rate_limit_error_envelope"
        && !provider_rate_limit_terminalized_by_scripted_parsers(std::slice::from_ref(event))
    {
        return Err(
            "rate-limit provider mutation lacked 429 retryable terminal classification proofs"
                .to_string(),
        );
    }
    if mutation == "dropped_terminal_event"
        && !provider_dropped_terminal_event_classified(std::slice::from_ref(event))
    {
        return Err(
            "dropped-terminal provider mutation lacked non-retryable parser classification proofs"
                .to_string(),
        );
    }
    generated_fact(
        fact,
        assertion,
        vec![event],
        json!({
            "provider_mutation_boundary": event.boundary_id,
            "mutation": mutation,
            "provider_kinds": providers,
            "runtime_completion_family": "provider_script_mutation",
            "real_provider_parser_execution": true,
        }),
    )
}

pub(super) fn provider_mutation_providers(event: &DeliveredBoundary) -> BTreeSet<&str> {
    event
        .observed
        .pointer("/provider_parser_matrix/matrix/provider_kinds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

pub(super) fn tool_reentry_fact(
    events: &[DeliveredBoundary],
    fact: &'static str,
    require_provider_event_release: bool,
) -> Result<ScenarioContractGeneratedFact, String> {
    let Some((tool, provider)) = tool_then_same_actor_provider(events) else {
        return Err(format!(
            "tool semantic fact `{fact}` did not find a scheduler-owned tool result followed by a successful provider re-entry for the same actor"
        ));
    };
    let provider_release = provider_event_for_turn(events, &provider.boundary_id);
    if require_provider_event_release && provider_release.is_none() {
        return Err(format!(
            "tool semantic fact `{fact}` did not find scheduler-owned provider-event release evidence for re-entry `{}`",
            provider.boundary_id
        ));
    }
    let mut fact_events = vec![tool, provider];
    if let Some(release) = provider_release {
        fact_events.push(release);
    }
    generated_fact(
        fact,
        if require_provider_event_release {
            "tool result executes once, checkpoints through scheduler-owned provider-event release, and re-enters the same actor"
        } else {
            "tool result executes once and re-enters the same actor through a later successful provider boundary"
        },
        fact_events,
        json!({
            "tool_boundary": tool.boundary_id,
            "tool_name": tool.observed.get("tool_name").cloned().unwrap_or(Value::Null),
            "tool_call_id": tool.observed.get("tool_call_id").cloned().unwrap_or(Value::Null),
            "tool_sequence": tool.sequence,
            "reentry_provider_boundary": provider.boundary_id,
            "reentry_provider_sequence": provider.sequence,
            "actor": tool.actor_alias,
            "execution_count": 1,
            "provider_event_release": provider_release.map(|event| event.boundary_id.as_str()),
        }),
    )
}

pub(super) fn tool_then_same_actor_provider(
    events: &[DeliveredBoundary],
) -> Option<(&DeliveredBoundary, &DeliveredBoundary)> {
    events
        .iter()
        .filter(|event| {
            event.kind == BoundaryKind::Tool
                && event.observed.get("runtime_tool_output").is_some()
                && event
                    .observed
                    .get("execution_count")
                    .and_then(Value::as_u64)
                    == Some(1)
                && event
                    .payload
                    .pointer("/runtime_completion/completion_family")
                    .and_then(Value::as_str)
                    == Some("tool_return")
        })
        .find_map(|tool| {
            successful_provider_events(events)
                .into_iter()
                .filter(|provider| {
                    provider.actor_alias == tool.actor_alias && provider.sequence > tool.sequence
                })
                .min_by_key(|provider| provider.sequence)
                .map(|provider| (tool, provider))
        })
}

pub(super) fn provider_event_for_turn<'a>(
    events: &'a [DeliveredBoundary],
    turn_boundary_id: &str,
) -> Option<&'a DeliveredBoundary> {
    provider_events_for_turn(events, turn_boundary_id)
        .into_iter()
        .next()
}

pub(super) fn provider_events_for_turn<'a>(
    events: &'a [DeliveredBoundary],
    turn_boundary_id: &str,
) -> Vec<&'a DeliveredBoundary> {
    events
        .iter()
        .filter(|event| {
            event.kind == BoundaryKind::ProviderEvent
                && event
                    .payload
                    .get("turn_boundary_id")
                    .and_then(Value::as_str)
                    == Some(turn_boundary_id)
                && event
                    .observed
                    .get("provider_event_release")
                    .and_then(Value::as_bool)
                    == Some(true)
        })
        .collect()
}

pub(super) fn contract_execution_event<'a>(
    events: &'a [DeliveredBoundary],
    contract: &str,
) -> Result<&'a DeliveredBoundary, String> {
    events
        .iter()
        .find(|event| {
            event.kind == BoundaryKind::Trigger
                && event
                    .observed
                    .pointer("/contract_execution/contract")
                    .and_then(Value::as_str)
                    == Some(contract)
        })
        .ok_or_else(|| {
            format!("contract execution `{contract}` was not present in generated events")
        })
}

pub(super) fn contract_execution_payload_matches_observed<'a>(
    event: &'a DeliveredBoundary,
    contract: &str,
    expected_source_scenario: &str,
) -> Result<&'a Value, String> {
    let observed = event.observed.get("contract_execution").ok_or_else(|| {
        format!(
            "contract execution boundary `{}` did not record observed execution payload",
            event.boundary_id
        )
    })?;
    if event.payload.get("contract_execution") != Some(observed) {
        return Err(format!(
            "contract execution boundary `{}` observed execution diverged from scheduler payload",
            event.boundary_id
        ));
    }
    if observed.get("contract").and_then(Value::as_str) != Some(contract) {
        return Err(format!(
            "contract execution boundary `{}` recorded wrong contract identity",
            event.boundary_id
        ));
    }
    if observed.pointer("/source/kind").and_then(Value::as_str) != Some("fixed_dst_api_execution")
        || observed
            .pointer("/source/path")
            .and_then(Value::as_str)
            .is_none()
        || observed.pointer("/source/scenario").and_then(Value::as_str)
            != Some(expected_source_scenario)
        || !is_sha256_hex_value(observed.pointer("/source/source_hash"))
        || !is_sha256_hex_value(observed.pointer("/source/result_sha256"))
    {
        return Err(format!(
            "contract execution boundary `{}` lacks fixed execution source path/scenario/hash evidence",
            event.boundary_id
        ));
    }
    if event.observed.get("semantic_proof").is_some()
        || event.payload.get("semantic_proof").is_some()
    {
        return Err(format!(
            "contract execution boundary `{}` used semantic-proof-only trigger evidence",
            event.boundary_id
        ));
    }
    contract_execution_replay_matches(observed, contract).map_err(|reason| {
        format!(
            "contract execution boundary `{}` failed fixed-source replay validation: {reason}",
            event.boundary_id
        )
    })?;
    Ok(observed)
}

pub(super) fn contract_execution_replay_matches(
    observed: &Value,
    contract: &str,
) -> Result<(), String> {
    let replayed = crate::runner::replay_contract_execution(contract)
        .map_err(|err| format!("could not re-execute `{contract}`: {err}"))?;
    if replayed.get("source") != observed.get("source") {
        return Err(format!(
            "`{contract}` source identity/hash diverged from re-execution"
        ));
    }
    if replayed.get("result") != observed.get("result") {
        return Err(format!(
            "`{contract}` result payload diverged from re-execution"
        ));
    }
    Ok(())
}

pub(super) fn is_sha256_hex_value(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|value| {
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

pub(super) fn event_by_boundary_id<'a>(
    events: &'a [DeliveredBoundary],
    boundary_id: &str,
) -> Option<&'a DeliveredBoundary> {
    events.iter().find(|event| event.boundary_id == boundary_id)
}

pub(super) fn json_array_equals(value: Option<&Value>, expected: &[&str]) -> bool {
    let Some(values) = value.and_then(Value::as_array) else {
        return false;
    };
    values.len() == expected.len()
        && values
            .iter()
            .zip(expected)
            .all(|(value, expected)| value.as_str() == Some(*expected))
}

pub(super) fn standard_protocol_execution_fact(
    events: &[DeliveredBoundary],
    contract: &'static str,
) -> Result<ScenarioContractGeneratedFact, String> {
    let (scenario, fact, assertion) = standard_protocol_contract_metadata(contract)?;
    let proof_event = contract_execution_event(events, contract)?;
    let execution = contract_execution_payload_matches_observed(proof_event, contract, scenario)?;
    let result = execution
        .get("result")
        .ok_or_else(|| format!("{contract} execution missing result"))?;
    require_standard_str(
        result,
        "/execution_api",
        "lash_core::sansio::TurnMachine",
        contract,
    )?;
    require_standard_str(
        result,
        "/driver",
        "lash_protocol_standard::StandardDriver",
        contract,
    )?;
    require_standard_bool(
        result,
        "/initial_request_contains_user_message",
        true,
        contract,
    )?;
    let contract_observed = match contract {
        "standard.initial_request_projection" => {
            require_standard_bool(result, "/done", false, contract)?;
            require_standard_u64(result, "/llm_call_count", 1, contract)?;
            json!({
                "initial_request_contains_user_message": true,
                "llm_call_count": 1,
                "done": false,
            })
        }
        "standard.empty_response_finishes" => {
            require_standard_bool(result, "/done", true, contract)?;
            require_standard_u64(result, "/llm_call_count", 1, contract)?;
            require_standard_u64(result, "/text_delta_count", 0, contract)?;
            require_standard_checkpoint(result, "before_completion", contract)?;
            require_standard_finished_outcome_contains(result, "AssistantMessage", contract)?;
            json!({
                "done": true,
                "llm_call_count": 1,
                "text_delta_count": 0,
                "checkpoint": "before_completion",
                "turn_outcome": "assistant_message",
            })
        }
        "standard.provider_error_without_checkpoint" => {
            require_standard_bool(result, "/done", true, contract)?;
            require_standard_u64(result, "/llm_call_count", 1, contract)?;
            require_standard_checkpoint_count(result, 0, contract)?;
            require_standard_error_contains(
                result,
                "LLM error: upstream provider unavailable",
                contract,
            )?;
            require_standard_stopped_outcome(result, "ProviderError", contract)?;
            json!({
                "done": true,
                "stop_reason": "provider_error",
                "checkpoint_count": 0,
                "llm_call_count": 1,
            })
        }
        "standard.native_tool_loop_reenters_model" => {
            require_standard_bool(result, "/done", false, contract)?;
            require_standard_u64(result, "/llm_call_count", 2, contract)?;
            require_standard_checkpoint(result, "after_work", contract)?;
            require_standard_tool_call(result, "tc1", "read_file", contract)?;
            json!({
                "done": false,
                "llm_call_count": 2,
                "checkpoint": "after_work",
                "tool_call": "read_file/tc1",
            })
        }
        "standard.parallel_tool_results_checkpoint_once" => {
            require_standard_bool(result, "/done", false, contract)?;
            require_standard_u64(result, "/llm_call_count", 2, contract)?;
            require_standard_checkpoint_count(result, 1, contract)?;
            require_standard_checkpoint(result, "after_work", contract)?;
            require_standard_tool_call(result, "tc1", "read_file", contract)?;
            require_standard_tool_call(result, "tc2", "read_file", contract)?;
            json!({
                "done": false,
                "llm_call_count": 2,
                "checkpoint_count": 1,
                "tool_calls": ["tc1", "tc2"],
            })
        }
        "standard.tool_failure_feedback_reenters_model" => {
            require_standard_bool(result, "/done", false, contract)?;
            require_standard_u64(result, "/llm_call_count", 2, contract)?;
            require_standard_checkpoint(result, "after_work", contract)?;
            require_standard_tool_call(result, "tc1", "search", contract)?;
            require_standard_tool_result(
                result,
                "tc1",
                "failure",
                Some("search_failed"),
                contract,
            )?;
            json!({
                "done": false,
                "llm_call_count": 2,
                "checkpoint": "after_work",
                "tool_result": "failure/search_failed",
            })
        }
        "standard.streamed_text_finalizes_once" => {
            require_standard_bool(result, "/done", true, contract)?;
            require_standard_u64(result, "/llm_call_count", 1, contract)?;
            require_standard_u64(result, "/text_delta_count", 0, contract)?;
            require_standard_checkpoint(result, "before_completion", contract)?;
            require_standard_finished_outcome_contains(result, "AssistantMessage", contract)?;
            require_standard_finished_outcome_contains(result, "streamed done", contract)?;
            json!({
                "done": true,
                "llm_call_count": 1,
                "text_delta_count": 0,
                "checkpoint": "before_completion",
                "turn_outcome": "assistant_message",
            })
        }
        other => {
            return Err(format!(
                "Standard protocol contract execution fact has no checker for `{other}`"
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

pub(super) fn standard_protocol_contract_metadata(
    contract: &str,
) -> Result<(&'static str, &'static str, &'static str), String> {
    match contract {
        "standard.initial_request_projection" => Ok((
            "standard_protocol_scenario_projects_initial_request",
            "standard_initial_request_projection_execution",
            "StandardDriver projects the user input into the first TurnMachine LLM request",
        )),
        "standard.empty_response_finishes" => Ok((
            "standard_protocol_scenario_empty_model_response_finishes_after_checkpoint",
            "standard_empty_response_finishes_execution",
            "StandardDriver sends a valid empty model response through the normal completion checkpoint and finishes successfully",
        )),
        "standard.provider_error_without_checkpoint" => Ok((
            "standard_protocol_scenario_provider_error_stops_without_checkpoint",
            "standard_provider_error_without_checkpoint_execution",
            "StandardDriver provider error stops immediately without committing a checkpoint",
        )),
        "standard.native_tool_loop_reenters_model" => Ok((
            "standard_protocol_scenario_native_tool_loop_reenters_model_after_checkpoint",
            "standard_native_tool_loop_reenters_model_execution",
            "StandardDriver native tool results checkpoint after work and re-enter the model loop",
        )),
        "standard.parallel_tool_results_checkpoint_once" => Ok((
            "standard_protocol_scenario_parallel_tool_results_checkpoint_once",
            "standard_parallel_tool_results_checkpoint_once_execution",
            "StandardDriver parallel tool results commit exactly one AfterWork checkpoint before model re-entry",
        )),
        "standard.tool_failure_feedback_reenters_model" => Ok((
            "standard_protocol_scenario_tool_failure_feedback_reenters_model_after_checkpoint",
            "standard_tool_failure_feedback_reenters_model_execution",
            "StandardDriver converts tool failure into model feedback, checkpoints, and re-enters",
        )),
        "standard.streamed_text_finalizes_once" => Ok((
            "standard_protocol_scenario_streamed_text_finishes_without_duplicate_delta",
            "standard_streamed_text_finalizes_once_execution",
            "StandardDriver streamed assistant text finalizes once without duplicate text deltas",
        )),
        other => Err(format!(
            "no Standard protocol metadata registered for `{other}`"
        )),
    }
}

pub(super) fn require_standard_bool(
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

pub(super) fn require_standard_u64(
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

pub(super) fn require_standard_str(
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

pub(super) fn require_standard_checkpoint(
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

pub(super) fn require_standard_checkpoint_count(
    result: &Value,
    expected: usize,
    contract: &str,
) -> Result<(), String> {
    let actual = result
        .get("checkpoints")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "{contract} expected {expected} checkpoint(s), found {actual}"
        ))
    }
}

pub(super) fn require_standard_error_contains(
    result: &Value,
    needle: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("errors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|error| error.contains(needle))
    {
        Ok(())
    } else {
        Err(format!("{contract} missing error containing `{needle}`"))
    }
}

pub(super) fn require_standard_stopped_outcome(
    result: &Value,
    stop_reason_needle: &str,
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
                    .is_some_and(|reason| reason.contains(stop_reason_needle))
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing stopped outcome containing `{stop_reason_needle}`"
        ))
    }
}

pub(super) fn require_standard_finished_outcome_contains(
    result: &Value,
    needle: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("turn_outcomes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|outcome| {
            outcome.get("kind").and_then(Value::as_str) == Some("finished")
                && outcome
                    .get("finish")
                    .and_then(Value::as_str)
                    .is_some_and(|finish| finish.contains(needle))
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing finished outcome containing `{needle}`"
        ))
    }
}

pub(super) fn require_standard_tool_call(
    result: &Value,
    call_id: &str,
    tool_name: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|call| {
            call.get("call_id").and_then(Value::as_str) == Some(call_id)
                && call.get("tool_name").and_then(Value::as_str) == Some(tool_name)
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing tool call `{tool_name}/{call_id}`"
        ))
    }
}

pub(super) fn require_standard_tool_result(
    result: &Value,
    call_id: &str,
    status: &str,
    error_code: Option<&str>,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("tool_results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|tool_result| {
            tool_result.get("call_id").and_then(Value::as_str) == Some(call_id)
                && tool_result.get("status").and_then(Value::as_str) == Some(status)
                && error_code.is_none_or(|code| {
                    tool_result.get("error_code").and_then(Value::as_str) == Some(code)
                })
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing tool result `{call_id}` status `{status}`"
        ))
    }
}
