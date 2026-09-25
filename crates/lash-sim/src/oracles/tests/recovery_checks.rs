use super::*;

fn clean_live_failure_facts() -> LiveProviderFailureFacts {
    LiveProviderFailureFacts {
        provider_kind: "openai-compatible".to_string(),
        fault_kind: "malformed_sse_chunk".to_string(),
        offered_prose_deltas: 1,
        streamed_prose_deltas: 1,
        turn_was_live_parked: true,
        terminalized_failure: true,
        committed_assistant_message_nonempty: false,
        committed_final_values: 0,
        committed_prose_in_transcript: false,
    }
}

#[test]
fn live_provider_failure_oracle_passes_on_clean_terminalization_and_fails_on_committed_output() {
    let clean = clean_live_failure_facts();
    assert!(live_provider_failure_terminalizes(&clean).is_passed());

    // Negative: the turn finished successfully instead of terminalizing.
    assert!(
        !live_provider_failure_terminalizes(&LiveProviderFailureFacts {
            terminalized_failure: false,
            ..clean.clone()
        })
        .is_passed()
    );

    // Negative: the committed turn result leaked a non-empty assistant message.
    assert!(
        !live_provider_failure_terminalizes(&LiveProviderFailureFacts {
            committed_assistant_message_nonempty: true,
            ..clean.clone()
        })
        .is_passed()
    );

    // Negative: the turn committed a Final Value despite failing.
    assert!(
        !live_provider_failure_terminalizes(&LiveProviderFailureFacts {
            committed_final_values: 1,
            ..clean.clone()
        })
        .is_passed()
    );

    // Negative: the partial prose leaked into the committed transcript.
    assert!(
        !live_provider_failure_terminalizes(&LiveProviderFailureFacts {
            committed_prose_in_transcript: true,
            ..clean.clone()
        })
        .is_passed()
    );

    // Negative (anti-vacuity): no valid prose was offered before the fault, so
    // "no committed output" would be vacuous -> the oracle must NOT pass.
    assert!(
        !live_provider_failure_terminalizes(&LiveProviderFailureFacts {
            offered_prose_deltas: 0,
            ..clean.clone()
        })
        .is_passed()
    );

    // Negative: the turn was never observed live/parked.
    assert!(
        !live_provider_failure_terminalizes(&LiveProviderFailureFacts {
            turn_was_live_parked: false,
            ..clean
        })
        .is_passed()
    );
}

#[test]
fn live_provider_failure_coverage_requires_multiple_kinds_and_positions() {
    let base = clean_live_failure_facts();
    let combo = |kind: &str, prose: usize| LiveProviderFailureFacts {
        provider_kind: kind.to_string(),
        offered_prose_deltas: prose,
        streamed_prose_deltas: prose,
        ..base.clone()
    };

    // Positive: >= 2 kinds and >= 2 positions, all clean.
    assert!(
        live_provider_failure_coverage(&[combo("openai-compatible", 1), combo("anthropic", 2),])
            .is_passed()
    );

    // Negative: only one provider kind.
    assert!(
        !live_provider_failure_coverage(&[
            combo("openai-compatible", 1),
            combo("openai-compatible", 2)
        ])
        .is_passed()
    );

    // Negative: only one fault position.
    assert!(
        !live_provider_failure_coverage(&[combo("openai-compatible", 1), combo("anthropic", 1)])
            .is_passed()
    );

    // Negative: a single combo that itself leaked output fails the aggregate.
    assert!(
        !live_provider_failure_coverage(&[
            combo("openai-compatible", 1),
            LiveProviderFailureFacts {
                committed_assistant_message_nonempty: true,
                ..combo("anthropic", 2)
            },
        ])
        .is_passed()
    );
}

#[test]
fn replay_determinism_compares_durable_effect_outcomes() {
    let summary = |execution_count| {
        AbstractWorldSummary::with_digest(
            1,
            1,
            vec![],
            vec![DurableEffectAbstractSummary {
                durable_key: "sleep/session-001/001".to_string(),
                execution_count,
                replay_count: 1,
                result_digest: "digest".to_string(),
            }],
        )
    };

    assert!(replay_determinism(&summary(1), &summary(1)).is_passed());
    assert!(!replay_determinism(&summary(1), &summary(2)).is_passed());
}

