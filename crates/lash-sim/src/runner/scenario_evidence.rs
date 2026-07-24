use super::*;

pub(super) fn trace_has_queued_cancel_race(lines: &[&TraceEventLine]) -> bool {
    lines
        .iter()
        .filter(|line| line.event.kind == BoundaryKind::QueuedIngress)
        .any(|queued| {
            let Some(source_key) = queued
                .event
                .payload
                .get("source_key")
                .and_then(Value::as_str)
            else {
                return false;
            };
            queued
                .event
                .payload
                .get("ingress_mode")
                .and_then(Value::as_str)
                == Some("active_turn")
                && queued
                    .event
                    .observed
                    .get("source_key")
                    .and_then(Value::as_str)
                    == Some(source_key)
                && queued
                    .event
                    .observed
                    .get("input_state")
                    .and_then(Value::as_str)
                    .is_some_and(|state| state.starts_with("pending"))
                && lines.iter().any(|line| {
                    line.event.kind == BoundaryKind::Cancellation
                        && line.event.sequence > queued.event.sequence
                        && line.event.observed.get("target").and_then(Value::as_str)
                            == Some(queued.event.boundary_id.as_str())
                        && line
                            .event
                            .observed
                            .get("cancelled")
                            .and_then(Value::as_bool)
                            == Some(true)
                })
        })
}

