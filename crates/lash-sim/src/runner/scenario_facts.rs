use super::*;

pub(super) fn scenario_transition_facts(
    contract: &ScenarioContractSpec,
    selected_events: &[TraceEventLine],
) -> Result<Vec<ScenarioTransitionFact>, FixedScriptRunnerError> {
    if matches!(contract.suite, "standard" | "rlm" | "agent") {
        let events = selected_events
            .iter()
            .map(|line| line.event.clone())
            .collect::<Vec<_>>();
        return scenario_contract_generated_facts(contract, &events)
            .map(|facts| {
                facts
                    .into_iter()
                    .map(|fact| ScenarioTransitionFact {
                        fact: fact.fact.to_string(),
                        status: "passed",
                        assertion: fact.assertion,
                        boundary_ids: fact.boundary_ids,
                        observed: fact.observed,
                    })
                    .collect()
            })
            .map_err(|reason| {
                FixedScriptRunnerError::Assertion(format!(
                    "scenario contract `{}` could not prove generated semantic facts: {reason}",
                    contract.test_name
                ))
            });
    }
    let mut facts = Vec::new();
    match contract.semantic_oracle {
        "runtime.checkpoint_redrive_cancel" => {
            facts.push(queued_active_turn_fact(contract, selected_events)?);
            facts.push(cancellation_terminalization_fact(
                contract,
                selected_events,
            )?);
        }
        "runtime.queued_work_keeps_pending_input" => {
            facts.push(queued_active_turn_fact(contract, selected_events)?);
        }
        "runtime.queued_turn_input_completion" => {
            facts.push(queued_active_turn_fact(contract, selected_events)?);
            facts.push(queued_turn_followup_provider_fact(
                contract,
                selected_events,
            )?);
        }
        "runtime.command_only_queue_drain" => {
            facts.push(command_queue_drain_fact(contract, selected_events)?);
        }
        "runtime.command_before_turn_work" => {
            facts.push(trigger_wakeup_fact(contract, selected_events)?);
            facts.push(queued_active_turn_fact(contract, selected_events)?);
        }
        "runtime.advisory_lease_head_cas" => {
            facts.push(worker_stale_completion_rejection_fact(
                contract,
                selected_events,
            )?);
        }
        "runtime.stale_lease_ttl" => {
            facts.push(worker_stale_lease_ttl_fact(contract, selected_events)?);
        }
        "runtime.observation_replay_preserves_input" => {
            facts.push(observer_reconnect_transition_fact(
                contract,
                selected_events,
            )?);
        }
        "standard.empty_provider_response_error" | "standard.provider_error_without_checkpoint" => {
            facts.push(provider_terminalization_fact(contract, selected_events)?);
        }
        "standard.streamed_text_finalizes_once" | "standard.initial_request_projection" => {
            facts.push(provider_success_terminal_fact(contract, selected_events)?);
        }
        "standard.native_tool_loop_reenters_model"
        | "standard.tool_failure_feedback_reenters_model"
        | "standard.max_turns_after_tool_result"
        | "standard.parallel_tool_results_checkpoint_once" => {
            facts.push(tool_result_fact(contract, selected_events)?);
            facts.push(provider_success_terminal_fact(contract, selected_events)?);
        }
        "rlm.lashlang_cell_exec_continues"
        | "rlm.streamed_lashlang_cell_exec_persists_trajectory"
        | "rlm.exec_error_max_turn_stop"
        | "rlm.exec_tool_control_frame_switch_terminal"
        | "rlm.exec_tool_control_fail_terminal"
        | "rlm.exec_result_no_tool_call_replay" => {
            facts.push(exec_terminal_fact(contract, selected_events)?);
        }
        "rlm.typed_schema_mismatch_repair_loop" | "rlm.typed_schema_any_of_mismatch" => {
            facts.push(provider_terminalization_fact(contract, selected_events)?);
            facts.push(provider_success_terminal_fact(contract, selected_events)?);
        }
        semantic if semantic.starts_with("rlm.") => {
            facts.push(provider_success_terminal_fact(contract, selected_events)?);
        }
        "agent.durable_input_suspension_resolution" => {
            facts.push(durable_effect_replay_fact(contract, selected_events)?);
            facts.push(process_wake_duplicate_fact(contract, selected_events)?);
        }
        semantic if semantic.starts_with("agent.") => {
            facts.push(process_wake_duplicate_fact(contract, selected_events)?);
        }
        _ => {}
    }
    if facts.is_empty() {
        if contract.suite == "runtime" {
            return Err(FixedScriptRunnerError::Assertion(format!(
                "runtime scenario contract `{}` has no contract-owned transition fact; generic selected-event fallback is not allowed for runtime contracts",
                contract.test_name
            )));
        }
        facts.push(generic_transition_fact(contract, selected_events)?);
    }
    Ok(facts)
}