#[test]
fn scheduler_owned_runtime_completion_oracle_rejects_missing_pending_evidence() {
    let verdict = scheduler_owned_runtime_completions(
        &[delivered_with_payload(
            0,
            "session-001:provider:001",
            "session-001",
            BoundaryKind::Provider,
            json!({}),
            json!({"provider_output": "answer"}),
        )],
        &WorkloadExpectations::default(),
    );

    assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);
    assert_eq!(verdict.oracle_id, SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE);
    assert!(
        verdict
            .message
            .contains("delivered without pending runtime boundary evidence")
    );
}

#[test]
fn scheduler_owned_runtime_completion_oracle_rejects_incomplete_pending_evidence() {
    for (name, completion) in [
        (
            "empty family",
            json!({
                "completion_family": "",
                "completion_units": [{"unit": "runtime:unit", "at": 0}],
                "ready_at": 0,
                "registered_after": "session-001:ingress"
            }),
        ),
        (
            "empty units",
            json!({
                "completion_family": "provider_turn_completion",
                "completion_units": [],
                "ready_at": 0,
                "registered_after": "session-001:ingress"
            }),
        ),
        (
            "wrong ready_at",
            json!({
                "completion_family": "provider_turn_completion",
                "completion_units": [{"unit": "runtime:unit", "at": 0}],
                "ready_at": 99,
                "registered_after": "session-001:ingress"
            }),
        ),
        (
            "empty registered_after",
            json!({
                "completion_family": "provider_turn_completion",
                "completion_units": [{"unit": "runtime:unit", "at": 0}],
                "ready_at": 0,
                "registered_after": ""
            }),
        ),
    ] {
        let verdict = scheduler_owned_runtime_completions(
            &[delivered_with_payload(
                0,
                "session-001:provider:001",
                "session-001",
                BoundaryKind::Provider,
                json!({"runtime_completion": completion}),
                json!({"provider_output": "answer"}),
            )],
            &WorkloadExpectations::default(),
        );

        assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);
        assert_eq!(verdict.oracle_id, SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE);
        assert!(
            verdict.message.contains("incomplete pending evidence"),
            "{name}: {}",
            verdict.message
        );
    }
}

#[test]
fn scheduler_owned_runtime_completion_kinds_match_runner_completion_kinds() {
    let runner_kinds: BTreeSet<BoundaryKind> =
        crate::runner::SCHEDULER_OWNED_RUNTIME_COMPLETION_KINDS
            .iter()
            .copied()
            .collect();
    let oracle_kinds: BTreeSet<BoundaryKind> = SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE_KINDS
        .iter()
        .copied()
        .collect();
    assert_eq!(
        runner_kinds, oracle_kinds,
        "runner scheduler-owned runtime completion kinds and oracle verification kinds must match exactly"
    );
    assert_eq!(
        crate::runner::SCHEDULER_OWNED_RUNTIME_COMPLETION_KINDS,
        SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE_KINDS,
        "runner and oracle lists must match in exact sequence"
    );
}

#[test]
fn boundary_kind_name_matches_serde_serialization() {
    let all_kinds = [
        BoundaryKind::Ingress,
        BoundaryKind::QueuedIngress,
        BoundaryKind::Provider,
        BoundaryKind::ProviderEvent,
        BoundaryKind::Tool,
        BoundaryKind::ExecCode,
        BoundaryKind::DurableEffect,
        BoundaryKind::Observer,
        BoundaryKind::Cancellation,
        BoundaryKind::Trigger,
        BoundaryKind::BackendFailure,
        BoundaryKind::ProviderMutation,
    ];
    assert_eq!(all_kinds.len(), 12, "must test all twelve boundary kinds");
    for kind in all_kinds {
        let serialized = serde_json::to_string(&kind).expect("serialization failed");
        let expected_name = serialized.trim_matches('"');
        assert_eq!(
            kind.to_string(),
            expected_name,
            "BoundaryKind Display for {kind:?} must match its serde-serialized snake_case string"
        );
        assert_eq!(
            expected_name.parse::<BoundaryKind>().expect("parse failed"),
            kind,
            "BoundaryKind FromStr must round-trip {kind:?} through its serde name"
        );
    }
}

