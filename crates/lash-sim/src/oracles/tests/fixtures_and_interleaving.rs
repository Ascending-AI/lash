use super::*;

pub(super) fn mutate_contract_execution(
    events: &mut [DeliveredBoundary],
    contract: &str,
    mut mutate: impl FnMut(&mut Value),
) {
    let event = events
        .iter_mut()
        .find(|event| {
            event
                .observed
                .pointer("/contract_execution/contract")
                .and_then(Value::as_str)
                == Some(contract)
        })
        .expect("contract execution event");
    mutate(
        event
            .observed
            .get_mut("contract_execution")
            .expect("observed contract execution"),
    );
    mutate(
        event
            .payload
            .get_mut("contract_execution")
            .expect("payload contract execution"),
    );
}

pub(super) fn semantic_summary() -> AbstractWorldSummary {
    AbstractWorldSummary::with_digest(
        2,
        29,
        vec![
            SessionAbstractSummary {
                alias: "session-001".to_string(),
                opened: true,
                ingress_count: 1,
                provider_turns: vec![
                    provider_turn_summary("answer for session-001 turn 1", 1, 3, 2),
                    provider_turn_summary("answer for session-001 turn 2", 2, 5, 4),
                    provider_turn_summary("answer for session-001 turn 3", 3, 7, 6),
                ],
                tool_outputs: vec!["tool result for session-001".to_string()],
                exec_code_outputs: vec!["exec result for session-001".to_string()],
                observer_turn_indices: vec![3],
                observer_reconnects: 1,
                queued_ingress_count: 1,
                cancellation_count: 1,
                trigger_count: 4,
                backend_failure_count: 2,
                provider_mutation_count: 3,
                process_wake_count: 2,
                process_lifecycle_count: 1,
                durable_effect_keys: vec!["durable/session-001".to_string()],
                lease_time_ticks: vec![1, 2],
                checkpoint_commit_count: 0,
                checkpoint_component_stored_count: 0,
                checkpoint_component_ref_count: 0,
                checkpoint_head_revision: 0,
            },
            SessionAbstractSummary {
                alias: "session-002".to_string(),
                opened: true,
                ingress_count: 1,
                provider_turns: vec![
                    provider_turn_summary("answer for session-002 turn 1", 1, 3, 2),
                    provider_turn_summary("answer for session-002 turn 2", 2, 5, 4),
                ],
                tool_outputs: Vec::new(),
                exec_code_outputs: Vec::new(),
                observer_turn_indices: vec![2],
                observer_reconnects: 0,
                queued_ingress_count: 0,
                cancellation_count: 0,
                trigger_count: 0,
                backend_failure_count: 0,
                provider_mutation_count: 0,
                process_wake_count: 0,
                process_lifecycle_count: 0,
                durable_effect_keys: Vec::new(),
                lease_time_ticks: vec![1, 2],
                checkpoint_commit_count: 0,
                checkpoint_component_stored_count: 0,
                checkpoint_component_ref_count: 0,
                checkpoint_head_revision: 0,
            },
        ],
        vec![DurableEffectAbstractSummary {
            durable_key: "durable/session-001".to_string(),
            execution_count: 1,
            replay_count: 1,
            result_digest: "digest".to_string(),
        }],
        vec![WorkerAbstractSummary {
            worker_alias: "worker-001".to_string(),
            session_alias: "session-001".to_string(),
            active_incarnation_id: "worker-001:incarnation-002".to_string(),
            active_fencing_token: 2,
            lease_owner_changes: 1,
            stale_completion_rejections: 1,
            process_stale_completion_rejected: true,
            process_stale_output_absent: true,
            process_terminal_writer: "successor".to_string(),
            process_terminal_event_count: 1,
        }],
    )
}

pub(super) fn provider_turn_summary(
    output: &str,
    exchange_count: u64,
    graph_node_count: u64,
    transcript_message_count: u64,
) -> ProviderTurnSummary {
    ProviderTurnSummary {
        output: output.to_string(),
        exchange_count: Some(exchange_count),
        graph_node_count: Some(graph_node_count),
        transcript_message_count: Some(transcript_message_count),
    }
}