pub(super) fn scenario_backend_regression_reference(
    contract: &ScenarioContractSpec,
) -> Option<ScenarioBackendRegressionReference> {
    let (fixture_id, regression_contract) = match contract.semantic_oracle {
        "runtime.checkpoint_redrive_cancel"
        | "runtime.queued_work_keeps_pending_input"
        | "runtime.queued_turn_input_completion" => (
            "queued-active-turn-cancel-race",
            "active-turn queued input stays hidden, then cancellation terminalizes the pending row before any later idle claim can surface it",
        ),
        "runtime.command_before_turn_work" => (
            "trigger-wakeup-routes-process",
            "trigger occurrence records a stable source key, reserves a matching delivery, and starts process wake routing without live external input",
        ),
        "runtime.advisory_lease_head_cas" | "runtime.stale_lease_ttl" => (
            "worker-stale-completion-fenced",
            "stale worker completion is rejected by durable commit fencing while the live incarnation remains active",
        ),
        "standard.empty_provider_response_error" | "standard.provider_error_without_checkpoint" => {
            (
                "provider-protocol-terminalization",
                "scripted provider mutation matrices classify retryable 429 and dropped-terminal parser failures through every migrated provider parser",
            )
        }
        "standard.streamed_text_finalizes_once"
        | "rlm.exec_tool_control_fail_terminal"
        | "rlm.exec_tool_control_frame_switch_terminal"
        | "rlm.exec_result_no_tool_call_replay" => (
            "rlm-standard-protocol-terminal-boundaries",
            "standard provider-error terminalization and RLM exec terminal boundaries stay represented by generated transitions with dynamic backend evidence",
        ),
        "standard.max_turns_after_tool_result" => (
            "backend-retry-terminalization",
            "retryable backend conflicts advance attempts and terminate on a non-retryable production StoreError class",
        ),
        _ => return None,
    };
    Some(ScenarioBackendRegressionReference {
        fixture_id,
        status: "generated_cross_backend_valid_trace",
        regression_contract,
    })
}

fn queued_active_turn_fact(
    contract: &ScenarioContractSpec,
    selected_events: &[TraceEventLine],
) -> Result<ScenarioTransitionFact, FixedScriptRunnerError> {
    let events = selected_events
        .iter()
        .filter(|line| {
            line.event.kind == BoundaryKind::QueuedIngress
                && line
                    .event
                    .observed
                    .get("ingress_mode")
                    .and_then(Value::as_str)
                    == Some("active_turn")
                && line
                    .event
                    .observed
                    .get("input_state")
                    .and_then(Value::as_str)
                    .is_some_and(|state| state.starts_with("pending"))
                && line
                    .event
                    .observed
                    .get("source_key")
                    .and_then(Value::as_str)
                    .is_some()
        })
        .collect::<Vec<_>>();
    let observed = json!({
        "queued_inputs": events.iter().map(|line| json!({
            "boundary_id": line.event.boundary_id,
            "source_key": line.event.observed.get("source_key").cloned().unwrap_or(Value::Null),
            "input_id": line.event.observed.get("input_id").cloned().unwrap_or(Value::Null),
            "input_state": line.event.observed.get("input_state").cloned().unwrap_or(Value::Null),
            "ingress_mode": line.event.observed.get("ingress_mode").cloned().unwrap_or(Value::Null),
        })).collect::<Vec<_>>(),
    });
    require_transition_fact(
        contract,
        "active_turn_input_queued_hidden",
        "active-turn queued input has stable source key and remains pending/hidden until terminalized",
        events,
        observed,
    )
}