#[test]
fn scheduler_owned_runtime_completion_oracle_rejects_missing_evidence_for_durable_effect_and_observer()
 {
    for kind in [BoundaryKind::DurableEffect, BoundaryKind::Observer] {
        let verdict = scheduler_owned_runtime_completions(
            &[delivered_with_payload(
                0,
                "session-001:boundary:001",
                "session-001",
                kind,
                json!({}),
                json!({}),
            )],
            &WorkloadExpectations::default(),
        );

        assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);
        assert_eq!(verdict.oracle_id, SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE);
        assert!(
            verdict
                .message
                .contains("delivered without pending runtime boundary evidence"),
            "for {kind:?}: {}",
            verdict.message
        );
    }
}

#[test]
fn scheduler_owned_runtime_completion_oracle_passes_with_all_eight_kinds_present() {
    let events = vec![
        delivered_with_payload(
            0,
            "session-001:provider:001",
            "session-001",
            BoundaryKind::Provider,
            json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ProviderTurnCompletion, 0)}),
            json!({}),
        ),
        delivered_with_payload(
            1,
            "session-001:cancellation:001",
            "session-001",
            BoundaryKind::Cancellation,
            json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::QueuedInputCancellation, 1)}),
            json!({}),
        ),
        delivered_with_payload(
            2,
            "session-001:backend-failure:001",
            "session-001",
            BoundaryKind::BackendFailure,
            json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::BackendRetryOrFailure, 2)}),
            json!({}),
        ),
        delivered_with_payload(
            3,
            "session-001:provider-mutation:001",
            "session-001",
            BoundaryKind::ProviderMutation,
            json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ProviderScriptMutation, 3)}),
            json!({}),
        ),
        delivered_with_payload(
            4,
            "session-001:tool:001",
            "session-001",
            BoundaryKind::Tool,
            json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ToolReturn, 4)}),
            json!({}),
        ),
        delivered_with_payload(
            5,
            "session-001:exec-code:001",
            "session-001",
            BoundaryKind::ExecCode,
            json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ExecResult, 5)}),
            json!({}),
        ),
        delivered_with_payload(
            6,
            "session-001:durable:001",
            "session-001",
            BoundaryKind::DurableEffect,
            json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::DurableEffectCompletion, 6)}),
            json!({}),
        ),
        delivered_with_payload(
            9,
            "session-001:observer:001",
            "session-001",
            BoundaryKind::Observer,
            json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ObserverSnapshot, 9)}),
            json!({}),
        ),
    ];

    let verdict = scheduler_owned_runtime_completions(&events, &WorkloadExpectations::default());
    assert_eq!(verdict.status, crate::trace::OracleStatus::Passed);
    assert_eq!(verdict.oracle_id, SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE);
}

#[test]
fn scheduler_owned_runtime_completion_oracle_fails_when_kind_is_missing() {
    for missing_kind in SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE_KINDS {
        let mut events = Vec::new();
        let mut seq = 0u64;
        for &kind in SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE_KINDS {
            if kind == *missing_kind {
                continue;
            }
            events.push(delivered_with_payload(
                seq as usize,
                &format!("boundary:{seq}"),
                "session-001",
                kind,
                json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ProviderTurnCompletion, seq)}),
                json!({}),
            ));
            seq += 1;
        }
        let verdict =
            scheduler_owned_runtime_completions(&events, &WorkloadExpectations::default());
        assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);
        assert_eq!(verdict.oracle_id, SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE);
        assert!(
            verdict.message.contains(&format!("{missing_kind:?}")),
            "expected message to name missing {missing_kind:?}, got: {}",
            verdict.message
        );
    }
}