pub(super) fn trace_has_trigger_wakeup_route(lines: &[&TraceEventLine]) -> bool {
    lines
        .iter()
        .filter(|line| line.event.kind == BoundaryKind::Trigger)
        .any(|line| {
            line.event
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
}

pub(super) fn trace_has_duplicate_process_wake_idempotency(lines: &[&TraceEventLine]) -> bool {
    let mut by_dedupe_key: BTreeMap<String, Vec<&TraceEventLine>> = BTreeMap::new();
    for line in lines
        .iter()
        .filter(|line| line.event.kind == BoundaryKind::ProcessWake)
    {
        let Some(dedupe_key) = line
            .event
            .observed
            .get("dedupe_key")
            .and_then(Value::as_str)
        else {
            continue;
        };
        by_dedupe_key
            .entry(dedupe_key.to_string())
            .or_default()
            .push(*line);
    }
    by_dedupe_key.values().any(|events| {
        let strict_claim_dedupe = events.len() >= 2
            && events.iter().any(|line| {
                line.event
                    .observed
                    .get("claimed_once")
                    .and_then(Value::as_bool)
                    == Some(true)
                    && line
                        .event
                        .observed
                        .pointer("/runtime_queued_work/claimed")
                        .and_then(Value::as_bool)
                        == Some(true)
            })
            && events.iter().all(|line| {
                line.event.observed.get("runtime_process_wake").is_some()
                    && line.event.observed.get("runtime_queued_work").is_some()
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
                    && line.event.observed.get("runtime_process_wake").is_some()
                    && line.event.observed.get("runtime_queued_work").is_some()
            });
        strict_claim_dedupe || in_flight_rejection
    })
}

pub(super) fn trace_has_worker_stale_completion(lines: &[&TraceEventLine]) -> bool {
    lines
        .iter()
        .filter(|line| line.event.kind == BoundaryKind::Worker)
        .any(|line| {
            line.event
                .observed
                .get("stale_completion_rejected")
                .and_then(Value::as_bool)
                == Some(true)
                && line
                    .event
                    .observed
                    .get("process_stale_completion_rejected")
                    .and_then(Value::as_bool)
                    == Some(true)
                && line
                    .event
                    .observed
                    .get("process_stale_output_absent")
                    .and_then(Value::as_bool)
                    == Some(true)
                && line
                    .event
                    .observed
                    .get("process_terminal_writer")
                    .and_then(Value::as_str)
                    == Some("successor")
                && line
                    .event
                    .observed
                    .get("process_terminal_event_count")
                    .and_then(Value::as_u64)
                    == Some(1)
                && line.event.observed.get("runtime_active_lease").is_some()
                && line
                    .event
                    .observed
                    .get("runtime_stale_completion")
                    .is_some()
        })
}

pub(super) fn trace_has_durable_effect_replay(lines: &[&TraceEventLine]) -> bool {
    let mut by_key: BTreeMap<String, Vec<&TraceEventLine>> = BTreeMap::new();
    for line in lines
        .iter()
        .filter(|line| line.event.kind == BoundaryKind::DurableEffect)
    {
        let Some(key) = line
            .event
            .observed
            .get("durable_key")
            .and_then(Value::as_str)
        else {
            continue;
        };
        by_key.entry(key.to_string()).or_default().push(*line);
    }
    by_key.values().any(|events| {
        let first = events.iter().any(|line| {
            line.event.observed.get("replayed").and_then(Value::as_bool) == Some(false)
                && line
                    .event
                    .observed
                    .pointer("/runtime_effect/local_executor_called")
                    .and_then(Value::as_bool)
                    == Some(true)
        });
        let replay = events.iter().any(|line| {
            line.event.observed.get("replayed").and_then(Value::as_bool) == Some(true)
                && line
                    .event
                    .observed
                    .pointer("/runtime_effect/local_executor_called")
                    .and_then(Value::as_bool)
                    == Some(false)
                && line
                    .event
                    .observed
                    .get("execution_count")
                    .and_then(Value::as_u64)
                    == Some(1)
        });
        first && replay
    })
}

pub(super) fn trace_has_backend_retry_terminalization(lines: &[&TraceEventLine]) -> bool {
    let mut by_operation: BTreeMap<String, Vec<&TraceEventLine>> = BTreeMap::new();
    for line in lines
        .iter()
        .filter(|line| line.event.kind == BoundaryKind::BackendFailure)
    {
        let operation = line
            .event
            .observed
            .get("operation")
            .or_else(|| line.event.payload.get("operation"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        by_operation.entry(operation).or_default().push(*line);
    }
    by_operation.values().any(|events| {
        let retryable = events.iter().any(|line| {
            line.event
                .observed
                .get("retryable")
                .and_then(Value::as_bool)
                == Some(true)
                && line
                    .event
                    .observed
                    .get("attempt")
                    .and_then(Value::as_u64)
                    .is_some_and(|attempt| attempt > 0)
        });
        let terminal = events.iter().any(|line| {
            line.event
                .observed
                .get("retryable")
                .and_then(Value::as_bool)
                == Some(false)
                && line
                    .event
                    .observed
                    .get("store_error_class")
                    .and_then(Value::as_str)
                    == Some("terminal_backend_error")
        });
        retryable && terminal
    })
}

pub(super) fn trace_has_protocol_terminal_boundary_mix(lines: &[&TraceEventLine]) -> bool {
    let has_provider_terminal = lines.iter().any(|line| {
        line.event.kind == BoundaryKind::Provider
            && line
                .event
                .observed
                .pointer("/runtime_contract/status")
                .and_then(Value::as_str)
                == Some("passed")
            && line.event.observed.get("success").and_then(Value::as_bool) == Some(true)
    });
    let has_provider_failure_terminal = trace_has_provider_protocol_terminalization(lines);
    let has_exec_terminal = lines.iter().any(|line| {
        line.event.kind == BoundaryKind::ExecCode
            && line.event.observed.get("runtime_effect_outcome").is_some()
            && line
                .event
                .observed
                .pointer("/runtime_effect/kind")
                .and_then(Value::as_str)
                == Some("exec_code")
    });
    has_provider_terminal && has_provider_failure_terminal && has_exec_terminal
}

pub(super) fn trace_has_provider_protocol_terminalization(lines: &[&TraceEventLine]) -> bool {
    let mutations = lines
        .iter()
        .filter(|line| line.event.kind == BoundaryKind::ProviderMutation)
        .filter_map(|line| {
            line.event
                .observed
                .get("mutation")
                .or_else(|| line.event.payload.get("mutation"))
                .and_then(Value::as_str)
        })
        .collect::<BTreeSet<_>>();
    mutations.contains("rate_limit_error_envelope")
        && mutations.contains("dropped_terminal_event")
        && lines.iter().any(|line| {
            line.event.kind == BoundaryKind::ProviderMutation
                && line
                    .event
                    .observed
                    .pointer("/provider_parser_matrix/matrix/real_provider_parser_execution")
                    .and_then(Value::as_bool)
                    == Some(true)
        })
}

pub(super) fn selected_events(
    contract: &ScenarioContractSpec,
    event_lines: &[TraceEventLine],
    selected_evidence: &[ScenarioEvidenceSelection],
) -> Result<Vec<TraceEventLine>, FixedScriptRunnerError> {
    let mut selected_keys = BTreeSet::new();
    let mut events = Vec::new();
    for evidence in selected_evidence {
        if selected_keys.insert((evidence.trace_alias.clone(), evidence.boundary_id.clone())) {
            let event_line = event_lines
                .iter()
                .find(|line| {
                    line.trace_alias == evidence.trace_alias
                        && line.event.boundary_id == evidence.boundary_id
                })
                .ok_or_else(|| {
                    FixedScriptRunnerError::Assertion(format!(
                        "scenario contract `{}` selected missing event `{}`",
                        contract.test_name, evidence.boundary_id
                    ))
                })?;
            events.push(event_line.clone());
        }
    }
    Ok(events)
}

pub(super) fn replay_artifact_lookup(
    replay_reports: &[GeneratedReplayArtifact],
) -> BTreeMap<String, &GeneratedReplayArtifact> {
    replay_reports
        .iter()
        .map(|replay| (format!("seed-{:016x}", replay.seed), replay))
        .collect()
}

pub(super) fn scenario_positive_evidence(
    contract: &ScenarioContractSpec,
    selected_evidence: &[ScenarioEvidenceSelection],
    verdicts: &[OracleVerdict],
    replay_lookup: &BTreeMap<String, &GeneratedReplayArtifact>,
) -> Result<ScenarioPositiveEvidence, FixedScriptRunnerError> {
    let mut source_trace_aliases = BTreeSet::new();
    let mut selected_boundary_ids = BTreeSet::new();
    for evidence in selected_evidence {
        source_trace_aliases.insert(evidence.trace_alias.clone());
        selected_boundary_ids.insert(evidence.boundary_id.clone());
    }
    let mut source_trace_paths = Vec::new();
    let mut replay_report_paths = Vec::new();
    let mut sqlite_replay_report_paths = Vec::new();
    for alias in &source_trace_aliases {
        let replay = replay_lookup.get(alias).ok_or_else(|| {
            FixedScriptRunnerError::Assertion(format!(
                "scenario contract `{}` selected trace `{alias}` without replay artifact",
                contract.test_name
            ))
        })?;
        source_trace_paths.push(replay.trace_path.clone());
        replay_report_paths.push(replay.replay_report_path.clone());
        sqlite_replay_report_paths.push(replay.sqlite_replay_report_path.clone());
    }
    let verdict = verdicts
        .iter()
        .find(|verdict| verdict.status == OracleStatus::Passed)
        .unwrap_or_else(|| {
            verdicts
                .first()
                .expect("scenario package requires at least one verdict")
        });
    Ok(ScenarioPositiveEvidence {
        schema: "lash.sim.scenario-contract-positive-evidence.v1",
        source_trace_aliases: source_trace_aliases.into_iter().collect(),
        source_trace_paths,
        replay_report_paths,
        sqlite_replay_report_paths,
        selected_boundary_ids: selected_boundary_ids.into_iter().collect(),
        selected_event_count: selected_evidence.len(),
        oracle_status: verdict.status.clone(),
        oracle_reason: verdict.message.clone(),
    })
}

pub(super) fn scenario_negative_evidence(
    fixture: &ScenarioNegativeFixture,
) -> ScenarioNegativeEvidence {
    ScenarioNegativeEvidence {
        schema: "lash.sim.scenario-contract-negative-evidence.v1",
        fixture_id: fixture.fixture_id,
        fixture_path: fixture.fixture_path,
        expected_oracle_id: fixture.expected_oracle_id,
        expected_reason_contains: fixture.expected_reason_contains,
        minimize_command: format!(
            "cargo run -p lash-sim --locked -- minimize {} --out <artifact-root>/{}",
            fixture.fixture_path, fixture.fixture_id
        ),
        minimized_package_path: format!(
            "sim/failing-fixtures/{}/minimized-regression/package.json",
            fixture.fixture_id
        ),
    }
}

pub(super) fn scenario_operational_cases(
    contract: &ScenarioContractSpec,
    selected_evidence: &[ScenarioEvidenceSelection],
) -> Vec<String> {
    let mut cases = BTreeSet::new();
    cases.insert(format!("scenario-suite:{}", contract.suite));
    cases.insert(format!("semantic:{}", contract.semantic_oracle));
    cases.insert("protocol-state-transition".to_string());
    for evidence in selected_evidence {
        for case in operational_cases_for_evidence(&evidence.evidence) {
            cases.insert(case.to_string());
        }
    }
    for case in operational_cases_for_semantic(contract.semantic_oracle) {
        cases.insert(case.to_string());
    }
    cases.into_iter().collect()
}

fn operational_cases_for_evidence(evidence: &str) -> &'static [&'static str] {
    match evidence {
        "queued_ingress" => &[
            "queueing-inputs",
            "active-turn-input-queueing",
            "duplicate-replayed-inputs",
        ],
        "cancellation" => &["cancellation"],
        "process_wake" => &["triggers-wakeups", "process-wake", "duplicate-delivery"],
        "worker_stale_completion" => &["worker-failover", "lease-fencing", "stale-completion"],
        "provider_turn" => &["provider-runtime-turn", "scripted-provider-transport"],
        "provider_event" => &["provider-runtime-turn", "scheduler-owned-provider-events"],
        "tool_result" => &["tool-boundary", "tool-loop"],
        "max_turn_stop" => &["tool-boundary", "max-turn-stop"],
        "final_value" => &["semantic-final-value", "final-value-event"],
        "exec_code" => &["exec-boundary", "rlm-lashlang-exec"],
        "trigger" => &["triggers-wakeups", "trigger-delivery"],
        "durable_effect" => &["durable-effect", "crash-reopen-effect-replay"],
        "provider_mutation" => &[
            "provider-failure",
            "provider-mutation",
            "real-provider-parser",
        ],
        "backend_failure" => &["backend-retry", "persistence-fault"],
        "observer_reconnect" => &["observer-reconnect"],
        "observer_convergence" => &["observer-convergence"],
        "lease_time" => &["lease-fencing", "scheduler-time"],
        "multi_session" => &["multi-session"],
        "runtime_session_graph" => &["runtime-session-graph"],
        _ => &[],
    }
}

fn operational_cases_for_semantic(semantic_oracle: &str) -> &'static [&'static str] {
    match semantic_oracle {
        "runtime.checkpoint_redrive_cancel" => &[
            "active-turn-input-queueing",
            "cancellation",
            "duplicate-replayed-inputs",
        ],
        "runtime.queued_work_keeps_pending_input" | "runtime.queued_turn_input_completion" => {
            &["queueing-inputs", "active-turn-input-queueing"]
        }
        "runtime.advisory_lease_head_cas" | "runtime.dead_lease_reclaim_rejects_stale" => {
            &["lease-fencing", "worker-failover", "stale-completion"]
        }
        "standard.empty_provider_response_error" | "standard.provider_error_without_checkpoint" => {
            &["provider-failure", "retry-exhaustion"]
        }
        "standard.streamed_text_finalizes_once" => &["duplicate-free-stream-finalization"],
        "standard.native_tool_loop_reenters_model"
        | "standard.tool_failure_feedback_reenters_model"
        | "standard.parallel_tool_results_checkpoint_once" => &["tool-loop"],
        "rlm.lashlang_cell_exec_continues" => &["rlm-lashlang-exec", "triggers-wakeups"],
        "rlm.streamed_lashlang_cell_exec_persists_trajectory" => {
            &["rlm-lashlang-exec", "scheduler-owned-provider-events"]
        }
        "rlm.typed_schema_mismatch_repair_loop" | "rlm.typed_schema_any_of_mismatch" => {
            &["provider-failure", "repair-loop"]
        }
        semantic if semantic.starts_with("rlm.") => &["rlm-protocol-transition"],
        "agent.durable_input_suspension_resolution" => {
            &["durable-effect", "process-wake", "observer-reconnect"]
        }
        semantic if semantic.starts_with("agent.") => &["agent-process-graph", "worker-failover"],
        _ => &[],
    }
}

pub(super) fn scenario_generated_shape(
    contract: &ScenarioContractSpec,
    selected_evidence: &[ScenarioEvidenceSelection],
    selected_events: &[TraceEventLine],
) -> Result<ScenarioGeneratedShape, FixedScriptRunnerError> {
    let mut evidence_names = BTreeSet::new();
    let required_evidence = contract
        .required_sim_evidence
        .iter()
        .copied()
        .chain(semantic_scenario_evidence(contract.semantic_oracle))
        .filter(|evidence| evidence_names.insert(*evidence))
        .map(|evidence| ScenarioRequiredEvidence {
            evidence: evidence.to_string(),
            boundary_kind: scenario_evidence_boundary_kind(evidence).to_string(),
            assertion: scenario_evidence_assertion(evidence),
            selected_event_count: selected_evidence
                .iter()
                .filter(|selection| selection.evidence == evidence)
                .count(),
        })
        .collect();
    Ok(ScenarioGeneratedShape {
        schema: "lash.sim.scenario-contract-generated-shape.v1",
        transition_kind: scenario_transition_kind(contract).to_string(),
        semantic_oracle: contract.semantic_oracle,
        required_evidence,
        transition_facts: scenario_transition_facts(contract, selected_events)?,
        generated_backend_regression: scenario_backend_regression_reference(contract),
        negative_fixture: scenario_negative_fixture_for_contract(contract, selected_evidence),
    })
}

fn scenario_transition_kind(contract: &ScenarioContractSpec) -> &'static str {
    match contract.semantic_oracle {
        "runtime.advisory_lease_head_cas" => "runtime.advisory-lease-head-cas-transition",
        "runtime.dead_lease_reclaim_rejects_stale" => {
            "runtime.dead-lease-reclaim-stale-completion-transition"
        }
        "runtime.checkpoint_redrive_cancel" => "runtime.checkpoint-redrive-cancellation-transition",
        "runtime.queued_work_keeps_pending_input" => {
            "runtime.live-turn-keeps-queued-input-hidden-transition"
        }
        "runtime.queued_turn_input_completion" => {
            "runtime.queued-turn-input-claim-completion-transition"
        }
        "runtime.command_only_queue_drain" => "runtime.command-only-queue-drain-transition",
        "runtime.command_before_turn_work" => "runtime.command-before-turn-work-transition",
        "runtime.observation_replay_preserves_input" => {
            "runtime.observer-reconnect-preserves-input-transition"
        }
        "standard.empty_provider_response_error" => {
            "standard.empty-provider-response-terminal-error-transition"
        }
        "standard.provider_error_without_checkpoint" => {
            "standard.provider-error-no-checkpoint-transition"
        }
        "standard.native_tool_loop_reenters_model" => {
            "standard.native-tool-result-model-reentry-transition"
        }
        "standard.tool_failure_feedback_reenters_model" => {
            "standard.tool-failure-feedback-model-reentry-transition"
        }
        "standard.max_turns_after_tool_result" => "standard.max-turns-after-tool-result-transition",
        "standard.parallel_tool_results_checkpoint_once" => {
            "standard.parallel-tool-results-single-checkpoint-transition"
        }
        "standard.streamed_text_finalizes_once" => {
            "standard.streamed-text-duplicate-free-finalization-transition"
        }
        "standard.initial_request_projection" => "standard.initial-request-projection-transition",
        "rlm.lashlang_cell_exec_continues" => "rlm.lashlang-cell-exec-continue-transition",
        "rlm.streamed_lashlang_cell_exec_persists_trajectory" => {
            "rlm.streamed-lashlang-cell-exec-trajectory-transition"
        }
        "rlm.exec_error_max_turn_stop" => "rlm.exec-error-max-turn-stop-transition",
        "rlm.exec_tool_control_frame_switch_terminal" => {
            "rlm.exec-tool-control-frame-switch-terminal-transition"
        }
        "rlm.exec_tool_control_fail_terminal" => "rlm.exec-tool-control-fail-terminal-transition",
        "rlm.exec_result_no_tool_call_replay" => {
            "rlm.exec-result-without-tool-call-replay-transition"
        }
        "rlm.typed_schema_mismatch_repair_loop" => "rlm.typed-schema-mismatch-repair-transition",
        "rlm.typed_schema_any_of_mismatch" => "rlm.typed-anyof-mismatch-repair-transition",
        semantic if semantic.starts_with("rlm.") => "rlm.provider-repair-or-finish-transition",
        "agent.shell_results_are_data" => "agent.shell-result-data-transition",
        "agent.shell_output_print_projection_survives" => {
            "agent.shell-output-print-projection-transition"
        }
        "agent.foreground_tool_call_round_trip" => {
            "agent.foreground-tool-call-round-trip-transition"
        }
        "agent.durable_input_suspension_resolution" => {
            "agent.durable-input-suspend-resolve-transition"
        }
        "agent.tuple_values_finish_as_json_arrays" => "agent.tuple-finish-json-array-transition",
        semantic if semantic.starts_with("agent.") => "agent.process-graph-transition",
        _ => "scenario.generic-generated-transition",
    }
}

fn scenario_evidence_boundary_kind(evidence: &str) -> &'static str {
    match evidence {
        "queued_ingress" => "queued_ingress",
        "cancellation" => "cancellation",
        "process_wake" => "process_wake",
        "worker_stale_completion" => "worker",
        "lease_time" => "lease_time",
        "provider_turn" => "provider",
        "provider_event" => "provider_event",
        "tool_result" => "tool",
        "max_turn_stop" => "trigger",
        "final_value" => "trigger",
        "observer_convergence" | "observer_reconnect" => "observer",
        "runtime_session_graph" => "ingress/provider",
        "exec_code" => "exec_code",
        "trigger" => "trigger",
        "durable_effect" => "durable_effect",
        "multi_session" => "multi_session",
        "provider_mutation" => "provider_mutation",
        "backend_failure" => "backend_failure",
        _ => "unknown",
    }
}