fn queued_turn_followup_provider_fact(
    contract: &ScenarioContractSpec,
    selected_events: &[TraceEventLine],
) -> Result<ScenarioTransitionFact, FixedScriptRunnerError> {
    let mut fact_events = Vec::new();
    for queued in selected_events.iter().filter(|line| {
        line.event.kind == BoundaryKind::QueuedIngress
            && line
                .event
                .observed
                .get("source_key")
                .and_then(Value::as_str)
                .is_some()
    }) {
        if let Some(provider) = selected_events
            .iter()
            .filter(|line| {
                line.trace_alias == queued.trace_alias
                    && line.event.kind == BoundaryKind::Provider
                    && line.event.actor_alias == queued.event.actor_alias
                    && line.event.sequence > queued.event.sequence
                    && line.event.observed.get("success").and_then(Value::as_bool) == Some(true)
            })
            .min_by_key(|line| line.event.sequence)
        {
            fact_events.push(queued);
            fact_events.push(provider);
            let observed = json!({
                "queued_boundary": queued.event.boundary_id,
                "provider_boundary": provider.event.boundary_id,
                "trace_alias": queued.trace_alias,
                "actor": queued.event.actor_alias,
                "source_key": queued.event.observed.get("source_key").cloned().unwrap_or(Value::Null),
                "provider_exchange_count": provider.event.observed.get("provider_exchange_count").cloned().unwrap_or(Value::Null),
            });
            return require_transition_fact(
                contract,
                "queued_turn_input_followed_by_provider_completion",
                "queued turn input evidence is followed by a same-trace same-actor provider completion",
                fact_events,
                observed,
            );
        }
    }
    require_transition_fact(
        contract,
        "queued_turn_input_followed_by_provider_completion",
        "queued turn input evidence is followed by a same-trace same-actor provider completion",
        fact_events,
        json!({ "queued_turn_completion": false }),
    )
}

fn cancellation_terminalization_fact(
    contract: &ScenarioContractSpec,
    selected_events: &[TraceEventLine],
) -> Result<ScenarioTransitionFact, FixedScriptRunnerError> {
    let events = selected_events
        .iter()
        .filter(|line| {
            line.event.kind == BoundaryKind::Cancellation
                && line
                    .event
                    .observed
                    .get("cancelled")
                    .and_then(Value::as_bool)
                    == Some(true)
                && line
                    .event
                    .observed
                    .get("target")
                    .and_then(Value::as_str)
                    .is_some()
        })
        .collect::<Vec<_>>();
    let observed = json!({
        "terminalizations": events.iter().map(|line| json!({
            "boundary_id": line.event.boundary_id,
            "target": line.event.observed.get("target").cloned().unwrap_or(Value::Null),
            "cancel_outcome": line.event.observed.get("cancel_outcome").cloned().unwrap_or(Value::Null),
        })).collect::<Vec<_>>(),
    });
    require_transition_fact(
        contract,
        "cancellation_terminalized_pending_input",
        "cancellation targets a generated queued input and returns a terminal cancelled outcome",
        events,
        observed,
    )
}