#[test]
fn standard_provider_error_oracle_requires_ordered_failure_and_parser_matrix() {
    let parser_matrix = delivered_with_payload(
        2,
        "session-001:provider-mutation:001",
        "session-001",
        BoundaryKind::ProviderMutation,
        json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ProviderScriptMutation, 2)}),
        provider_mutation_observed("malformed_sse_chunk"),
    );
    let provider = delivered_with_payload(
        1,
        "session-001:provider:001",
        "session-001",
        BoundaryKind::Provider,
        json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ProviderTurnCompletion, 1)}),
        json!({"provider_output": "answer"}),
    );
    let failure_equal_sequence = delivered_with_payload(
        1,
        "session-001:backend-failure:001",
        "session-001",
        BoundaryKind::BackendFailure,
        json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::BackendRetryOrFailure, 1)}),
        json!({"backend_failure": true}),
    );
    let verdict = mini_standard_provider_error_without_checkpoint(&[
        provider.clone(),
        failure_equal_sequence,
        parser_matrix.clone(),
    ]);
    assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);
    assert_eq!(
        verdict.oracle_id,
        SCENARIO_MINI_STANDARD_PROVIDER_ERROR_ORACLE
    );

    let missing_runtime_completion = delivered_with_payload(
        0,
        "session-001:backend-failure:001",
        "session-001",
        BoundaryKind::BackendFailure,
        json!({}),
        json!({"backend_failure": true}),
    );
    let verdict = mini_standard_provider_error_without_checkpoint(&[
        missing_runtime_completion,
        provider.clone(),
        parser_matrix.clone(),
    ]);
    assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);

    let wrong_kind_before_provider = delivered_with_payload(
        0,
        "session-001:tool:001",
        "session-001",
        BoundaryKind::Tool,
        json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ToolReturn, 0)}),
        json!({"tool_output": "not a provider failure"}),
    );
    let verdict = mini_standard_provider_error_without_checkpoint(&[
        wrong_kind_before_provider,
        provider.clone(),
        parser_matrix.clone(),
    ]);
    assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);

    let ordered_failure = delivered_with_payload(
        0,
        "session-001:backend-failure:001",
        "session-001",
        BoundaryKind::BackendFailure,
        json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::BackendRetryOrFailure, 0)}),
        json!({"backend_failure": true}),
    );
    let verdict =
        mini_standard_provider_error_without_checkpoint(&[ordered_failure, provider.clone()]);
    assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);

    let provider_after_failure = delivered_with_payload(
        2,
        "session-001:provider:002",
        "session-001",
        BoundaryKind::Provider,
        json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ProviderTurnCompletion, 2)}),
        json!({"provider_output": "answer"}),
    );
    let late_failure = delivered_with_payload(
        3,
        "session-001:backend-failure:002",
        "session-001",
        BoundaryKind::BackendFailure,
        json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::BackendRetryOrFailure, 3)}),
        json!({"backend_failure": true}),
    );
    let verdict = mini_standard_provider_error_without_checkpoint(&[
        provider_after_failure,
        late_failure,
        parser_matrix.clone(),
    ]);
    assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);

    let passing_failure = delivered_with_payload(
        0,
        "session-001:backend-failure:003",
        "session-001",
        BoundaryKind::BackendFailure,
        json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::BackendRetryOrFailure, 0)}),
        json!({"backend_failure": true}),
    );
    let verdict = mini_standard_provider_error_without_checkpoint(&[
        passing_failure,
        provider,
        parser_matrix,
    ]);
    assert_eq!(verdict.status, crate::trace::OracleStatus::Passed);
}

#[test]
fn rlm_mini_oracle_rejects_exec_without_runtime_effect_outcome() {
    let events = vec![
        delivered_with_payload(
            0,
            "session-001:exec:001",
            "session-001",
            BoundaryKind::ExecCode,
            json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ExecResult, 0)}),
            json!({
                "exec_output": "cell ran",
                "execution_count": 1
            }),
        ),
        delivered_with_payload(
            1,
            "session-001:provider:001",
            "session-001",
            BoundaryKind::Provider,
            json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ProviderTurnCompletion, 1)}),
            json!({"provider_output": "continued"}),
        ),
    ];

    let verdict = mini_rlm_lashlang_cell_exec_continues(&events);

    assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);
    assert_eq!(verdict.oracle_id, SCENARIO_MINI_RLM_CELL_EXEC_ORACLE);
    assert!(verdict.message.contains("did not continue after exec"));
}