fn scenario_evidence_assertion(evidence: &str) -> &'static str {
    match evidence {
        "queued_ingress" => "queued input has stable source key and remains explicit work",
        "cancellation" => "cancellation targets an existing generated boundary",
        "process_wake" => "process wake crosses runtime queued-work claim/dedupe DTOs",
        "worker_stale_completion" => {
            "worker failover advances lease fencing and rejects stale completion"
        }
        "lease_time" => "lease ticks remain monotonic under scheduler delivery",
        "provider_turn" => {
            "provider completion is delivered through scripted provider runtime path"
        }
        "provider_event" => "provider event release is scheduler-owned generated evidence",
        "tool_result" => "tool result crosses runtime effect-controller output DTO",
        "max_turn_stop" => {
            "semantic proof records explicit max-turn stopped outcome after a tool result"
        }
        "final_value" => {
            "semantic proof records typed final value outcome and terminal event facts"
        }
        "observer_convergence" => "observer sees the generated final provider turn",
        "runtime_session_graph" => "session graph advances with generated ingress/provider turns",
        "exec_code" => "exec-code result crosses runtime effect-controller outcome DTO",
        "trigger" => "trigger delivery carries stable trigger identity",
        "durable_effect" => "durable effect records first completion and replay evidence",
        "multi_session" => "trace slice contains at least two generated sessions",
        "observer_reconnect" => "observer reconnect converges to the same session state",
        "provider_mutation" => "mutated provider script executes through real provider parsers",
        "backend_failure" => "backend failure is classified as a production StoreError shape",
        _ => "scenario evidence is selected from generated trace events",
    }
}