fn trigger_wakeup_fact(
    contract: &ScenarioContractSpec,
    selected_events: &[TraceEventLine],
) -> Result<ScenarioTransitionFact, FixedScriptRunnerError> {
    let events = selected_events
        .iter()
        .filter(|line| {
            line.event.kind == BoundaryKind::Trigger
                && line
                    .event
                    .observed
                    .get("trigger_delivered")
                    .and_then(Value::as_bool)
                    == Some(true)
                && line
                    .event
                    .observed
                    .get("started_process")
                    .and_then(Value::as_bool)
                    == Some(true)
                && line
                    .event
                    .observed
                    .get("reservation_count")
                    .and_then(Value::as_u64)
                    .is_some_and(|count| count > 0)
        })
        .collect::<Vec<_>>();
    let observed = json!({
        "trigger_deliveries": events.iter().map(|line| json!({
            "boundary_id": line.event.boundary_id,
            "source_key": line.event.observed.get("source_key").cloned().unwrap_or(Value::Null),
            "occurrence_id": line.event.observed.get("occurrence_id").cloned().unwrap_or(Value::Null),
            "reservation_count": line.event.observed.get("reservation_count").cloned().unwrap_or(Value::Null),
            "started_process": true,
        })).collect::<Vec<_>>(),
    });
    require_transition_fact(
        contract,
        "trigger_routes_process_wakeup",
        "trigger occurrence records a stable source key, reserves matching delivery, and starts process routing",
        events,
        observed,
    )
}

fn command_queue_drain_fact(
    contract: &ScenarioContractSpec,
    selected_events: &[TraceEventLine],
) -> Result<ScenarioTransitionFact, FixedScriptRunnerError> {
    let queued_events = selected_events
        .iter()
        .filter(|line| {
            line.event.kind == BoundaryKind::QueuedIngress
                && line
                    .event
                    .observed
                    .get("source_key")
                    .and_then(Value::as_str)
                    .is_some_and(|source_key| !source_key.is_empty())
        })
        .collect::<Vec<_>>();
    let lease_events = selected_events
        .iter()
        .filter(|line| {
            line.event.kind == BoundaryKind::LeaseTime
                && line
                    .event
                    .observed
                    .pointer("/runtime_lease_probe/real_lease_store")
                    .and_then(Value::as_bool)
                    == Some(true)
                && line
                    .event
                    .observed
                    .pointer("/runtime_lease_probe/session_execution_lease_fencing_token")
                    .and_then(Value::as_u64)
                    .is_some()
        })
        .collect::<Vec<_>>();
    let mut events = Vec::new();
    events.extend(queued_events.iter().copied());
    events.extend(lease_events.iter().copied());
    let observed = json!({
        "queued_inputs": queued_events.iter().map(|line| json!({
            "boundary_id": line.event.boundary_id,
            "source_key": line.event.observed.get("source_key").cloned().unwrap_or(Value::Null),
            "input_state": line.event.observed.get("input_state").cloned().unwrap_or(Value::Null),
            "ingress_mode": line.event.observed.get("ingress_mode").cloned().unwrap_or(Value::Null),
        })).collect::<Vec<_>>(),
        "lease_fences": lease_events.iter().map(|line| json!({
            "boundary_id": line.event.boundary_id,
            "session": line.event.actor_alias,
            "fencing_token": line.event.observed.pointer("/runtime_lease_probe/session_execution_lease_fencing_token").cloned().unwrap_or(Value::Null),
            "real_lease_store": true,
        })).collect::<Vec<_>>(),
    });
    require_transition_fact(
        contract,
        "command_queue_drains_with_real_lease_fence",
        "command-only queued work carries scheduler-owned source keys and drains under real session-execution-lease fencing tokens",
        events,
        observed,
    )
}