#[test]
fn rlm_mini_oracle_requires_provider_after_same_actor_exec() {
    let exec = delivered_with_payload(
        1,
        "session-001:exec:001",
        "session-001",
        BoundaryKind::ExecCode,
        json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ExecResult, 1)}),
        json!({
            "exec_output": "cell ran",
            "runtime_effect_outcome": {"type": "exec_code"},
            "execution_count": 1
        }),
    );
    for (name, event) in [
        (
            "same sequence provider",
            delivered_with_payload(
                1,
                "session-001:provider:001",
                "session-001",
                BoundaryKind::Provider,
                json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ProviderTurnCompletion, 1)}),
                json!({"provider_output": "continued"}),
            ),
        ),
        (
            "different actor provider",
            delivered_with_payload(
                2,
                "session-002:provider:001",
                "session-002",
                BoundaryKind::Provider,
                json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ProviderTurnCompletion, 2)}),
                json!({"provider_output": "continued"}),
            ),
        ),
        (
            "non-provider event",
            delivered_with_payload(
                2,
                "session-001:tool:001",
                "session-001",
                BoundaryKind::Tool,
                json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ToolReturn, 2)}),
                json!({"tool_output": "continued"}),
            ),
        ),
    ] {
        let verdict = mini_rlm_lashlang_cell_exec_continues(&[exec.clone(), event]);
        assert_eq!(verdict.status, crate::trace::OracleStatus::Failed, "{name}");
        assert_eq!(verdict.oracle_id, SCENARIO_MINI_RLM_CELL_EXEC_ORACLE);
    }

    let continued = delivered_with_payload(
        2,
        "session-001:provider:001",
        "session-001",
        BoundaryKind::Provider,
        json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ProviderTurnCompletion, 2)}),
        json!({"provider_output": "continued"}),
    );
    let verdict = mini_rlm_lashlang_cell_exec_continues(&[exec, continued]);
    assert_eq!(verdict.status, crate::trace::OracleStatus::Passed);
}

#[test]
fn agent_mini_oracle_rejects_provider_completions_without_runtime_sessions() {
    let summary = AbstractWorldSummary::with_digest(2, 2, Vec::new(), Vec::new());
    let provider = |sequence, session: &str, observed: serde_json::Value| {
        delivered_with_payload(
            sequence,
            &format!("{session}:provider:001"),
            session,
            BoundaryKind::Provider,
            json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ProviderTurnCompletion, sequence as u64)}),
            observed,
        )
    };
    let joined = [
        provider(
            0,
            "session-001",
            json!({"runtime_session_id": "session-001"}),
        ),
        provider(
            1,
            "session-002",
            json!({"runtime_session_id": "session-002"}),
        ),
    ];
    assert_eq!(
        mini_agent_parallel_spawn_join(&joined, &summary).status,
        crate::trace::OracleStatus::Passed
    );

    let unattributed = [
        provider(0, "session-001", json!({})),
        provider(1, "session-002", json!({})),
    ];
    let verdict = mini_agent_parallel_spawn_join(&unattributed, &summary);
    assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);
    assert_eq!(verdict.oracle_id, SCENARIO_MINI_AGENT_PARALLEL_JOIN_ORACLE);
    assert!(
        verdict
            .message
            .contains("did not record provider completions across runtime sessions")
    );

    let one_session = [
        provider(
            0,
            "session-001",
            json!({"runtime_session_id": "session-001"}),
        ),
        provider(
            1,
            "session-001",
            json!({"runtime_session_id": "session-001"}),
        ),
    ];
    assert_eq!(
        mini_agent_parallel_spawn_join(&one_session, &summary).status,
        crate::trace::OracleStatus::Failed
    );
}