fn scenario_negative_fixture_for_contract(
    contract: &ScenarioContractSpec,
    selected_evidence: &[ScenarioEvidenceSelection],
) -> ScenarioNegativeFixture {
    match contract.semantic_oracle {
        "runtime.checkpoint_redrive_cancel" => {
            return scenario_negative_fixture("operational_coverage_missing_cancellation");
        }
        "runtime.queued_work_keeps_pending_input" => {
            return scenario_negative_fixture("queued_input_operational_missing");
        }
        "runtime.command_before_turn_work" => {
            return scenario_negative_fixture("trigger_wakeup_operational_missing");
        }
        "standard.max_turns_after_tool_result" => {
            return scenario_negative_fixture("standard_max_turn_stop_missing");
        }
        "standard.provider_error_without_checkpoint" => {
            return scenario_negative_fixture("standard_provider_error_missing_parser_matrix");
        }
        "rlm.typed_finish_emits_outcome_and_done" => {
            return scenario_negative_fixture("rlm_typed_finish_terminal_event_missing");
        }
        "rlm.empty_options_natural_default" => {
            return scenario_negative_fixture("rlm_empty_options_default_mode_broken");
        }
        "rlm.lashlang_cell_exec_continues" => {
            return scenario_negative_fixture("rlm_lashlang_cell_missing_exec_outcome");
        }
        "agent.tuple_values_finish_as_json_arrays" => {
            return scenario_negative_fixture("agent_tuple_json_array_shape_broken");
        }
        _ => {}
    }
    let has = |evidence: &str| {
        selected_evidence
            .iter()
            .any(|selection| selection.evidence == evidence)
            || semantic_scenario_evidence(contract.semantic_oracle).contains(&evidence)
    };
    if has("backend_failure") {
        return scenario_negative_fixture("backend_retry_runtime_completion_missing");
    }
    if has("provider_mutation") {
        if contract.suite == "standard"
            && contract
                .semantic_oracle
                .contains("provider_error_without_checkpoint")
        {
            return scenario_negative_fixture("standard_provider_error_missing_parser_matrix");
        }
        return scenario_negative_fixture("provider_mutation_runtime_completion_missing");
    }
    if has("worker_stale_completion") {
        return scenario_negative_fixture("worker_failover_stale_rejection_missing");
    }
    if has("cancellation") {
        return scenario_negative_fixture("operational_coverage_missing_cancellation");
    }
    if has("queued_ingress") {
        return scenario_negative_fixture("queued_input_operational_missing");
    }
    if has("trigger") {
        return scenario_negative_fixture("trigger_wakeup_operational_missing");
    }
    if has("process_wake") {
        if contract.suite == "agent" {
            return scenario_negative_fixture("agent_parallel_join_missing_wake_session");
        }
        return scenario_negative_fixture("process_wake_operational_missing");
    }
    if has("exec_code") || contract.suite == "rlm" {
        return scenario_negative_fixture("rlm_lashlang_cell_missing_exec_outcome");
    }
    if contract.suite == "standard" {
        return scenario_negative_fixture("standard_provider_error_missing_parser_matrix");
    }
    if contract.suite == "agent" {
        return scenario_negative_fixture("agent_parallel_join_missing_wake_session");
    }
    scenario_negative_fixture("scheduler_owned_provider_completion_missing_evidence")
}