pub(super) fn semantic_events() -> Vec<DeliveredBoundary> {
    let base = [
        delivered_with_payload(
            0,
            "session-001:provider:001",
            "session-001",
            BoundaryKind::Provider,
            json!({
                "text": "answer for session-001 turn 1",
                "runtime_completion": runtime_completion("provider_turn_completion", 0),
                "expected_provider_exchange_count": 1,
            }),
            json!({
                "provider_kind": "openai-compatible",
                "provider_output": "answer for session-001 turn 1",
                "success": true,
                "provider_exchange_count": 1,
                "runtime_contract": {"status": "passed"},
            }),
        ),
        delivered_with_payload(
            1,
            "session-001:queue:001",
            "session-001",
            BoundaryKind::QueuedIngress,
            json!({
                "active_turn_id": "session-001:provider:002",
                "ingress_mode": "active_turn",
                "source_key": "queue/session-001/001",
                "text": "queued follow-up hidden from live turn",
            }),
            json!({
                "ingress_mode": "active_turn",
                "input_id": "input-001",
                "input_state": "pending_active",
                "queued_ingress": true,
                "session": "session-001",
                "source_key": "queue/session-001/001",
                "active_turn_id": "session-001:provider:002",
            }),
        ),
        delivered_with_payload(
            2,
            "session-001:provider:002:provider-event:001:sse",
            "session-001",
            BoundaryKind::ProviderEvent,
            json!({
                "turn_boundary_id": "session-001:provider:002",
                "event_index": 1,
                "event_name": "sse",
            }),
            json!({
                "provider_event_release": true,
                "released_while_turn_pending": true,
                "turn_boundary_id": "session-001:provider:002",
            }),
        ),
        delivered_with_payload(
            2,
            "session-001:cancel:001",
            "session-001",
            BoundaryKind::Cancellation,
            json!({
                "target": "session-001:queue:001",
                "runtime_completion": {
                    "completion_family": "queued_input_cancellation",
                    "completion_units": [{"unit": "runtime:cancel_pending_turn_input", "at": 2}],
                    "ready_at": 2,
                    "registered_after": "session-001:queue:001"
                }
            }),
            json!({
                "cancel_outcome": "cancelled",
                "cancelled": true,
                "session": "session-001",
                "target": "session-001:queue:001",
            }),
        ),
        delivered_with_payload(
            3,
            "session-001:observer:reconnect:001",
            "session-001",
            BoundaryKind::Observer,
            json!({"runtime_completion": runtime_completion("observer_snapshot", 3)}),
            json!({"reconnected": true, "turn_index": 2}),
        ),
        delivered_with_payload(
            4,
            "session-001:provider:002",
            "session-001",
            BoundaryKind::Provider,
            json!({
                "text": "answer for session-001 turn 2",
                "runtime_completion": runtime_completion("provider_turn_completion", 4),
                "expected_provider_exchange_count": 2,
            }),
            json!({
                "provider_kind": "openai-compatible",
                "provider_output": "answer for session-001 turn 2",
                "success": true,
                "provider_exchange_count": 2,
                "runtime_contract": {"status": "passed"},
            }),
        ),
        delivered_with_payload(
            5,
            "session-002:provider:001",
            "session-002",
            BoundaryKind::Provider,
            json!({
                "text": "answer for session-002 turn 1",
                "runtime_completion": runtime_completion("provider_turn_completion", 5),
                "expected_provider_exchange_count": 1,
            }),
            json!({
                "provider_kind": "anthropic",
                "provider_output": "answer for session-002 turn 1",
                "success": true,
                "provider_exchange_count": 1,
                "runtime_contract": {"status": "passed"},
            }),
        ),
        delivered_with_payload(
            6,
            "session-002:provider:002",
            "session-002",
            BoundaryKind::Provider,
            json!({
                "text": "answer for session-002 turn 2",
                "runtime_completion": runtime_completion("provider_turn_completion", 6),
                "expected_provider_exchange_count": 2,
            }),
            json!({
                "provider_kind": "anthropic",
                "provider_output": "answer for session-002 turn 2",
                "success": true,
                "provider_exchange_count": 2,
                "runtime_contract": {"status": "passed"},
            }),
        ),
        delivered_with_payload(
            7,
            "session-001:process-wake:001",
            "session-001",
            BoundaryKind::ProcessWake,
            json!({
                "process_id": "process-001",
                "sequence": 1,
                "runtime_completion": runtime_completion("process_wake", 7),
            }),
            json!({
                "claimed_once": true,
                "runtime_process_wake": {
                    "process_id": "process-001",
                    "sequence": 1,
                    "event_invocation": {
                        "subject": {
                            "process_id": "process-001"
                        }
                    }
                },
                "runtime_queued_work": {
                    "claimed": true,
                    "source_key": "process:process-001:event:1:wake"
                },
                "session": "session-001",
                "wake_id": "wake:duplicate"
            }),
        ),
        delivered_with_payload(
            8,
            "session-001:process-wake:002",
            "session-001",
            BoundaryKind::ProcessWake,
            json!({
                "process_id": "process-001",
                "sequence": 1,
                "runtime_completion": runtime_completion("process_wake", 8),
            }),
            json!({
                "claimed_once": false,
                "runtime_process_wake": {
                    "process_id": "process-001",
                    "sequence": 1,
                    "event_invocation": {
                        "subject": {
                            "process_id": "process-001"
                        }
                    }
                },
                "runtime_queued_work": {
                    "claimed": false,
                    "source_key": "process:process-001:event:1:wake"
                },
                "session": "session-001",
                "wake_id": "wake:duplicate"
            }),
        ),
        delivered_with_payload(
            9,
            "worker-001:worker:001",
            "worker-001",
            BoundaryKind::Worker,
            json!({"runtime_completion": runtime_completion("worker_lease_completion", 9)}),
            json!({
                "stale_completion_rejected": true,
                "runtime_active_lease": {},
                "runtime_stale_completion": {},
            }),
        ),
        delivered_with_payload(
            10,
            "session-001:durable:001:first",
            "session-001",
            BoundaryKind::DurableEffect,
            json!({"runtime_completion": runtime_completion("durable_effect_completion", 10)}),
            json!({
                "durable_key": "durable/session-001",
                "replayed": false,
                "runtime_effect": {"local_executor_called": true},
                "result_digest": "digest",
                "execution_count": 1,
                "replay_count": 0,
            }),
        ),
        delivered_with_payload(
            11,
            "session-001:durable:001:replay",
            "session-001",
            BoundaryKind::DurableEffect,
            json!({"runtime_completion": runtime_completion("durable_effect_completion", 11)}),
            json!({
                "durable_key": "durable/session-001",
                "replayed": true,
                "runtime_effect": {"local_executor_called": false},
                "result_digest": "digest",
                "execution_count": 1,
                "replay_count": 1,
            }),
        ),
        delivered_with_payload(
            12,
            "session-001:tool:001",
            "session-001",
            BoundaryKind::Tool,
            json!({"runtime_completion": runtime_completion("tool_return", 12)}),
            json!({
                "runtime_tool_output": {},
                "runtime_tool_record": {},
                "execution_count": 1,
            }),
        ),
        delivered_with_payload(
            13,
            "session-001:exec:001",
            "session-001",
            BoundaryKind::ExecCode,
            json!({"runtime_completion": runtime_completion("exec_result", 13)}),
            json!({
                "runtime_effect_outcome": {
                    "result": {
                        "Ok": {
                            "calls": []
                        }
                    },
                    "type": "exec_code"
                },
                "execution_count": 1,
            }),
        ),
        delivered_with_payload(
            14,
            "session-001:trigger:001",
            "session-001",
            BoundaryKind::Trigger,
            json!({
                "session": "session-001",
                "source_key": "trigger/button/session-001/001",
                "started_process": true,
            }),
            json!({
                "occurrence_id": "trigger:abc",
                "reservation_count": 1,
                "session": "session-001",
                "source_key": "trigger/button/session-001/001",
                "started_process": true,
                "trigger_delivered": true,
            }),
        ),
        delivered_with_payload(
            15,
            "session-001:backend-failure:001",
            "session-001",
            BoundaryKind::BackendFailure,
            json!({
                "operation": "commit_runtime_state:001",
                "runtime_completion": runtime_completion("backend_retry_or_failure", 15),
            }),
            json!({
                "attempt": 1,
                "backend_failure": true,
                "operation": "commit_runtime_state:001",
                "production_store_error": {
                    "retryable_class": true,
                    "type": "lash_core::StoreError",
                    "variant": "HeadRevisionConflict"
                },
                "fault_injector": {"point": "after_begin"},
                "retryable": true,
                "store_error_class": "retryable_conflict"
            }),
        ),
        delivered_with_payload(
            16,
            "session-001:backend-failure:002",
            "session-001",
            BoundaryKind::BackendFailure,
            json!({
                "operation": "commit_runtime_state:001",
                "runtime_completion": runtime_completion("backend_retry_or_failure", 16),
            }),
            json!({
                "attempt": 2,
                "backend_failure": true,
                "operation": "commit_runtime_state:001",
                "production_store_error": {
                    "retryable_class": false,
                    "type": "lash_core::StoreError",
                    "variant": "SessionExecutionLeaseExpired"
                },
                "fault_injector": {"point": "commit_io"},
                "retryable": false,
                "store_error_class": "terminal_backend_error"
            }),
        ),
        delivered_with_payload(
            17,
            "session-001:provider-mutation:001",
            "session-001",
            BoundaryKind::ProviderMutation,
            json!({"runtime_completion": runtime_completion("provider_script_mutation", 17)}),
            json!({
                "mutation": "malformed_sse_chunk",
                "provider_parser_matrix": {
                    "matrix": {
                        "real_provider_parser_execution": true,
                        "provider_kinds": [
                            "anthropic",
                            "google_oauth",
                            "openai",
                            "openai-compatible"
                        ]
                    }
                }
            }),
        ),
        delivered_with_payload(
            18,
            "session-001:provider-mutation:002",
            "session-001",
            BoundaryKind::ProviderMutation,
            json!({"runtime_completion": runtime_completion("provider_script_mutation", 18)}),
            json!({
                "mutation": "rate_limit_error_envelope",
                "provider_parser_matrix": {
                    "matrix": {
                        "real_provider_parser_execution": true,
                        "provider_kinds": [
                            "anthropic",
                            "google_oauth",
                            "openai",
                            "openai-compatible"
                        ],
                        "proofs": [
                            {"provider_kind": "openai-compatible", "terminal_reason": "provider_error", "status": 429, "classification": {"kind": "Http", "retryable": true, "status": 429}},
                            {"provider_kind": "openai", "terminal_reason": "provider_error", "status": 429, "classification": {"kind": "Http", "retryable": true, "status": 429}},
                            {"provider_kind": "anthropic", "terminal_reason": "provider_error", "status": 429, "classification": {"kind": "Http", "retryable": true, "status": 429}},
                            {"provider_kind": "google_oauth", "terminal_reason": "provider_error", "status": 429, "classification": {"kind": "Http", "retryable": true, "status": 429}}
                        ]
                    }
                }
            }),
        ),
        delivered_with_payload(
            19,
            "session-001:provider-mutation:003",
            "session-001",
            BoundaryKind::ProviderMutation,
            json!({"runtime_completion": runtime_completion("provider_script_mutation", 19)}),
            json!({
                "mutation": "dropped_terminal_event",
                "provider_parser_matrix": {
                    "matrix": {
                        "real_provider_parser_execution": true,
                        "provider_kinds": [
                            "anthropic",
                            "google_oauth",
                            "openai",
                            "openai-compatible"
                        ],
                        "proofs": [
                            {"provider_kind": "openai-compatible", "terminal_reason": "provider_error", "classification": {"retryable": false}},
                            {"provider_kind": "openai", "terminal_reason": "provider_error", "classification": {"retryable": false}},
                            {"provider_kind": "anthropic", "terminal_reason": "provider_error", "classification": {"retryable": false}},
                            {"provider_kind": "google_oauth", "terminal_reason": "provider_error", "classification": {"retryable": false}}
                        ]
                    }
                }
            }),
        ),
        delivered_with_payload(
            20,
            "session-001:provider:003:provider-event:001:sse",
            "session-001",
            BoundaryKind::ProviderEvent,
            json!({
                "turn_boundary_id": "session-001:provider:003",
                "event_index": 1,
                "event_name": "sse",
            }),
            json!({
                "provider_event_release": true,
                "released_while_turn_pending": true,
                "turn_boundary_id": "session-001:provider:003",
            }),
        ),
        delivered_with_payload(
            21,
            "session-001:provider:003",
            "session-001",
            BoundaryKind::Provider,
            json!({
                "text": "answer for session-001 turn 3",
                "runtime_completion": runtime_completion("provider_turn_completion", 21),
                "expected_provider_exchange_count": 3,
            }),
            json!({
                "provider_kind": "openai-compatible",
                "provider_output": "answer for session-001 turn 3",
                "success": true,
                "provider_exchange_count": 3,
                "runtime_contract": {"status": "passed"},
            }),
        ),
        delivered_with_payload(
            22,
            "session-001:contract-execution:standard-max-turn-after-tool-result",
            "session-001",
            BoundaryKind::Trigger,
            json!({
                "session": "session-001",
                "source_key": "contract-execution/session-001/standard-max-turn-after-tool-result",
                "started_process": false,
                "contract_execution": standard_max_turn_execution_fixture()
            }),
            json!({
                "session": "session-001",
                "trigger_delivered": true,
                "source_key": "contract-execution/session-001/standard-max-turn-after-tool-result",
                "occurrence_id": "trigger:contract-execution-standard-max-turn-after-tool-result",
                "reservation_count": 1,
                "started_process": false,
                "contract_execution": standard_max_turn_execution_fixture()
            }),
        ),
    ];
    let mut events: Vec<DeliveredBoundary> = base.into();
    events.extend(contract_execution_fixture_events(24));
    events
}