fn process_wake_duplicate_fact(
    contract: &ScenarioContractSpec,
    selected_events: &[TraceEventLine],
) -> Result<ScenarioTransitionFact, FixedScriptRunnerError> {
    let mut by_dedupe_key: BTreeMap<String, Vec<&TraceEventLine>> = BTreeMap::new();
    for line in selected_events
        .iter()
        .filter(|line| line.event.kind == BoundaryKind::ProcessWake)
    {
        if let Some(dedupe_key) = line
            .event
            .observed
            .get("dedupe_key")
            .and_then(Value::as_str)
        {
            by_dedupe_key
                .entry(dedupe_key.to_string())
                .or_default()
                .push(line);
        }
    }
    let events = by_dedupe_key
        .values()
        .find(|events| {
            let strict_claim_dedupe = events.len() >= 2
                && events.iter().any(|line| {
                    line.event
                        .observed
                        .get("claimed_once")
                        .and_then(Value::as_bool)
                        == Some(true)
                });
            let in_flight_rejection = events.len() >= 2
                && events.iter().any(|line| {
                    line.event
                        .observed
                        .get("lease_busy")
                        .and_then(Value::as_bool)
                        == Some(true)
                        && line
                            .event
                            .observed
                            .pointer("/runtime_queued_work/enqueued")
                            .and_then(Value::as_bool)
                            == Some(false)
                });
            strict_claim_dedupe || in_flight_rejection
        })
        .cloned()
        .unwrap_or_default();
    let observed = json!({
        "dedupe_keys": by_dedupe_key.iter().map(|(dedupe_key, events)| json!({
            "dedupe_key": dedupe_key,
            "delivery_count": events.len(),
            "claimed_once": events.iter().any(|line| line.event.observed.get("claimed_once").and_then(Value::as_bool) == Some(true)),
            "lease_busy_rejected": events.iter().any(|line| {
                line.event.observed.get("lease_busy").and_then(Value::as_bool) == Some(true)
                    && line.event.observed.pointer("/runtime_queued_work/enqueued").and_then(Value::as_bool) == Some(false)
            }),
            "boundary_ids": boundary_ids(events),
        })).collect::<Vec<_>>(),
    });
    require_transition_fact(
        contract,
        "duplicate_process_wake_idempotent",
        "duplicate process wake deliveries share a dedupe key and are terminalized by queued-work claim dedupe or in-flight lease rejection",
        events,
        observed,
    )
}

fn observer_reconnect_transition_fact(
    contract: &ScenarioContractSpec,
    selected_events: &[TraceEventLine],
) -> Result<ScenarioTransitionFact, FixedScriptRunnerError> {
    let events = selected_events
        .iter()
        .filter(|line| {
            line.event.kind == BoundaryKind::Observer
                && line
                    .event
                    .observed
                    .get("reconnected")
                    .and_then(Value::as_bool)
                    == Some(true)
                && line
                    .event
                    .observed
                    .get("turn_index")
                    .and_then(Value::as_u64)
                    .is_some()
                && line
                    .event
                    .observed
                    .pointer("/observer_invariants/session_id")
                    .and_then(Value::as_bool)
                    == Some(true)
                && line
                    .event
                    .observed
                    .pointer("/observer_invariants/turn_index_converged")
                    .and_then(Value::as_bool)
                    == Some(true)
                && line
                    .event
                    .observed
                    .pointer("/observer_invariants/transcript_message_count_converged")
                    .and_then(Value::as_bool)
                    == Some(true)
        })
        .collect::<Vec<_>>();
    let observed = json!({
        "observer_reconnects": events.iter().map(|line| json!({
            "boundary_id": line.event.boundary_id,
            "session": line.event.actor_alias,
            "turn_index": line.event.observed.get("turn_index").cloned().unwrap_or(Value::Null),
            "graph_node_count": line.event.observed.get("graph_node_count").cloned().unwrap_or(Value::Null),
            "transcript_message_count": line.event.observed.get("transcript_message_count").cloned().unwrap_or(Value::Null),
            "observer_invariants": line.event.observed.get("observer_invariants").cloned().unwrap_or(Value::Null),
        })).collect::<Vec<_>>(),
    });
    require_transition_fact(
        contract,
        "observer_reconnect_replays_original_input_state",
        "observer reconnect boundary reads a concrete session observation with converged session id, turn index, graph, and transcript state",
        events,
        observed,
    )
}