fn scenario_negative_fixture(fixture_id: &str) -> ScenarioNegativeFixture {
    match fixture_id {
        "operational_coverage_missing_cancellation" => ScenarioNegativeFixture {
            fixture_id: "operational-coverage-missing-cancellation",
            fixture_path: "crates/lash-sim/failure-fixtures/operational-coverage-missing-cancellation.json",
            expected_oracle_id: "sim.oracle.scheduler-owned-runtime-completions.v1",
            expected_reason_contains: "Cancellation",
        },
        "queued_input_operational_missing" => ScenarioNegativeFixture {
            fixture_id: "queued-input-operational-missing",
            fixture_path: "crates/lash-sim/failure-fixtures/queued-input-operational-missing.json",
            expected_oracle_id: "sim.oracle.state-machine-semantic-invariants.v1",
            expected_reason_contains: "queued active-turn input",
        },
        "trigger_wakeup_operational_missing" => ScenarioNegativeFixture {
            fixture_id: "trigger-wakeup-operational-missing",
            fixture_path: "crates/lash-sim/failure-fixtures/trigger-wakeup-operational-missing.json",
            expected_oracle_id: "sim.oracle.state-machine-semantic-invariants.v1",
            expected_reason_contains: "trigger wakeup routes",
        },
        "process_wake_operational_missing" => ScenarioNegativeFixture {
            fixture_id: "process-wake-operational-missing",
            fixture_path: "crates/lash-sim/failure-fixtures/process-wake-operational-missing.json",
            expected_oracle_id: "sim.oracle.state-machine-semantic-invariants.v1",
            expected_reason_contains: "duplicate delivery/replay semantics",
        },
        "standard_provider_error_missing_parser_matrix" => ScenarioNegativeFixture {
            fixture_id: "standard-provider-error-missing-parser-matrix",
            fixture_path: "crates/lash-sim/failure-fixtures/standard-provider-error-missing-parser-matrix.json",
            expected_oracle_id: "sim.oracle.scenario-mini.standard.provider-error-without-checkpoint.v1",
            expected_reason_contains: "no scheduler-owned provider failure before checkpoint",
        },
        "standard_max_turn_stop_missing" => ScenarioNegativeFixture {
            fixture_id: "standard-max-turn-stop-missing",
            fixture_path: "crates/lash-sim/failure-fixtures/standard-max-turn-stop-missing.json",
            expected_oracle_id: "sim.oracle.scenario.standard-contract.v1:standard_protocol_scenario_max_turns_terminates_after_tool_result",
            expected_reason_contains: "fixed-source replay validation",
        },
        "rlm_lashlang_cell_missing_exec_outcome" => ScenarioNegativeFixture {
            fixture_id: "rlm-lashlang-cell-missing-exec-outcome",
            fixture_path: "crates/lash-sim/failure-fixtures/rlm-lashlang-cell-missing-exec-outcome.json",
            expected_oracle_id: "sim.oracle.scenario-mini.rlm.lashlang-cell-exec-continues.v1",
            expected_reason_contains: "did not continue after exec",
        },
        "rlm_typed_finish_terminal_event_missing" => ScenarioNegativeFixture {
            fixture_id: "rlm-typed-finish-terminal-event-missing",
            fixture_path: "crates/lash-sim/failure-fixtures/rlm-typed-finish-terminal-event-missing.json",
            expected_oracle_id: "sim.oracle.scenario.rlm-contract.v1:rlm_protocol_scenario_typed_finish_emits_turn_outcome_and_done",
            expected_reason_contains: "fixed-source replay validation",
        },
        "rlm_empty_options_default_mode_broken" => ScenarioNegativeFixture {
            fixture_id: "rlm-empty-options-default-mode-broken",
            fixture_path: "crates/lash-sim/failure-fixtures/rlm-empty-options-default-mode-broken.json",
            expected_oracle_id: "sim.oracle.scenario.rlm-contract.v1:rlm_protocol_scenario_empty_turn_options_use_natural_default",
            expected_reason_contains: "fixed-source replay validation",
        },
        "agent_parallel_join_missing_wake_session" => ScenarioNegativeFixture {
            fixture_id: "agent-parallel-join-missing-wake-session",
            fixture_path: "crates/lash-sim/failure-fixtures/agent-parallel-join-missing-wake-session.json",
            expected_oracle_id: "sim.oracle.scenario-mini.agent.parallel-spawn-join-determinism.v1",
            expected_reason_contains: "did not record deterministic process/worker ordering",
        },
        "agent_tuple_json_array_shape_broken" => ScenarioNegativeFixture {
            fixture_id: "agent-tuple-json-array-shape-broken",
            fixture_path: "crates/lash-sim/failure-fixtures/agent-tuple-json-array-shape-broken.json",
            expected_oracle_id: "sim.oracle.scenario.agent-contract.v1:agent_scenario_tuple_values_finish_as_json_arrays",
            expected_reason_contains: "fixed-source replay validation",
        },
        "provider_mutation_runtime_completion_missing" => ScenarioNegativeFixture {
            fixture_id: "provider-mutation-runtime-completion-missing",
            fixture_path: "crates/lash-sim/failure-fixtures/provider-mutation-runtime-completion-missing.json",
            expected_oracle_id: "sim.oracle.scheduler-owned-runtime-completions.v1",
            expected_reason_contains: "for ProviderMutation",
        },
        "worker_failover_stale_rejection_missing" => ScenarioNegativeFixture {
            fixture_id: "worker-failover-stale-rejection-missing",
            fixture_path: "crates/lash-sim/failure-fixtures/worker-failover-stale-rejection-missing.json",
            expected_oracle_id: "sim.oracle.scheduler-owned-runtime-completions.v1",
            expected_reason_contains: "for Worker",
        },
        "backend_retry_runtime_completion_missing" => ScenarioNegativeFixture {
            fixture_id: "backend-retry-runtime-completion-missing",
            fixture_path: "crates/lash-sim/failure-fixtures/backend-retry-runtime-completion-missing.json",
            expected_oracle_id: "sim.oracle.scheduler-owned-runtime-completions.v1",
            expected_reason_contains: "for BackendFailure",
        },
        _ => ScenarioNegativeFixture {
            fixture_id: "scheduler-owned-provider-completion-missing-evidence",
            fixture_path: "crates/lash-sim/failure-fixtures/scheduler-owned-provider-completion-missing-evidence.json",
            expected_oracle_id: "sim.oracle.scheduler-owned-runtime-completions.v1",
            expected_reason_contains: "delivered without pending runtime boundary evidence",
        },
    }
}