pub(super) fn standard_max_turn_execution_fixture() -> serde_json::Value {
    let mut execution = replay_contract_execution_fixture("standard.max_turns_after_tool_result");
    execution
        .as_object_mut()
        .expect("contract execution object")
        .insert(
            "generated_anchor".to_string(),
            json!({
                "tool_boundary": "session-001:tool:001",
                "continuation_provider_boundary": "session-001:provider:003",
                "actor": "session-001",
                "tool_sequence": 12,
                "continuation_provider_sequence": 21,
                "same_actor_continuation": true
            }),
        );
    execution
}

pub(super) fn replay_contract_execution_fixture(contract: &str) -> serde_json::Value {
    crate::runner::replay_contract_execution(contract)
        .unwrap_or_else(|err| panic!("fixed contract execution fixture `{contract}` failed: {err}"))
}

pub(super) fn contract_execution_fixture_events(start_sequence: usize) -> Vec<DeliveredBoundary> {
    [
        "standard.initial_request_projection",
        "standard.empty_response_finishes",
        "standard.provider_error_without_checkpoint",
        "standard.native_tool_loop_reenters_model",
        "standard.parallel_tool_results_checkpoint_once",
        "standard.tool_failure_feedback_reenters_model",
        "standard.streamed_text_finalizes_once",
        "rlm.natural_prose_finalizes",
        "rlm.typed_prose_requires_finish",
        "rlm.finish_required_max_turn_stop",
        "rlm.exec_error_max_turn_stop",
        "rlm.typed_finish_emits_outcome_and_done",
        "rlm.finish_required_diagnostic_counts",
        "rlm.natural_diagnostic_counts",
        "rlm.cell_diagnostic_counts",
        "rlm.retired_marker_plain_lashlang_text",
        "rlm.lashlang_cell_exec_continues",
        "rlm.streamed_lashlang_cell_exec_persists_trajectory",
        "rlm.empty_options_natural_default",
        "rlm.exec_result_no_tool_call_replay",
        "rlm.exec_tool_control_frame_switch_terminal",
        "rlm.exec_tool_control_fail_terminal",
        "rlm.natural_allows_finish_value",
        "rlm.typed_schema_mismatch_repair_loop",
        "rlm.typed_schema_any_of_mismatch",
        "agent.foreground_tool_call_round_trip",
        "agent.started_process_tool_call_graph",
        "agent.durable_input_suspension_resolution",
        "agent.shell_results_are_data",
        "agent.shell_output_print_projection_survives",
        "agent.started_process_subagent_spawn",
        "agent.nested_process_start_await",
        "agent.session_turn_process_child",
        "agent.failed_child_preserves_failure_graph",
        "agent.parallel_spawn_and_join",
        "agent.tuple_values_finish_as_json_arrays",
    ]
    .into_iter()
    .enumerate()
    .map(|(offset, contract)| contract_execution_fixture_event(start_sequence + offset, contract))
    .collect()
}

