use super::*;

/// Scenario-contract oracle vector. Every contract emits its own named,
/// failing-capable verdict; suite-level coverage manifests are deliberately not
/// used as backing oracles because they let generated packages look
/// per-contract while sharing the same semantic proof.
pub fn scenario_contract_oracles(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> Vec<OracleVerdict> {
    all_scenario_contracts()
        .map(|contract| scenario_contract_oracle(contract, events, summary))
        .collect()
}

pub(super) fn all_scenario_contracts() -> impl Iterator<Item = &'static ScenarioContractSpec> {
    [
        RUNTIME_SCENARIO_CONTRACTS,
        STANDARD_PROTOCOL_SCENARIO_CONTRACTS,
        RLM_PROTOCOL_SCENARIO_CONTRACTS,
        AGENT_SCENARIO_CONTRACTS,
    ]
    .into_iter()
    .flat_map(|contracts| contracts.iter())
}

pub fn scenario_contract_mini_oracles(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> Vec<OracleVerdict> {
    vec![
        mini_runtime_queued_input_hidden(events),
        mini_runtime_cancellation_prevents_idle_claim(events),
        mini_runtime_process_wake_duplicate_rejected(events),
        mini_runtime_stale_lease_commit_rejected(events, summary),
        mini_standard_streamed_text_finalizes_once(events),
        mini_standard_provider_error_without_checkpoint(events),
        mini_standard_tool_loop_reenters(events),
        mini_rlm_finish_required_prose_repair(events, summary),
        mini_rlm_schema_mismatch_repair(events),
        mini_rlm_lashlang_cell_exec_continues(events),
        mini_agent_durable_input_resolution(events),
        mini_agent_child_failure_graph(events, summary),
        mini_agent_parallel_spawn_join(events, summary),
    ]
}

pub(super) fn mini_runtime_queued_input_hidden(events: &[DeliveredBoundary]) -> OracleVerdict {
    let Some(queued) = first_event(events, BoundaryKind::QueuedIngress) else {
        return OracleVerdict::failed(
            SCENARIO_MINI_RUNTIME_QUEUED_HIDDEN_ORACLE,
            "no queued input boundary was generated",
        );
    };
    let queued_text = queued
        .payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("");
    let leaked = events.iter().any(|event| {
        event.kind == BoundaryKind::Provider
            && event.actor_alias == queued.actor_alias
            && event
                .observed
                .get("provider_output")
                .and_then(Value::as_str)
                .is_some_and(|output| !queued_text.is_empty() && output.contains(queued_text))
    });
    if leaked {
        OracleVerdict::failed(
            SCENARIO_MINI_RUNTIME_QUEUED_HIDDEN_ORACLE,
            format!(
                "queued input `{}` leaked into a provider turn before explicit claim",
                queued.boundary_id
            ),
        )
    } else {
        OracleVerdict::passed(
            SCENARIO_MINI_RUNTIME_QUEUED_HIDDEN_ORACLE,
            "queued input stayed hidden from provider turns while the generated live turn continued",
        )
    }
}

pub(super) fn mini_runtime_cancellation_prevents_idle_claim(
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    let queued = events
        .iter()
        .filter(|event| event.kind == BoundaryKind::QueuedIngress)
        .map(|event| event.boundary_id.as_str())
        .collect::<BTreeSet<_>>();
    let Some(cancel) = events.iter().find(|event| {
        event.kind == BoundaryKind::Cancellation
            && event
                .observed
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    }) else {
        let outcomes = events
            .iter()
            .filter(|event| event.kind == BoundaryKind::Cancellation)
            .map(|event| {
                format!(
                    "{}={}",
                    event.boundary_id,
                    event
                        .observed
                        .get("cancel_outcome")
                        .and_then(Value::as_str)
                        .unwrap_or("missing")
                )
            })
            .collect::<Vec<_>>();
        return OracleVerdict::failed(
            SCENARIO_MINI_RUNTIME_CANCEL_IDLE_ORACLE,
            format!(
                "no cancellation boundary reported a real runtime cancellation; outcomes={}",
                outcomes.join(",")
            ),
        );
    };
    let target = cancel
        .observed
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("");
    if queued.contains(target) {
        OracleVerdict::passed(
            SCENARIO_MINI_RUNTIME_CANCEL_IDLE_ORACLE,
            "cancelled queued input targets a generated queued boundary and prevents later idle claim",
        )
    } else {
        OracleVerdict::failed(
            SCENARIO_MINI_RUNTIME_CANCEL_IDLE_ORACLE,
            format!("cancellation target `{target}` was not a generated queued boundary"),
        )
    }
}

pub(super) fn mini_runtime_process_wake_duplicate_rejected(
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    let mut source_events: BTreeMap<String, Vec<&DeliveredBoundary>> = BTreeMap::new();
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::ProcessWake)
    {
        if let Some(source_key) = process_wake_source_key(event) {
            source_events.entry(source_key).or_default().push(event);
        }
    }
    if source_events.values().any(|events| {
        let claims = events
            .iter()
            .filter_map(|event| event.observed.get("claimed_once").and_then(Value::as_bool))
            .collect::<Vec<_>>();
        let strict_claim_dedupe =
            claims.iter().filter(|claimed| **claimed).count() == 1 && claims.contains(&false);
        let in_flight_rejection = events.iter().any(|event| {
            event
                .observed
                .get("lease_busy")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && event
                    .observed
                    .pointer("/runtime_queued_work/enqueued")
                    .and_then(Value::as_bool)
                    == Some(false)
        }) && claims.contains(&false);
        strict_claim_dedupe || in_flight_rejection
    }) {
        OracleVerdict::passed(
            SCENARIO_MINI_RUNTIME_PROCESS_WAKE_DEDUPE_ORACLE,
            "duplicate process wake used the same structural process/event source key and was claimed at most once",
        )
    } else {
        OracleVerdict::failed(
            SCENARIO_MINI_RUNTIME_PROCESS_WAKE_DEDUPE_ORACLE,
            "no structural process/event source key showed a claim/rejection pair or in-flight lease rejection",
        )
    }
}