pub(super) fn select_scenario_contract_evidence(
    contract: &ScenarioContractSpec,
    event_lines: &[TraceEventLine],
) -> Result<Vec<ScenarioEvidenceSelection>, FixedScriptRunnerError> {
    let mut selected = Vec::new();
    let mut generated_fact_trace_alias = None;
    if matches!(contract.suite, "standard" | "rlm" | "agent") {
        let (trace_alias, facts) = select_scenario_contract_fact_trace(contract, event_lines)
            .map_err(|reason| {
                FixedScriptRunnerError::Assertion(format!(
                    "scenario contract `{}` could not select generated semantic facts: {reason}",
                    contract.test_name
                ))
            })?;
        generated_fact_trace_alias = Some(trace_alias.clone());
        for fact in facts {
            for boundary_id in fact.boundary_ids {
                let line = event_lines
                    .iter()
                    .find(|line| {
                        line.trace_alias == trace_alias && line.event.boundary_id == boundary_id
                    })
                    .ok_or_else(|| {
                        FixedScriptRunnerError::Assertion(format!(
                            "scenario contract `{}` generated fact `{}` selected missing boundary `{boundary_id}` in trace `{trace_alias}`",
                            contract.test_name, fact.fact
                        ))
                    })?;
                selected.push(ScenarioEvidenceSelection {
                    evidence: format!("semantic_fact:{}", fact.fact),
                    trace_alias: line.trace_alias.clone(),
                    boundary_id: line.event.boundary_id.clone(),
                    boundary_kind: line.event.kind,
                    sequence: line.event.sequence,
                });
            }
        }
    }
    let mut evidence_names = BTreeSet::new();
    for evidence in contract
        .required_sim_evidence
        .iter()
        .copied()
        .chain(semantic_scenario_evidence(contract.semantic_oracle))
        .filter(|evidence| evidence_names.insert(*evidence))
    {
        if let Some(trace_alias) = generated_fact_trace_alias.as_deref() {
            let scoped_event_lines = event_lines
                .iter()
                .filter(|line| line.trace_alias == trace_alias)
                .cloned()
                .collect::<Vec<_>>();
            let matches = select_event_lines_for_evidence(evidence, &scoped_event_lines);
            if matches.is_empty() {
                return Err(FixedScriptRunnerError::Assertion(format!(
                    "scenario contract `{}` could not select generated trace evidence `{}` from fact-owning trace `{trace_alias}`",
                    contract.test_name, evidence
                )));
            }
            selected.extend(matches.into_iter().map(|line| ScenarioEvidenceSelection {
                evidence: evidence.to_string(),
                trace_alias: line.trace_alias.clone(),
                boundary_id: line.event.boundary_id.clone(),
                boundary_kind: line.event.kind,
                sequence: line.event.sequence,
            }));
        } else {
            let matches = select_event_lines_for_evidence(evidence, event_lines);
            if matches.is_empty() {
                return Err(FixedScriptRunnerError::Assertion(format!(
                    "scenario contract `{}` could not select generated trace evidence `{}`",
                    contract.test_name, evidence
                )));
            }
            selected.extend(matches.into_iter().map(|line| ScenarioEvidenceSelection {
                evidence: evidence.to_string(),
                trace_alias: line.trace_alias.clone(),
                boundary_id: line.event.boundary_id.clone(),
                boundary_kind: line.event.kind,
                sequence: line.event.sequence,
            }));
        }
    }
    if contract.semantic_oracle == "runtime.queued_turn_input_completion" {
        let matches = select_queued_turn_followup_provider_evidence(event_lines);
        if matches.is_empty() {
            return Err(FixedScriptRunnerError::Assertion(format!(
                "scenario contract `{}` could not select same-actor provider completion after queued input",
                contract.test_name
            )));
        }
        selected.extend(matches.into_iter().map(|line| ScenarioEvidenceSelection {
            evidence: "queued_turn_followup_provider".to_string(),
            trace_alias: line.trace_alias.clone(),
            boundary_id: line.event.boundary_id.clone(),
            boundary_kind: line.event.kind,
            sequence: line.event.sequence,
        }));
    }
    Ok(selected)
}