pub(super) fn contract_execution_fixture_event(
    sequence: usize,
    contract: &str,
) -> DeliveredBoundary {
    let proof_id = contract.replace(['.', '_'], "-");
    let boundary_id = format!("session-001:contract-execution:{proof_id}");
    let source_key = format!("contract-execution/session-001/{proof_id}");
    let execution = replay_contract_execution_fixture(contract);
    delivered_with_payload(
        sequence,
        &boundary_id,
        "session-001",
        BoundaryKind::Trigger,
        json!({
            "session": "session-001",
            "source_key": source_key,
            "started_process": false,
            "contract_execution": execution.clone(),
        }),
        json!({
            "session": "session-001",
            "trigger_delivered": true,
            "source_key": source_key,
            "occurrence_id": format!("trigger:contract-execution-{proof_id}"),
            "reservation_count": 1,
            "started_process": false,
            "contract_execution": execution,
        }),
    )
}

pub(super) fn delivered_with_payload(
    sequence: usize,
    boundary_id: &str,
    actor_alias: &str,
    kind: BoundaryKind,
    payload: serde_json::Value,
    observed: serde_json::Value,
) -> DeliveredBoundary {
    DeliveredBoundary {
        schema: crate::scheduler::BOUNDARY_EVENT_SCHEMA.to_string(),
        sequence,
        scheduler: SchedulerDeliveryEvidence {
            scheduler_controlled: true,
            delivered_at: sequence as u64,
            ..SchedulerDeliveryEvidence::default()
        },
        boundary_id: boundary_id.to_string(),
        actor_alias: actor_alias.to_string(),
        kind,
        at: sequence as u64,
        label: format!("{kind:?}"),
        payload,
        observed,
    }
}