#[test]
fn agent_durable_input_mini_oracle_requires_all_resolution_evidence() {
    let served = json!({
        "replayed": true,
        "redrive_served_recorded_result": true,
        "execution_count": 1,
        "replay_count": 1,
        "runtime_effect": {
            "local_executor_called": true,
            "redrive_local_executor_called": false,
        },
    });
    let durable = delivered_with_payload(
        0,
        "session-001:durable:001",
        "session-001",
        BoundaryKind::DurableEffect,
        json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::DurableEffectCompletion, 0)}),
        served.clone(),
    );
    let observer = delivered_with_payload(
        2,
        "session-001:observer:reconnect:001",
        "session-001",
        BoundaryKind::Observer,
        json!({}),
        json!({"reconnected": true}),
    );
    let mut re_executed = served.clone();
    re_executed["execution_count"] = json!(2);
    re_executed["runtime_effect"]["redrive_local_executor_called"] = json!(true);

    for (name, events) in [
        ("missing durable", vec![observer.clone()]),
        ("missing observer", vec![durable.clone()]),
        (
            "wrong durable kind",
            vec![
                delivered_with_payload(
                    0,
                    "session-001:tool:001",
                    "session-001",
                    BoundaryKind::Tool,
                    json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ToolReturn, 0)}),
                    served.clone(),
                ),
                observer.clone(),
            ],
        ),
        (
            "redrive re-executed the effect",
            vec![
                delivered_with_payload(
                    0,
                    "session-001:durable:001",
                    "session-001",
                    BoundaryKind::DurableEffect,
                    json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::DurableEffectCompletion, 0)}),
                    re_executed,
                ),
                observer.clone(),
            ],
        ),
    ] {
        let verdict = mini_agent_durable_input_resolution(&events);
        assert_eq!(verdict.status, crate::trace::OracleStatus::Failed, "{name}");
        assert_eq!(verdict.oracle_id, SCENARIO_MINI_AGENT_DURABLE_INPUT_ORACLE);
    }
    let verdict = mini_agent_durable_input_resolution(&[durable, observer]);
    assert_eq!(verdict.status, crate::trace::OracleStatus::Passed);
}

#[test]
fn scheduler_owned_runtime_completion_oracle_fails_on_declared_count_shortfall() {
    let mut events = Vec::new();
    for (seq, kind) in SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE_KINDS
        .iter()
        .enumerate()
    {
        events.push(delivered_with_payload(
            seq,
            &format!("boundary:{seq:03}"),
            "session-001",
            *kind,
            json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ProviderTurnCompletion, seq as u64)}),
            json!({}),
        ));
    }
    // One declared Provider completion was shrunk away: observed 1, declared 2.
    // Every other kind still meets its declared count.
    let expectations = SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE_KINDS
        .iter()
        .map(|&kind| {
            (
                kind,
                if kind == BoundaryKind::Provider {
                    2usize
                } else {
                    1
                },
            )
        })
        .collect();
    let expectations = WorkloadExpectations::default().with_completion_counts(expectations);
    let verdict = scheduler_owned_runtime_completions(&events, &expectations);
    assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);
    assert_eq!(verdict.oracle_id, SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE);
    for fragment in ["Provider", "1", "2", "declared"] {
        assert!(
            verdict.message.contains(fragment),
            "under-delivery message must name the kind and both counts; got: {}",
            verdict.message
        );
    }
}

#[test]
fn scheduler_owned_runtime_completion_oracle_keeps_presence_floor_when_undeclared() {
    // A declaration for other kinds must not exempt an undeclared kind.
    let expectations = WorkloadExpectations::default()
        .with_completion_counts(BTreeMap::from([(BoundaryKind::Provider, 1)]));
    let events = vec![delivered_with_payload(
        0,
        "session-001:provider:001",
        "session-001",
        BoundaryKind::Provider,
        json!({"runtime_completion": runtime_completion(RuntimeCompletionFamily::ProviderTurnCompletion, 0)}),
        json!({}),
    )];
    let verdict = scheduler_owned_runtime_completions(&events, &expectations);
    assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);
    assert!(
        verdict
            .message
            .contains("did not include scheduler-owned runtime completion kinds"),
        "undeclared kinds must still hit the presence floor, got: {}",
        verdict.message
    );
}