fn select_scenario_contract_fact_trace(
    contract: &ScenarioContractSpec,
    event_lines: &[TraceEventLine],
) -> Result<(String, Vec<crate::oracles::ScenarioContractGeneratedFact>), String> {
    let mut by_trace: BTreeMap<&str, Vec<crate::scheduler::DeliveredBoundary>> = BTreeMap::new();
    for line in event_lines {
        by_trace
            .entry(line.trace_alias.as_str())
            .or_default()
            .push(line.event.clone());
    }
    let mut failures = Vec::new();
    for (trace_alias, events) in by_trace {
        match scenario_contract_generated_facts(contract, &events) {
            Ok(facts) => return Ok((trace_alias.to_string(), facts)),
            Err(reason) => failures.push(format!("{trace_alias}: {reason}")),
        }
    }
    Err(format!(
        "no single generated trace contained the contract-owned fact graph; {}",
        failures.join("; ")
    ))
}

fn semantic_scenario_evidence(semantic_oracle: &str) -> Vec<&'static str> {
    match semantic_oracle {
        "runtime.advisory_lease_head_cas" | "runtime.dead_lease_reclaim_rejects_stale" => {
            vec!["worker_stale_completion"]
        }
        "runtime.checkpoint_redrive_cancel" => vec!["queued_ingress", "cancellation"],
        "runtime.queued_work_keeps_pending_input" | "runtime.queued_turn_input_completion" => {
            vec!["queued_ingress", "provider_turn"]
        }
        "runtime.command_only_queue_drain" => vec!["queued_ingress", "lease_time"],
        "runtime.command_before_turn_work" => vec!["trigger", "queued_ingress", "lease_time"],
        "runtime.observation_replay_preserves_input" => vec!["observer_reconnect"],
        _ => Vec::new(),
    }
}

fn select_event_lines_for_evidence<'a>(
    evidence: &str,
    event_lines: &'a [TraceEventLine],
) -> Vec<&'a TraceEventLine> {
    match evidence {
        "multi_session" => {
            let mut actors = BTreeSet::new();
            event_lines
                .iter()
                .filter(|line| {
                    line.event.kind == BoundaryKind::Ingress
                        && actors.insert(line.event.actor_alias.clone())
                })
                .take(2)
                .collect()
        }
        "runtime_session_graph" => event_lines
            .iter()
            .filter(|line| {
                matches!(
                    line.event.kind,
                    BoundaryKind::Ingress | BoundaryKind::Provider
                )
            })
            .take(2)
            .collect(),
        "process_wake" => select_process_wake_evidence(event_lines),
        "durable_effect" => select_durable_effect_evidence(event_lines),
        _ => event_lines
            .iter()
            .filter(|line| event_satisfies_scenario_evidence(&line.event, evidence))
            .take(2)
            .collect(),
    }
}