pub(super) fn runtime_completion(family: &str, ready_at: u64) -> serde_json::Value {
    json!({
        "completion_family": family,
        "completion_units": [
            {
                "unit": format!("runtime:{family}"),
                "at": ready_at
            }
        ],
        "ready_at": ready_at,
        "registered_after": "session-001:ingress"
    })
}

pub(super) fn provider_mutation_observed(mutation: &str) -> serde_json::Value {
    json!({
        "mutation": mutation,
        "provider_parser_matrix": {
            "matrix": {
                "real_provider_parser_execution": true,
                "provider_kinds": [
                    "anthropic",
                    "google_oauth",
                    "openai",
                    "openai-compatible"
                ]
            }
        }
    })
}

pub(super) fn provider_event(
    sequence: usize,
    actor: &str,
    turn_boundary_id: &str,
) -> DeliveredBoundary {
    delivered_with_payload(
        sequence,
        &format!("{turn_boundary_id}:event:{sequence}"),
        actor,
        BoundaryKind::ProviderEvent,
        json!({ "turn_boundary_id": turn_boundary_id }),
        json!({}),
    )
}

pub(super) fn provider_completion(
    sequence: usize,
    actor: &str,
    turn_boundary_id: &str,
) -> DeliveredBoundary {
    delivered_with_payload(
        sequence,
        turn_boundary_id,
        actor,
        BoundaryKind::Provider,
        json!({ "provider_kind": "openai" }),
        json!({ "provider_kind": "openai" }),
    )
}