fn worker_stale_completion_rejection_fact(
    contract: &ScenarioContractSpec,
    selected_events: &[TraceEventLine],
) -> Result<ScenarioTransitionFact, FixedScriptRunnerError> {
    let events = selected_events
        .iter()
        .filter(|line| {
            line.event.kind == BoundaryKind::Worker
                && line
                    .event
                    .observed
                    .get("stale_completion_rejected")
                    .and_then(Value::as_bool)
                    == Some(true)
                && line.event.observed.get("runtime_active_lease").is_some()
                && line
                    .event
                    .observed
                    .get("runtime_stale_completion")
                    .is_some()
                && line
                    .event
                    .observed
                    .pointer("/runtime_active_lease/fencing_token")
                    .and_then(Value::as_u64)
                    > line
                        .event
                        .observed
                        .pointer("/runtime_stale_completion/fencing_token")
                        .and_then(Value::as_u64)
        })
        .collect::<Vec<_>>();
    let observed = json!({
        "stale_completions": events.iter().map(|line| json!({
            "boundary_id": line.event.boundary_id,
            "active_fencing_token": line.event.observed.pointer("/runtime_active_lease/fencing_token").cloned().unwrap_or(Value::Null),
            "stale_fencing_token": line.event.observed.pointer("/runtime_stale_completion/fencing_token").cloned().unwrap_or(Value::Null),
            "stale_completion_rejected": true,
        })).collect::<Vec<_>>(),
    });
    require_transition_fact(
        contract,
        "lease_release_rejects_stale_completion",
        "stale worker completion carries an older fence and is rejected while the live lease remains active",
        events,
        observed,
    )
}

fn worker_stale_lease_ttl_fact(
    contract: &ScenarioContractSpec,
    selected_events: &[TraceEventLine],
) -> Result<ScenarioTransitionFact, FixedScriptRunnerError> {
    let events = selected_events
        .iter()
        .filter(|line| {
            line.event.kind == BoundaryKind::Worker
                && line
                    .event
                    .observed
                    .get("lease_owner_changed")
                    .and_then(Value::as_bool)
                    == Some(true)
                && line
                    .event
                    .observed
                    .pointer("/runtime_worker_store/session_execution_lease_acquired_after_ttl")
                    .and_then(Value::as_bool)
                    == Some(true)
                && line
                    .event
                    .observed
                    .pointer("/runtime_worker_store/worker_owned_work/second_owner_resumed_work")
                    .and_then(Value::as_bool)
                    == Some(true)
                && line
                    .event
                    .observed
                    .pointer("/runtime_worker_store/worker_owned_work/second_owner_outranks_first")
                    .and_then(Value::as_bool)
                    == Some(true)
        })
        .collect::<Vec<_>>();
    let observed = json!({
        "stale_lease_takeovers": events.iter().map(|line| json!({
            "boundary_id": line.event.boundary_id,
            "initial_owner": line.event.observed.get("initial_owner").cloned().unwrap_or(Value::Null),
            "active_owner": line.event.observed.get("active_owner").cloned().unwrap_or(Value::Null),
            "source_key": line.event.observed.pointer("/runtime_worker_store/worker_owned_work/source_key").cloned().unwrap_or(Value::Null),
            "second_owner_resumed_work": true,
            "second_owner_outranks_first": true,
        })).collect::<Vec<_>>(),
    });
    require_transition_fact(
        contract,
        "stale_lease_ttl_takeover_resumes_worker_owned_work",
        "successor worker waits for the stale lease TTL, acquires a higher fence, and resumes the owned work",
        events,
        observed,
    )
}