fn select_queued_turn_followup_provider_evidence(
    event_lines: &[TraceEventLine],
) -> Vec<&TraceEventLine> {
    for queued in event_lines.iter().filter(|line| {
        event_satisfies_scenario_evidence(&line.event, "queued_ingress")
            && line
                .event
                .observed
                .get("ingress_mode")
                .and_then(Value::as_str)
                == Some("active_turn")
    }) {
        if let Some(provider) = event_lines
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
            return vec![queued, provider];
        }
    }
    Vec::new()
}

fn select_process_wake_evidence(event_lines: &[TraceEventLine]) -> Vec<&TraceEventLine> {
    let mut by_dedupe_key = BTreeMap::<String, Vec<&TraceEventLine>>::new();
    for line in event_lines
        .iter()
        .filter(|line| event_satisfies_scenario_evidence(&line.event, "process_wake"))
    {
        let Some(dedupe_key) = line
            .event
            .observed
            .get("dedupe_key")
            .and_then(Value::as_str)
        else {
            continue;
        };
        by_dedupe_key
            .entry(dedupe_key.to_string())
            .or_default()
            .push(line);
    }
    if let Some((_dedupe_key, events)) = by_dedupe_key
        .iter()
        .find(|(_dedupe_key, events)| events.len() >= 2)
    {
        return events.iter().copied().take(2).collect();
    }
    event_lines
        .iter()
        .filter(|line| event_satisfies_scenario_evidence(&line.event, "process_wake"))
        .take(2)
        .collect()
}

fn select_durable_effect_evidence(event_lines: &[TraceEventLine]) -> Vec<&TraceEventLine> {
    let mut by_durable_key = BTreeMap::<String, Vec<&TraceEventLine>>::new();
    for line in event_lines
        .iter()
        .filter(|line| event_satisfies_scenario_evidence(&line.event, "durable_effect"))
    {
        let Some(durable_key) = line
            .event
            .observed
            .get("durable_key")
            .and_then(Value::as_str)
        else {
            continue;
        };
        by_durable_key
            .entry(durable_key.to_string())
            .or_default()
            .push(line);
    }
    if let Some((_durable_key, events)) = by_durable_key.iter().find(|(_durable_key, events)| {
        events
            .iter()
            .any(|line| line.event.observed.get("replayed").and_then(Value::as_bool) == Some(false))
            && events.iter().any(|line| {
                line.event.observed.get("replayed").and_then(Value::as_bool) == Some(true)
            })
    }) {
        return events.iter().copied().take(2).collect();
    }
    event_lines
        .iter()
        .filter(|line| event_satisfies_scenario_evidence(&line.event, "durable_effect"))
        .take(2)
        .collect()
}

fn event_satisfies_scenario_evidence(
    event: &crate::scheduler::DeliveredBoundary,
    evidence: &str,
) -> bool {
    match evidence {
        "queued_ingress" => {
            event.kind == BoundaryKind::QueuedIngress
                && event
                    .observed
                    .get("source_key")
                    .and_then(Value::as_str)
                    .is_some()
        }
        "cancellation" => {
            event.kind == BoundaryKind::Cancellation
                && event
                    .observed
                    .get("target")
                    .and_then(Value::as_str)
                    .is_some()
        }
        "process_wake" => {
            event.kind == BoundaryKind::ProcessWake
                && event.observed.get("runtime_process_wake").is_some()
        }
        "worker_stale_completion" => {
            event.kind == BoundaryKind::Worker
                && event
                    .observed
                    .get("stale_completion_rejected")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                && event
                    .observed
                    .get("process_stale_completion_rejected")
                    .and_then(Value::as_bool)
                    == Some(true)
                && event
                    .observed
                    .get("process_stale_output_absent")
                    .and_then(Value::as_bool)
                    == Some(true)
                && event
                    .observed
                    .get("process_terminal_writer")
                    .and_then(Value::as_str)
                    == Some("successor")
                && event
                    .observed
                    .get("process_terminal_event_count")
                    .and_then(Value::as_u64)
                    == Some(1)
        }
        "lease_time" => event.kind == BoundaryKind::LeaseTime,
        "provider_turn" => event.kind == BoundaryKind::Provider,
        "provider_event" => {
            event.kind == BoundaryKind::ProviderEvent
                && event
                    .observed
                    .get("provider_event_release")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        }
        "tool_result" => {
            event.kind == BoundaryKind::Tool && event.observed.get("runtime_tool_output").is_some()
        }
        "max_turn_stop" => {
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
        }
        "final_value" => {
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
        }
        "observer_convergence" => event.kind == BoundaryKind::Observer,
        "exec_code" => {
            event.kind == BoundaryKind::ExecCode
                && event.observed.get("runtime_effect_outcome").is_some()
        }
        "trigger" => {
            event.kind == BoundaryKind::Trigger
                && event
                    .observed
                    .get("trigger_delivered")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        }
        "durable_effect" => {
            event.kind == BoundaryKind::DurableEffect
                && event.observed.get("runtime_effect").is_some()
        }
        "observer_reconnect" => {
            event.kind == BoundaryKind::Observer
                && event
                    .observed
                    .get("reconnected")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        }
        "provider_mutation" => {
            event.kind == BoundaryKind::ProviderMutation
                && event
                    .observed
                    .pointer("/provider_parser_matrix/matrix/real_provider_parser_execution")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        }
        "backend_failure" => event.kind == BoundaryKind::BackendFailure,
        _ => false,
    }
}