#[test]
fn interleaving_oracle_passes_when_two_sessions_overlap() {
    // Turn A and turn B are both live (each has released a provider event)
    // before either completes.
    let events = vec![
        provider_event(0, "session-a", "turn-a"),
        provider_event(1, "session-b", "turn-b"),
        provider_completion(2, "session-a", "turn-a"),
        provider_completion(3, "session-b", "turn-b"),
    ];
    assert_eq!(peak_concurrent_live_turns(&events), 2);
    assert!(
        provider_turn_interleaving_depth(
            &events,
            &WorkloadExpectations::new(
                vec!["session-a".to_string(), "session-b".to_string()],
                2,
                0,
                0,
            )
        )
        .is_passed()
    );
}

#[test]
fn interleaving_oracle_fails_when_multi_session_turns_never_overlap() {
    // Two sessions each run a turn, but each turn completes before the next
    // one releases an event, so peak concurrency is 1.
    let events = vec![
        provider_event(0, "session-a", "turn-a"),
        provider_completion(1, "session-a", "turn-a"),
        provider_event(2, "session-b", "turn-b"),
        provider_completion(3, "session-b", "turn-b"),
    ];
    assert_eq!(peak_concurrent_live_turns(&events), 1);
    let verdict = provider_turn_interleaving_depth(
        &events,
        &WorkloadExpectations::new(
            vec!["session-a".to_string(), "session-b".to_string()],
            2,
            0,
            0,
        ),
    );
    assert!(!verdict.is_passed());
    assert_eq!(verdict.oracle_id, PROVIDER_TURN_INTERLEAVING_ORACLE);
}