fn provider_terminalization_fact(
    contract: &ScenarioContractSpec,
    selected_events: &[TraceEventLine],
) -> Result<ScenarioTransitionFact, FixedScriptRunnerError> {
    let events = selected_events
        .iter()
        .filter(|line| {
            line.event.kind == BoundaryKind::ProviderMutation
                && line
                    .event
                    .observed
                    .pointer("/provider_parser_matrix/matrix/real_provider_parser_execution")
                    .and_then(Value::as_bool)
                    == Some(true)
        })
        .collect::<Vec<_>>();
    let observed = json!({
        "provider_mutations": events.iter().map(|line| json!({
            "boundary_id": line.event.boundary_id,
            "mutation": line.event.observed.get("mutation").cloned().unwrap_or(Value::Null),
            "parser_matrix_digest": line.event.observed.pointer("/provider_parser_matrix/digest").cloned().unwrap_or(Value::Null),
            "real_provider_parser_execution": true,
        })).collect::<Vec<_>>(),
    });
    require_transition_fact(
        contract,
        "provider_failure_terminalized_by_parser_matrix",
        "scripted provider failure reaches real migrated provider parsers and terminal classification",
        events,
        observed,
    )
}

fn provider_success_terminal_fact(
    contract: &ScenarioContractSpec,
    selected_events: &[TraceEventLine],
) -> Result<ScenarioTransitionFact, FixedScriptRunnerError> {
    let events = selected_events
        .iter()
        .filter(|line| {
            line.event.kind == BoundaryKind::Provider
                && line.event.observed.get("success").and_then(Value::as_bool) == Some(true)
                && line
                    .event
                    .observed
                    .pointer("/runtime_contract/status")
                    .and_then(Value::as_str)
                    == Some("passed")
        })
        .collect::<Vec<_>>();
    let observed = json!({
        "provider_turns": events.iter().map(|line| json!({
            "boundary_id": line.event.boundary_id,
            "provider_kind": line.event.observed.get("provider_kind").cloned().unwrap_or(Value::Null),
            "turn_index": line.event.observed.get("turn_index").cloned().unwrap_or(Value::Null),
            "runtime_contract": line.event.observed.get("runtime_contract").cloned().unwrap_or(Value::Null),
        })).collect::<Vec<_>>(),
    });
    require_transition_fact(
        contract,
        "provider_success_terminal_boundary",
        "provider turn terminates through the scripted runtime provider path and passes runtime contract checks",
        events,
        observed,
    )
}

fn tool_result_fact(
    contract: &ScenarioContractSpec,
    selected_events: &[TraceEventLine],
) -> Result<ScenarioTransitionFact, FixedScriptRunnerError> {
    let events = selected_events
        .iter()
        .filter(|line| {
            line.event.kind == BoundaryKind::Tool
                && line.event.observed.get("runtime_tool_output").is_some()
        })
        .collect::<Vec<_>>();
    let observed = json!({
        "tool_results": events.iter().map(|line| json!({
            "boundary_id": line.event.boundary_id,
            "tool_name": line.event.observed.get("tool_name").cloned().unwrap_or(Value::Null),
            "tool_call_id": line.event.observed.get("tool_call_id").cloned().unwrap_or(Value::Null),
            "runtime_effect": line.event.observed.get("runtime_effect").cloned().unwrap_or(Value::Null),
        })).collect::<Vec<_>>(),
    });
    require_transition_fact(
        contract,
        "tool_result_checkpoint_boundary",
        "tool result crosses runtime effect-controller output and remains available for protocol re-entry",
        events,
        observed,
    )
}

fn exec_terminal_fact(
    contract: &ScenarioContractSpec,
    selected_events: &[TraceEventLine],
) -> Result<ScenarioTransitionFact, FixedScriptRunnerError> {
    let events = selected_events
        .iter()
        .filter(|line| {
            line.event.kind == BoundaryKind::ExecCode
                && line.event.observed.get("runtime_effect_outcome").is_some()
        })
        .collect::<Vec<_>>();
    let observed = json!({
        "exec_boundaries": events.iter().map(|line| json!({
            "boundary_id": line.event.boundary_id,
            "exit_code": line.event.observed.get("exit_code").cloned().unwrap_or(Value::Null),
            "runtime_effect": line.event.observed.get("runtime_effect").cloned().unwrap_or(Value::Null),
            "runtime_effect_outcome_type": line.event.observed.pointer("/runtime_effect_outcome/type").cloned().unwrap_or(Value::Null),
        })).collect::<Vec<_>>(),
    });
    require_transition_fact(
        contract,
        "rlm_exec_terminal_boundary",
        "exec-code result crosses runtime effect-controller outcome and is available to RLM terminal state-machine contracts",
        events,
        observed,
    )
}