pub(super) fn mini_runtime_stale_lease_commit_rejected(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> OracleVerdict {
    let stale_rejected = worker_runtime_lease_dto_observed(events)
        && summary.workers.iter().any(|worker| {
            worker.active_fencing_token > 1 && worker.stale_completion_rejections > 0
        });
    verdict_from_bool(
        SCENARIO_MINI_RUNTIME_STALE_LEASE_ORACLE,
        stale_rejected,
        "stale worker lease completion was rejected while the reclaimed live lease stayed renewable",
        "worker lease evidence did not prove stale completion rejection",
    )
}

pub(super) fn mini_standard_streamed_text_finalizes_once(
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    let Some(provider) = events.iter().find(|event| {
        event.kind == BoundaryKind::Provider
            && provider_completion_units(event)
                .iter()
                .filter(|unit| unit.contains("sse"))
                .count()
                >= 2
    }) else {
        return OracleVerdict::failed(
            SCENARIO_MINI_STANDARD_STREAM_FINALIZE_ORACLE,
            "no streamed provider completion with multiple scheduler-owned SSE units was observed",
        );
    };
    let expected = provider
        .payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("");
    let output = provider
        .observed
        .get("provider_output")
        .and_then(Value::as_str)
        .unwrap_or("");
    let occurrences = if expected.is_empty() {
        0
    } else {
        output.matches(expected).count()
    };
    verdict_from_bool(
        SCENARIO_MINI_STANDARD_STREAM_FINALIZE_ORACLE,
        occurrences == 1,
        "streamed provider output contains exactly one final assistant text projection",
        "streamed provider output did not finalize exactly once",
    )
}

pub(super) fn mini_standard_provider_error_without_checkpoint(
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    let failure_before_provider = events.iter().any(|event| {
        matches!(
            event.kind,
            BoundaryKind::ProviderMutation | BoundaryKind::BackendFailure
        ) && event.payload.get("runtime_completion").is_some()
            && event.sequence
                < next_provider_sequence(events, &event.actor_alias).unwrap_or(usize::MAX)
    });
    verdict_from_bool(
        SCENARIO_MINI_STANDARD_PROVIDER_ERROR_ORACLE,
        failure_before_provider && provider_mutation_parser_matrix_observed(events),
        "provider failure/mutation completed through scheduler before any later successful checkpointed provider turn",
        "no scheduler-owned provider failure before checkpoint/next provider turn was observed",
    )
}

pub(super) fn mini_standard_tool_loop_reenters(events: &[DeliveredBoundary]) -> OracleVerdict {
    let Some(tool) = first_event(events, BoundaryKind::Tool) else {
        return OracleVerdict::failed(
            SCENARIO_MINI_STANDARD_TOOL_REENTRY_ORACLE,
            "no tool boundary was generated",
        );
    };
    let reentered = events.iter().any(|event| {
        event.kind == BoundaryKind::Provider
            && event.actor_alias == tool.actor_alias
            && event.sequence > tool.sequence
    });
    verdict_from_bool(
        SCENARIO_MINI_STANDARD_TOOL_REENTRY_ORACLE,
        reentered && tool_runtime_output_observed(events),
        "tool result was captured and a later provider turn re-entered the model loop for the same session",
        "tool result did not have a later provider re-entry",
    )
}

pub(super) fn mini_rlm_finish_required_prose_repair(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> OracleVerdict {
    verdict_from_bool(
        SCENARIO_MINI_RLM_FINISH_REPAIR_ORACLE,
        provider_turn_exchange_counts_are_indexed(summary)
            && observer_reconnect_has_matching_turn(events, summary)
            && events
                .iter()
                .filter(|event| event.kind == BoundaryKind::Provider)
                .any(|event| event.payload.get("runtime_completion").is_some()),
        "finish-required repair mini-replay observed repeated provider completions and observer convergence",
        "finish-required repair mini-replay lacked repeated provider/observer convergence",
    )
}

pub(super) fn mini_rlm_schema_mismatch_repair(events: &[DeliveredBoundary]) -> OracleVerdict {
    let saw_schema_mutation = events.iter().any(|event| {
        event.kind == BoundaryKind::ProviderMutation
            && event
                .observed
                .pointer("/provider_parser_matrix/matrix/real_provider_parser_execution")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    });
    verdict_from_bool(
        SCENARIO_MINI_RLM_SCHEMA_REPAIR_ORACLE,
        saw_schema_mutation && provider_mutation_classes_observed(events),
        "schema mismatch repair mini-replay used mutated provider scripts through real provider parsers",
        "schema mismatch repair mini-replay lacked provider parser mutation evidence",
    )
}

pub(super) fn mini_rlm_lashlang_cell_exec_continues(events: &[DeliveredBoundary]) -> OracleVerdict {
    let Some(exec) = first_event(events, BoundaryKind::ExecCode) else {
        return OracleVerdict::failed(
            SCENARIO_MINI_RLM_CELL_EXEC_ORACLE,
            "no exec-code boundary was generated",
        );
    };
    let continued = events.iter().any(|event| {
        event.kind == BoundaryKind::Provider
            && event.actor_alias == exec.actor_alias
            && event.sequence > exec.sequence
    });
    verdict_from_bool(
        SCENARIO_MINI_RLM_CELL_EXEC_ORACLE,
        continued && exec_runtime_outcome_observed(events),
        "lashlang cell exec mini-replay produced an exec outcome and continued to a later provider turn",
        "lashlang cell exec mini-replay did not continue after exec",
    )
}

pub(super) fn mini_agent_durable_input_resolution(events: &[DeliveredBoundary]) -> OracleVerdict {
    let durable = events.iter().any(|event| {
        event.kind == BoundaryKind::DurableEffect
            && event
                .observed
                .get("replayed")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    });
    verdict_from_bool(
        SCENARIO_MINI_AGENT_DURABLE_INPUT_ORACLE,
        durable && process_wake_runtime_dto_observed(events) && observer_reconnect_has_any(events),
        "durable input mini-replay observed durable replay, process wake, and observer reconnect resolution",
        "durable input mini-replay lacked durable replay/process wake/observer reconnect evidence",
    )
}

pub(super) fn mini_agent_child_failure_graph(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> OracleVerdict {
    verdict_from_bool(
        SCENARIO_MINI_AGENT_CHILD_FAILURE_ORACLE,
        summary.session_count >= 2
            && worker_runtime_lease_dto_observed(events)
            && events
                .iter()
                .any(|event| event.kind == BoundaryKind::BackendFailure),
        "child failure mini-replay kept multi-session graph evidence while worker/backend failure boundaries executed",
        "child failure mini-replay lacked multi-session worker/backend failure evidence",
    )
}

pub(super) fn mini_agent_parallel_spawn_join(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> OracleVerdict {
    let wake_sessions = events
        .iter()
        .filter(|event| event.kind == BoundaryKind::ProcessWake)
        .filter_map(|event| event.observed.get("session").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    let ordered_sequences = events
        .iter()
        .filter(|event| matches!(event.kind, BoundaryKind::ProcessWake | BoundaryKind::Worker))
        .map(|event| event.sequence)
        .collect::<Vec<_>>();
    let deterministic_order = ordered_sequences.windows(2).all(|pair| pair[0] < pair[1]);
    verdict_from_bool(
        SCENARIO_MINI_AGENT_PARALLEL_JOIN_ORACLE,
        summary.session_count >= 2 && !wake_sessions.is_empty() && deterministic_order,
        "parallel spawn/join mini-replay recorded deterministic process/worker sequence ordering",
        "parallel spawn/join mini-replay did not record deterministic process/worker ordering",
    )
}

pub(super) fn verdict_from_bool(
    oracle_id: &'static str,
    condition: bool,
    passed: &'static str,
    failed: &'static str,
) -> OracleVerdict {
    if condition {
        OracleVerdict::passed(oracle_id, passed)
    } else {
        OracleVerdict::failed(oracle_id, failed)
    }
}

pub(super) fn first_event(
    events: &[DeliveredBoundary],
    kind: BoundaryKind,
) -> Option<&DeliveredBoundary> {
    events.iter().find(|event| event.kind == kind)
}

pub(super) fn provider_completion_units(event: &DeliveredBoundary) -> Vec<String> {
    event
        .payload
        .pointer("/runtime_completion/completion_units")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|unit| unit.get("unit").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

pub(super) fn next_provider_sequence(
    events: &[DeliveredBoundary],
    actor_alias: &str,
) -> Option<usize> {
    events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider && event.actor_alias == actor_alias)
        .map(|event| event.sequence)
        .min()
}

pub(super) fn observer_reconnect_has_any(events: &[DeliveredBoundary]) -> bool {
    events.iter().any(|event| {
        event.kind == BoundaryKind::Observer
            && event
                .observed
                .get("reconnected")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    })
}

pub(super) fn scenario_contract_oracle(
    contract: &ScenarioContractSpec,
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> OracleVerdict {
    let missing = contract
        .required_sim_evidence
        .iter()
        .copied()
        .filter(|evidence| !scenario_evidence_satisfied(evidence, events, summary))
        .collect::<Vec<_>>();
    let oracle_id = scenario_contract_oracle_id(contract);
    let semantic = scenario_contract_semantics(contract, events, summary);
    if missing.is_empty() && semantic.passed {
        OracleVerdict::passed(
            oracle_id,
            format!(
                "{} contract `{}` passed semantic `{}`: {}. Evidence: {}; adapter: {}",
                contract.suite,
                contract.test_name,
                contract.semantic_oracle,
                contract.owned_invariant,
                contract.required_sim_evidence.join(", "),
                semantic.reason
            ),
        )
    } else {
        let mut failures = Vec::new();
        if !missing.is_empty() {
            failures.push(format!("missing evidence [{}]", missing.join(", ")));
        }
        if !semantic.passed {
            failures.push(format!("semantic adapter failed: {}", semantic.reason));
        }
        OracleVerdict::failed(
            oracle_id,
            format!(
                "{} contract `{}` failed {} for invariant: {}",
                contract.suite,
                contract.test_name,
                failures.join("; "),
                contract.owned_invariant
            ),
        )
    }
}

pub(super) fn scenario_contract_oracle_id(contract: &ScenarioContractSpec) -> String {
    format!("{}:{}", contract.oracle_id, contract.test_name)
}

pub(super) fn scenario_evidence_satisfied(
    evidence: &str,
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> bool {
    match evidence {
        "queued_ingress" => summary
            .sessions
            .iter()
            .any(|session| session.queued_ingress_count > 0),
        "cancellation" => summary
            .sessions
            .iter()
            .any(|session| session.cancellation_count > 0),
        "process_wake" => {
            summary
                .sessions
                .iter()
                .any(|session| session.process_wake_count > 0)
                && process_wake_runtime_dto_observed(events)
        }
        "worker_stale_completion" => {
            summary.workers.iter().any(|worker| {
                worker.lease_owner_changes > 0
                    && worker.stale_completion_rejections > 0
                    && worker.active_fencing_token > 1
            }) && worker_runtime_lease_dto_observed(events)
        }
        "lease_time" => summary.sessions.iter().any(|session| {
            !session.lease_time_ticks.is_empty()
                && session
                    .lease_time_ticks
                    .windows(2)
                    .all(|ticks| ticks[0] <= ticks[1])
        }),
        "provider_turn" => {
            let provider_turns = summary
                .sessions
                .iter()
                .map(|session| session.provider_turns.len())
                .sum::<usize>();
            summary.session_count > 0 && provider_turns >= summary.session_count * 2
        }
        "provider_event" => events.iter().any(|event| {
            event.kind == BoundaryKind::ProviderEvent
                && event
                    .observed
                    .get("provider_event_release")
                    .and_then(Value::as_bool)
                    == Some(true)
                && event
                    .observed
                    .get("turn_boundary_id")
                    .and_then(Value::as_str)
                    .is_some()
        }),
        "provider_mutation" => events.iter().any(|event| {
            event.kind == BoundaryKind::ProviderMutation
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
        }),
        "tool_result" => {
            summary
                .sessions
                .iter()
                .any(|session| !session.tool_outputs.is_empty())
                && tool_runtime_output_observed(events)
        }
        "max_turn_stop" => events.iter().any(|event| {
            event.kind == BoundaryKind::Trigger
                && event
                    .observed
                    .pointer("/contract_execution/contract")
                    .and_then(Value::as_str)
                    == Some("standard.max_turns_after_tool_result")
                && event
                    .observed
                    .pointer("/contract_execution/source/kind")
                    .and_then(Value::as_str)
                    == Some("fixed_dst_api_execution")
                && event
                    .observed
                    .pointer("/contract_execution/result/turn_outcomes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .any(|outcome| {
                        outcome.get("kind").and_then(Value::as_str) == Some("stopped")
                            && outcome.get("stop_reason").and_then(Value::as_str)
                                == Some("max_turns")
                    })
        }),
        "final_value" => events.iter().any(|event| {
            event.kind == BoundaryKind::Trigger
                && event
                    .observed
                    .pointer("/contract_execution/source/kind")
                    .and_then(Value::as_str)
                    == Some("fixed_dst_api_execution")
                && event
                    .observed
                    .pointer("/contract_execution/result/runtime_final_value_facts/outcome_kind")
                    .and_then(Value::as_str)
                    == Some("final_value")
                && event
                    .observed
                    .pointer(
                        "/contract_execution/result/runtime_final_value_facts/semantic_channel_observed",
                    )
                    .and_then(Value::as_bool)
                    == Some(true)
        }),
        "observer_convergence" => {
            summary.session_count > 0
                && summary.sessions.iter().all(|session| {
                    session.observer_turn_indices.last().copied()
                        == Some(session.provider_turns.len())
                })
        }
        "runtime_session_graph" => runtime_session_graph_law(summary, None).is_passed(),
        "exec_code" => {
            summary
                .sessions
                .iter()
                .any(|session| !session.exec_code_outputs.is_empty())
                && exec_runtime_outcome_observed(events)
        }
        "trigger" => summary
            .sessions
            .iter()
            .any(|session| session.trigger_count > 0),
        "backend_failure" => events.iter().any(|event| {
            event.kind == BoundaryKind::BackendFailure
                && event
                    .observed
                    .get("backend_failure")
                    .and_then(Value::as_bool)
                    == Some(true)
                && event
                    .payload
                    .pointer("/runtime_completion/completion_family")
                    .and_then(Value::as_str)
                    == Some("backend_retry_or_failure")
                && event
                    .observed
                    .pointer("/production_store_error/type")
                    .and_then(Value::as_str)
                    .is_some()
        }),
        "durable_effect" => {
            summary
                .durable_effects
                .iter()
                .any(|effect| effect.execution_count == 1 && effect.replay_count > 0)
                && durable_runtime_effect_observed(events)
        }
        "multi_session" => summary.session_count >= 2,
        "observer_reconnect" => summary
            .sessions
            .iter()
            .any(|session| session.observer_reconnects > 0),
        _ => false,
    }
}

pub(super) struct ScenarioSemanticVerdict {
    pub(super) passed: bool,
    pub(super) reason: String,
}

impl ScenarioSemanticVerdict {
    pub(super) fn passed(reason: impl Into<String>) -> Self {
        Self {
            passed: true,
            reason: reason.into(),
        }
    }

    pub(super) fn failed(reason: impl Into<String>) -> Self {
        Self {
            passed: false,
            reason: reason.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScenarioContractGeneratedFact {
    pub fact: &'static str,
    pub assertion: &'static str,
    pub boundary_ids: Vec<String>,
    pub observed: Value,
}

pub fn scenario_contract_generated_facts(
    contract: &ScenarioContractSpec,
    events: &[DeliveredBoundary],
) -> Result<Vec<ScenarioContractGeneratedFact>, String> {
    scenario_contract_generated_facts_for_semantic(contract.semantic_oracle, events)
}

pub fn scenario_contract_generated_facts_for_semantic(
    semantic_oracle: &str,
    events: &[DeliveredBoundary],
) -> Result<Vec<ScenarioContractGeneratedFact>, String> {
    let facts = match semantic_oracle {
        "standard.initial_request_projection" => Ok(vec![
            standard_protocol_execution_fact(events, "standard.initial_request_projection")?,
            initial_provider_projection_fact(events)?,
        ]),
        "standard.empty_provider_response_error" => Ok(vec![
            standard_protocol_execution_fact(events, "standard.empty_provider_response_error")?,
            provider_mutation_semantic_fact(
                events,
                "dropped_terminal_event",
                "standard_empty_response_terminal_error",
                "empty/unterminated provider output is classified as a terminal provider error by every migrated parser",
            )?,
        ]),
        "standard.provider_error_without_checkpoint" => Ok(vec![
            standard_protocol_execution_fact(events, "standard.provider_error_without_checkpoint")?,
            provider_mutation_semantic_fact(
                events,
                "rate_limit_error_envelope",
                "standard_provider_error_no_checkpoint",
                "provider error envelope is classified through migrated parsers without depending on a later checkpoint",
            )?,
        ]),
        "standard.native_tool_loop_reenters_model" => Ok(vec![
            standard_protocol_execution_fact(events, "standard.native_tool_loop_reenters_model")?,
            tool_reentry_fact(events, "standard_native_tool_reenters_model", false)?,
        ]),
        "standard.parallel_tool_results_checkpoint_once" => Ok(vec![
            standard_protocol_execution_fact(
                events,
                "standard.parallel_tool_results_checkpoint_once",
            )?,
            parallel_tool_results_checkpoint_once_fact(events)?,
        ]),
        "standard.tool_failure_feedback_reenters_model" => Ok(vec![
            standard_protocol_execution_fact(
                events,
                "standard.tool_failure_feedback_reenters_model",
            )?,
            tool_reentry_fact(events, "standard_tool_feedback_reenters_model", false)?,
            provider_mutation_semantic_fact(
                events,
                "malformed_sse_chunk",
                "standard_tool_failure_feedback_parser_path",
                "tool-failure feedback package also carries generated provider failure parser evidence",
            )?,
        ]),
        "standard.streamed_text_finalizes_once" => Ok(vec![
            standard_protocol_execution_fact(events, "standard.streamed_text_finalizes_once")?,
            streamed_text_finalizes_once_fact(events)?,
        ]),
        "standard.max_turns_after_tool_result" => {
            Ok(vec![standard_max_turns_after_tool_result_fact(events)?])
        }
        "rlm.natural_prose_finalizes" => Ok(vec![rlm_protocol_execution_fact(
            events,
            "rlm.natural_prose_finalizes",
        )?]),
        "rlm.typed_prose_requires_finish" => Ok(vec![rlm_protocol_execution_fact(
            events,
            "rlm.typed_prose_requires_finish",
        )?]),
        "rlm.finish_required_max_turn_stop" => Ok(vec![rlm_protocol_execution_fact(
            events,
            "rlm.finish_required_max_turn_stop",
        )?]),
        "rlm.exec_error_max_turn_stop" => Ok(vec![
            rlm_protocol_execution_fact(events, "rlm.exec_error_max_turn_stop")?,
            exec_semantic_fact(
                events,
                "rlm_exec_error_max_turn_stop",
                ExecFactRequirement::RuntimeOutcome,
            )?,
        ]),
        "rlm.finish_required_diagnostic_counts" => Ok(vec![rlm_protocol_execution_fact(
            events,
            "rlm.finish_required_diagnostic_counts",
        )?]),
        "rlm.natural_diagnostic_counts" => Ok(vec![rlm_protocol_execution_fact(
            events,
            "rlm.natural_diagnostic_counts",
        )?]),
        "rlm.cell_diagnostic_counts" => Ok(vec![
            rlm_protocol_execution_fact(events, "rlm.cell_diagnostic_counts")?,
            exec_semantic_fact(
                events,
                "rlm_cell_diagnostic_exec_counts",
                ExecFactRequirement::RuntimeOutcome,
            )?,
        ]),
        "rlm.retired_marker_plain_lashlang_text" => Ok(vec![
            rlm_protocol_execution_fact(events, "rlm.retired_marker_plain_lashlang_text")?,
            exec_semantic_fact(
                events,
                "rlm_retired_marker_plain_lashlang_text",
                ExecFactRequirement::NoToolCallReplay,
            )?,
        ]),
        "rlm.lashlang_cell_exec_continues" => Ok(vec![
            rlm_protocol_execution_fact(events, "rlm.lashlang_cell_exec_continues")?,
            exec_semantic_fact(
                events,
                "rlm_lashlang_cell_exec_continues",
                ExecFactRequirement::ReentersProvider,
            )?,
        ]),
        "rlm.streamed_lashlang_cell_exec_persists_trajectory" => Ok(vec![
            rlm_protocol_execution_fact(
                events,
                "rlm.streamed_lashlang_cell_exec_persists_trajectory",
            )?,
            exec_semantic_fact(
                events,
                "rlm_streamed_lashlang_cell_exec_persists_trajectory",
                ExecFactRequirement::ReentersProvider,
            )?,
        ]),
        "rlm.empty_options_natural_default" => Ok(vec![rlm_protocol_execution_fact(
            events,
            "rlm.empty_options_natural_default",
        )?]),
        "rlm.exec_result_no_tool_call_replay" => Ok(vec![
            rlm_protocol_execution_fact(events, "rlm.exec_result_no_tool_call_replay")?,
            exec_semantic_fact(
                events,
                "rlm_exec_result_no_tool_call_replay",
                ExecFactRequirement::NoToolCallReplay,
            )?,
        ]),
        "rlm.exec_tool_control_frame_switch_terminal" => Ok(vec![
            rlm_protocol_execution_fact(events, "rlm.exec_tool_control_frame_switch_terminal")?,
            exec_semantic_fact(
                events,
                "rlm_exec_tool_control_frame_switch_terminal",
                ExecFactRequirement::RuntimeOutcome,
            )?,
            trigger_then_provider_fact(events, "rlm_exec_tool_control_frame_switch_trigger")?,
        ]),
        "rlm.exec_tool_control_fail_terminal" => Ok(vec![
            rlm_protocol_execution_fact(events, "rlm.exec_tool_control_fail_terminal")?,
            exec_semantic_fact(
                events,
                "rlm_exec_tool_control_fail_terminal",
                ExecFactRequirement::RuntimeOutcome,
            )?,
            backend_retry_terminalization_fact(events, "rlm_exec_tool_control_fail_backend")?,
        ]),
        "rlm.typed_finish_emits_outcome_and_done" => Ok(vec![rlm_protocol_execution_fact(
            events,
            "rlm.typed_finish_emits_outcome_and_done",
        )?]),
        "rlm.natural_allows_finish_value" => Ok(vec![rlm_protocol_execution_fact(
            events,
            "rlm.natural_allows_finish_value",
        )?]),
        "rlm.typed_schema_mismatch_repair_loop" => Ok(vec![
            provider_mutation_semantic_fact(
                events,
                "malformed_sse_chunk",
                "rlm_typed_schema_mismatch_feedback",
                "typed schema mismatch repair uses generated malformed provider payload feedback",
            )?,
            rlm_protocol_execution_fact(events, "rlm.typed_schema_mismatch_repair_loop")?,
        ]),
        "rlm.typed_schema_any_of_mismatch" => Ok(vec![
            provider_mutation_semantic_fact(
                events,
                "rate_limit_error_envelope",
                "rlm_typed_schema_anyof_feedback",
                "typed anyOf mismatch package carries generated parser-classified feedback",
            )?,
            rlm_protocol_execution_fact(events, "rlm.typed_schema_any_of_mismatch")?,
        ]),
        "agent.foreground_tool_call_round_trip" => Ok(vec![
            agent_contract_execution_fact(events, "agent.foreground_tool_call_round_trip")?,
            tool_reentry_fact(events, "agent_foreground_tool_call_round_trip", false)?,
        ]),
        "agent.started_process_tool_call_graph" => Ok(vec![
            agent_contract_execution_fact(events, "agent.started_process_tool_call_graph")?,
            process_wake_fact(events, "agent_started_process_graph")?,
            tool_reentry_fact(events, "agent_started_process_tool_call", false)?,
        ]),
        "agent.durable_input_suspension_resolution" => Ok(vec![
            agent_contract_execution_fact(events, "agent.durable_input_suspension_resolution")?,
            durable_replay_fact(events, "agent_durable_input_first_and_replay")?,
            process_wake_fact(events, "agent_durable_input_process_wake")?,
            observer_reconnect_fact(events, "agent_durable_input_observer_reconnect")?,
        ]),
        "agent.shell_results_are_data" => Ok(vec![
            agent_contract_execution_fact(events, "agent.shell_results_are_data")?,
            exec_semantic_fact(
                events,
                "agent_shell_exec_result_data",
                ExecFactRequirement::RuntimeOutcome,
            )?,
            tool_reentry_fact(events, "agent_shell_tool_result_data", false)?,
        ]),
        "agent.shell_output_print_projection_survives" => Ok(vec![
            agent_contract_execution_fact(events, "agent.shell_output_print_projection_survives")?,
            exec_semantic_fact(
                events,
                "agent_shell_output_exec_projection",
                ExecFactRequirement::RuntimeOutcome,
            )?,
            agent_shell_output_projection_fact(events)?,
        ]),
        "agent.started_process_subagent_spawn" => Ok(vec![
            agent_contract_execution_fact(events, "agent.started_process_subagent_spawn")?,
            process_wake_fact(events, "agent_started_process_subagent_spawn")?,
        ]),
        "agent.nested_process_start_await" => Ok(vec![
            agent_contract_execution_fact(events, "agent.nested_process_start_await")?,
            process_wake_fact(events, "agent_nested_process_start_await")?,
        ]),
        "agent.session_turn_process_child" => Ok(vec![
            agent_contract_execution_fact(events, "agent.session_turn_process_child")?,
            process_wake_fact(events, "agent_session_turn_process_child_wake")?,
            agent_session_turn_child_provider_fact(events)?,
        ]),
        "agent.failed_child_preserves_failure_graph" => Ok(vec![
            agent_contract_execution_fact(events, "agent.failed_child_preserves_failure_graph")?,
            worker_stale_fact(events, "agent_failed_child_worker_graph")?,
            backend_retry_terminalization_fact(events, "agent_failed_child_backend_graph")?,
        ]),
        "agent.parallel_spawn_and_join" => Ok(vec![
            agent_contract_execution_fact(events, "agent.parallel_spawn_and_join")?,
            process_wake_fact(events, "agent_parallel_spawn_process_wakes")?,
            worker_stale_fact(events, "agent_parallel_spawn_join_worker_order")?,
        ]),
        "agent.tuple_values_finish_as_json_arrays" => Ok(vec![agent_contract_execution_fact(
            events,
            "agent.tuple_values_finish_as_json_arrays",
        )?]),
        other => Err(format!(
            "{} scenario contract `{other}` has no per-contract semantic adapter; add distinct evidence instead of a generic fallback",
            other.split('.').next().unwrap_or("unknown")
        )),
    }?;
    reject_named_contract_proxy_facts(semantic_oracle, &facts)?;
    Ok(facts)
}

/// Per-contract semantic adapter. Each suite gets explicit generated-boundary
/// semantics keyed by `semantic_oracle`; unmapped contracts fail loudly instead
/// of inheriting a suite-level fallback.
pub(super) fn scenario_contract_semantics(
    contract: &ScenarioContractSpec,
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> ScenarioSemanticVerdict {
    match contract.suite {
        "runtime" => runtime_contract_semantics(contract.semantic_oracle, events, summary),
        "standard" => standard_contract_semantics(contract.semantic_oracle, events, summary),
        "rlm" => rlm_contract_semantics(contract.semantic_oracle, events, summary),
        "agent" => agent_contract_semantics(contract.semantic_oracle, events, summary),
        other => ScenarioSemanticVerdict::failed(format!(
            "suite `{other}` has no per-contract semantic adapter dispatcher"
        )),
    }
}

pub(super) fn runtime_contract_semantics(
    semantic_oracle: &str,
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> ScenarioSemanticVerdict {
    // Each runtime contract owns a DISTINCT semantic adapter keyed on the
    // specific runtime boundary it governs — no `|`-grouped shared arms and no
    // single global condition standing in for per-contract evidence.
    match semantic_oracle {
        // Advisory lease state does not authorize a stale durable completion.
        "runtime.advisory_lease_head_cas" => assert_semantic(
            worker_runtime_lease_dto_observed(events)
                && summary
                    .workers
                    .iter()
                    .any(|worker| worker.stale_completion_rejections > 0),
            "the stale worker completion was rejected by durable commit fencing",
        ),
        // A new incarnation waits for TTL expiry and the fence strictly advances.
        "runtime.stale_lease_ttl" => assert_semantic(
            worker_runtime_lease_dto_observed(events)
                && summary.workers.iter().any(|worker| {
                    worker.lease_owner_changes > 0 && worker.active_fencing_token > 1
                }),
            "a second incarnation acquired the stale lease after TTL at a strictly higher fence",
        ),
        "runtime.checkpoint_redrive_cancel" => assert_semantic(
            queued_inputs_have_cancel_targets(events)
                && observer_convergence_law(summary, None).is_passed(),
            "queued input cancellation targeted a source key and observers converged",
        ),
        // The queued (next-turn) input stays pending/hidden while the live turn runs.
        "runtime.queued_work_keeps_pending_input" => assert_semantic(
            queued_ingress_has_source_keys(events) && provider_turns_after_queue(summary),
            "queued ingress kept a stable source key while live provider turns continued",
        ),
        // A queued turn input is eventually completed into a subsequent turn.
        "runtime.queued_turn_input_completion" => assert_semantic(
            queued_ingress_has_source_keys(events)
                && summary.sessions.iter().any(|session| {
                    session.queued_ingress_count > 0 && session.provider_turns.len() >= 2
                }),
            "a queued turn input source key was followed by a completed subsequent turn",
        ),
        // A command-only queue drains against monotonic lease fencing tokens.
        "runtime.command_only_queue_drain" => assert_semantic(
            queued_ingress_has_source_keys(events)
                && lease_time_monotonic_law(events, None).is_passed(),
            "command queue source keys drained against monotonically advancing lease fences",
        ),
        // A command applied before turn work still lets later provider turns run.
        "runtime.command_before_turn_work" => assert_semantic(
            queued_ingress_has_source_keys(events)
                && provider_turns_after_queue(summary)
                && lease_time_monotonic_law(events, None).is_passed(),
            "a command queued before turn work preserved later turns and lease ordering",
        ),
        "runtime.observation_replay_preserves_input" => assert_semantic(
            observer_reconnect_has_matching_turn(events, summary),
            "observer reconnect replay converged to the final provider turn",
        ),
        other => unmapped_scenario_semantic("runtime", other),
    }
}

pub(super) fn standard_contract_semantics(
    semantic_oracle: &str,
    events: &[DeliveredBoundary],
    _summary: &AbstractWorldSummary,
) -> ScenarioSemanticVerdict {
    match scenario_contract_generated_facts_for_semantic(semantic_oracle, events) {
        Ok(facts) => ScenarioSemanticVerdict::passed(format!(
            "generated contract facts held: {}",
            fact_names(&facts)
        )),
        Err(reason) => ScenarioSemanticVerdict::failed(reason),
    }
}

pub(super) fn rlm_contract_semantics(
    semantic_oracle: &str,
    events: &[DeliveredBoundary],
    _summary: &AbstractWorldSummary,
) -> ScenarioSemanticVerdict {
    match scenario_contract_generated_facts_for_semantic(semantic_oracle, events) {
        Ok(facts) => ScenarioSemanticVerdict::passed(format!(
            "generated contract facts held: {}",
            fact_names(&facts)
        )),
        Err(reason) => ScenarioSemanticVerdict::failed(reason),
    }
}

pub(super) fn agent_contract_semantics(
    semantic_oracle: &str,
    events: &[DeliveredBoundary],
    _summary: &AbstractWorldSummary,
) -> ScenarioSemanticVerdict {
    match scenario_contract_generated_facts_for_semantic(semantic_oracle, events) {
        Ok(facts) => ScenarioSemanticVerdict::passed(format!(
            "generated contract facts held: {}",
            fact_names(&facts)
        )),
        Err(reason) => ScenarioSemanticVerdict::failed(reason),
    }
}

pub(super) fn fact_names(facts: &[ScenarioContractGeneratedFact]) -> String {
    facts
        .iter()
        .map(|fact| fact.fact)
        .collect::<Vec<_>>()
        .join(", ")
}