#[test]
fn interleaving_oracle_is_exempt_only_for_a_declared_single_session() {
    let events = vec![
        provider_event(0, "session-a", "turn-a"),
        provider_completion(1, "session-a", "turn-a"),
    ];
    assert_eq!(peak_concurrent_live_turns(&events), 1);
    let verdict = provider_turn_interleaving_depth(
        &events,
        &WorkloadExpectations::new(vec!["session-a".to_string()], 1, 0, 0),
    );
    assert!(verdict.is_passed(), "{}", verdict.message);
    assert!(
        verdict
            .message
            .contains("the workload declared 1 session(s)"),
        "the exemption must be proved from the declaration: {}",
        verdict.message
    );
}

pub(super) fn suspend_resume_event(
    suspended_before: bool,
    before: u64,
    after: u64,
) -> DeliveredBoundary {
    delivered_with_payload(
        0,
        "suspend-tool:suspend-resume:001",
        "suspend-tool",
        BoundaryKind::Tool,
        json!({ "suspend_resume": true, "tool": "await_tool", "output": {"ok": true} }),
        json!({
            "session": "suspend-tool",
            "tool_output": {"ok": true},
            "runtime_suspend": {
                "suspend_kind": "tool",
                "turn_suspended_before_completion": suspended_before,
                "scheduler_delivered_completion": true,
                "resolve_accepted": true,
                "resumed_after_completion": after > before,
                "completed_event_count_before_resolution": before,
                "completed_event_count_after_resolution": after,
                "final_assistant_message": "resumed",
            },
        }),
    )
}

#[test]
fn suspend_resume_oracle_passes_when_turn_parked_then_resumed() {
    let events = vec![suspend_resume_event(true, 0, 1)];
    assert!(generated_suspend_resume(&events).is_passed());
}

#[test]
fn suspend_resume_oracle_fails_when_turn_ran_synchronously() {
    // The tool completed before the scheduler delivered the completion
    // boundary: the turn never actually parked.
    let events = vec![suspend_resume_event(false, 1, 1)];
    let verdict = generated_suspend_resume(&events);
    assert!(!verdict.is_passed());
    assert_eq!(verdict.oracle_id, GENERATED_SUSPEND_RESUME_ORACLE);
}

#[test]
fn suspend_resume_oracle_fails_when_the_suspend_class_is_absent() {
    // Anti-vacuity: with no suspend-resume boundary present, the oracle must
    // FAIL rather than pass on an absent class.
    let events = vec![provider_completion(0, "session-a", "turn-a")];
    let verdict = generated_suspend_resume(&events);
    assert!(!verdict.is_passed());
    assert!(verdict.message.contains("class is absent"));
}