fn durable_effect_replay_fact(
    contract: &ScenarioContractSpec,
    selected_events: &[TraceEventLine],
) -> Result<ScenarioTransitionFact, FixedScriptRunnerError> {
    let mut by_key: BTreeMap<String, Vec<&TraceEventLine>> = BTreeMap::new();
    for line in selected_events
        .iter()
        .filter(|line| line.event.kind == BoundaryKind::DurableEffect)
    {
        if let Some(key) = line
            .event
            .observed
            .get("durable_key")
            .and_then(Value::as_str)
        {
            by_key.entry(key.to_string()).or_default().push(line);
        }
    }
    let events = by_key
        .values()
        .find(|events| {
            events.iter().any(|line| {
                line.event.observed.get("replayed").and_then(Value::as_bool) == Some(false)
                    && line
                        .event
                        .observed
                        .pointer("/runtime_effect/local_executor_called")
                        .and_then(Value::as_bool)
                        == Some(true)
            }) && events.iter().any(|line| {
                line.event.observed.get("replayed").and_then(Value::as_bool) == Some(true)
                    && line
                        .event
                        .observed
                        .pointer("/runtime_effect/local_executor_called")
                        .and_then(Value::as_bool)
                        == Some(false)
            })
        })
        .cloned()
        .unwrap_or_default();
    let observed = json!({
        "durable_keys": by_key.iter().map(|(key, events)| json!({
            "durable_key": key,
            "boundary_ids": boundary_ids(events),
            "first_execution": events.iter().any(|line| line.event.observed.get("replayed").and_then(Value::as_bool) == Some(false)),
            "replay": events.iter().any(|line| line.event.observed.get("replayed").and_then(Value::as_bool) == Some(true)),
        })).collect::<Vec<_>>(),
    });
    require_transition_fact(
        contract,
        "durable_effect_replay_idempotent",
        "durable effect executes locally once and replay returns stored history without re-execution",
        events,
        observed,
    )
}

fn generic_transition_fact(
    contract: &ScenarioContractSpec,
    selected_events: &[TraceEventLine],
) -> Result<ScenarioTransitionFact, FixedScriptRunnerError> {
    let events = selected_events.iter().collect::<Vec<_>>();
    let observed = json!({
        "selected_event_count": events.len(),
        "boundary_kinds": events
            .iter()
            .map(|line| format!("{:?}", line.event.kind))
            .collect::<BTreeSet<_>>(),
    });
    require_transition_fact(
        contract,
        "generated_transition_evidence_present",
        "scenario contract selected generated trace events for its required state transition",
        events,
        observed,
    )
}

fn require_transition_fact(
    contract: &ScenarioContractSpec,
    fact: &'static str,
    assertion: &'static str,
    events: Vec<&TraceEventLine>,
    observed: Value,
) -> Result<ScenarioTransitionFact, FixedScriptRunnerError> {
    if events.is_empty() {
        return Err(FixedScriptRunnerError::Assertion(format!(
            "scenario contract `{}` could not prove transition fact `{fact}`",
            contract.test_name
        )));
    }
    Ok(ScenarioTransitionFact {
        fact: fact.to_string(),
        status: "passed",
        assertion,
        boundary_ids: boundary_ids(&events),
        observed,
    })
}

fn boundary_ids(events: &[&TraceEventLine]) -> Vec<String> {
    events
        .iter()
        .map(|line| line.event.boundary_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
