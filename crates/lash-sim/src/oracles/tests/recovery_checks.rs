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
fn worker_stale_completion_oracle_requires_real_fencing() {
    // Negative: the real store did NOT fence (no incarnation change, no stale
    // rejection, fence not advanced) -> the oracle must FAIL. With the
    // fabrication deleted, this is the ONLY way the summary can look.
    let not_fenced = AbstractWorldSummary::with_digest(
        1,
        1,
        vec![],
        vec![],
        vec![WorkerAbstractSummary {
            worker_alias: "worker-001".to_string(),
            session_alias: "session-001".to_string(),
            active_incarnation_id: String::new(),
            active_fencing_token: 1,
            lease_owner_changes: 0,
            stale_completion_rejections: 0,
            process_stale_completion_rejected: false,
            process_stale_output_absent: false,
            process_terminal_writer: String::new(),
            process_terminal_event_count: 0,
        }],
    );
    assert!(!worker_stale_completion_rejected(&not_fenced).is_passed());

    // Positive: the real store fenced (incarnation change, stale rejection,
    // monotonic fence advance) -> the oracle passes.
    let fenced = AbstractWorldSummary::with_digest(
        1,
        1,
        vec![],
        vec![],
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
    );
    assert!(worker_stale_completion_rejected(&fenced).is_passed());

    let mut stale_output_present = fenced.clone();
    stale_output_present.workers[0].process_stale_output_absent = false;
    assert!(
        !worker_stale_completion_rejected(&stale_output_present).is_passed(),
        "the oracle must reject a trace where stale semantic output persisted"
    );
    let mut duplicate_terminal = fenced.clone();
    duplicate_terminal.workers[0].process_terminal_event_count = 2;
    assert!(
        !worker_stale_completion_rejected(&duplicate_terminal).is_passed(),
        "the oracle must reject duplicate successor terminals"
    );
    let mut stale_writer_won = fenced;
    stale_writer_won.workers[0].process_terminal_writer = "stale".to_string();
    assert!(
        !worker_stale_completion_rejected(&stale_writer_won).is_passed(),
        "the oracle must inspect the semantic terminal writer"
    );
}

#[test]
fn deliberate_expiry_oracles_remain_failure_capable() {
    let lease_event = |sequence, token| {
        delivered_with_payload(
            sequence,
            &format!("session-001:lease-time:{sequence:03}"),
            "session-001",
            BoundaryKind::LeaseTime,
            json!({"tick": sequence * 30_000}),
            json!({
                "runtime_lease_probe": {
                    "session_execution_lease_fencing_token": token,
                    "real_lease_store": true,
                }
            }),
        )
    };
    let two_lease_boundaries = WorkloadExpectations::new(vec!["session-001".to_string()], 0, 0, 2);
    assert!(
        lease_time_monotonic(
            &[lease_event(0, 1), lease_event(1, 2)],
            &two_lease_boundaries
        )
        .is_passed()
    );
    assert!(
        !lease_time_monotonic(
            &[lease_event(0, 1), lease_event(1, 1)],
            &two_lease_boundaries
        )
        .is_passed(),
        "lease-time-monotonic must fire when a real store reissues a fence"
    );

    let worker_event = delivered_with_payload(
        0,
        "worker-001:worker:001",
        "worker-001",
        BoundaryKind::Worker,
        json!({"session": "session-001"}),
        json!({
            "stale_completion_rejected": true,
            "runtime_active_lease": {"fencing_token": 2},
            "runtime_stale_completion": {"fencing_token": 1},
        }),
    );
    let fenced = AbstractWorldSummary::with_digest(
        1,
        1,
        vec![],
        vec![],
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
    );
    assert!(
        mini_runtime_stale_lease_commit_rejected(std::slice::from_ref(&worker_event), &fenced)
            .is_passed()
    );

    let mut unfenced = worker_event;
    unfenced.observed["stale_completion_rejected"] = json!(false);
    assert!(
        !mini_runtime_stale_lease_commit_rejected(&[unfenced], &fenced).is_passed(),
        "stale-lease-commit-rejected must fire when the stale writer is not rejected"
    );
}

#[test]
fn replay_determinism_ignores_only_opaque_fence_values() {
    let summary = |fence, stale_rejections| {
        AbstractWorldSummary::with_digest(
            1,
            1,
            vec![],
            vec![],
            vec![WorkerAbstractSummary {
                worker_alias: "worker-001".to_string(),
                session_alias: "session-001".to_string(),
                active_incarnation_id: "worker-001:incarnation-002".to_string(),
                active_fencing_token: fence,
                lease_owner_changes: 1,
                stale_completion_rejections: stale_rejections,
                process_stale_completion_rejected: true,
                process_stale_output_absent: true,
                process_terminal_writer: "successor".to_string(),
                process_terminal_event_count: 1,
            }],
        )
    };

    assert!(replay_determinism(&summary(12, 1), &summary(13, 1)).is_passed());
    assert!(!replay_determinism(&summary(12, 1), &summary(13, 0)).is_passed());

    let expected = summary(12, 1);
    let mut mutated = summary(13, 1);
    mutated.workers[0].process_stale_completion_rejected = false;
    assert!(!replay_determinism(&expected, &mutated).is_passed());
    let mut mutated = summary(13, 1);
    mutated.workers[0].process_stale_output_absent = false;
    assert!(!replay_determinism(&expected, &mutated).is_passed());
    let mut mutated = summary(13, 1);
    mutated.workers[0].process_terminal_writer = "stale".to_string();
    assert!(!replay_determinism(&expected, &mutated).is_passed());
    let mut mutated = summary(13, 1);
    mutated.workers[0].process_terminal_event_count = 2;
    assert!(!replay_determinism(&expected, &mutated).is_passed());
}

#[test]
fn worker_failover_continuation_oracle_requires_successor_commit() {
    let worker_event = |work: serde_json::Value| {
        delivered_with_payload(
            0,
            "worker-001:worker:001",
            "worker-001",
            BoundaryKind::Worker,
            json!({ "session": "session-001" }),
            json!({
                "expired_owner_commit_rejected": true,
                "runtime_worker_store": {
                    "takeover_after_ttl_expiry": true,
                    "worker_owned_work": work,
                    "process_completion": {
                        "stale_completion_rejected": true,
                        "stale_output_absent": true,
                        "terminal_writer": "successor",
                        "terminal_event_count": 1,
                    }
                }
            }),
        )
    };
    let full = json!({
        "first_owner_claimed_work": true,
        "second_owner_resumed_work": true,
        "second_owner_outranks_first": true,
        "stale_work_completion_rejected": true,
    });

    // Positive: a successor reclaimed and continued the work.
    assert!(worker_failover_continues_work(&[worker_event(full.clone())]).is_passed());

    // Negative: no worker boundary at all.
    assert!(!worker_failover_continues_work(&[]).is_passed());

    // Negative: the successor did not resume the dead owner's work.
    let mut not_resumed = full.clone();
    not_resumed["second_owner_resumed_work"] = json!(false);
    assert!(!worker_failover_continues_work(&[worker_event(not_resumed)]).is_passed());

    // Negative: the dead owner's stale completion was NOT rejected.
    let mut stale_not_rejected = full.clone();
    stale_not_rejected["stale_work_completion_rejected"] = json!(false);
    assert!(!worker_failover_continues_work(&[worker_event(stale_not_rejected)]).is_passed());

    // Negative: the successor did not outrank the first owner's fence.
    let mut not_outranked = full;
    not_outranked["second_owner_outranks_first"] = json!(false);
    assert!(!worker_failover_continues_work(&[worker_event(not_outranked)]).is_passed());
}

#[test]
fn process_lifecycle_recovery_oracles_verify_disposition_and_evidence() {
    let lifecycle = |processes: serde_json::Value| {
        delivered_with_payload(
            0,
            "session-001:process-lifecycle:001",
            "session-001",
            BoundaryKind::ProcessLifecycle,
            json!({ "session": "session-001" }),
            json!({
                "runtime_process_lifecycle": {
                    "sweep_driven": true,
                    "processes": processes,
                }
            }),
        )
    };
    // The correct disposition-driven recovery outcome.
    let ob_waiting = json!({
        "process_id": "ob-crashed", "disposition": "owner_bound", "started": true,
        "terminal_status": "running", "reran": false,
        "lease_lapsed": false, "abandon_requested": false,
    });
    let rerun = json!({
        "process_id": "rerun-crashed", "disposition": "rerunnable", "started": true,
        "terminal_status": "failed", "reran": true,
        "lease_lapsed": true, "abandon_requested": false,
    });
    let ob_reconciled = json!({
        "process_id": "ob-abandon-req", "disposition": "owner_bound", "started": true,
        "terminal_status": "abandoned", "reran": false, "abandon_writer": "reconciled_request",
        "abandon_evidence_owner": "sim-silent-owner",
        "lease_lapsed": true, "abandon_requested": true,
    });
    let full = vec![lifecycle(json!([ob_waiting, rerun, ob_reconciled]))];
    assert!(process_never_double_started(&full).is_passed());
    assert!(abandoned_requires_evidence(&full).is_passed());
    assert!(started_owner_bound_recovery_is_safe(&full));
    // Vacuous absence tolerates the new terminal.
    assert!(started_owner_bound_recovery_is_safe(&[]));

    // Negative: a started OwnerBound row reached a run terminal (double-start).
    let double_started = json!({
        "process_id": "ob-crashed", "disposition": "owner_bound", "started": true,
        "terminal_status": "failed", "reran": true,
    });
    let events = vec![lifecycle(json!([double_started, rerun.clone()]))];
    assert!(
        !process_never_double_started(&events).is_passed(),
        "a re-run started OwnerBound row must fail the double-start oracle"
    );
    assert!(!started_owner_bound_recovery_is_safe(&events));

    // Negative: no Rerunnable sibling re-run — refuses to pass on presence
    // alone.
    let events = vec![lifecycle(json!([ob_waiting.clone()]))];
    assert!(
        !process_never_double_started(&events).is_passed(),
        "an OwnerBound-only outcome without a re-run contrast must not pass vacuously"
    );

    // Negative: no lifecycle boundary at all — nothing recovered to verify.
    assert!(!process_never_double_started(&[]).is_passed());
    assert!(!abandoned_requires_evidence(&[]).is_passed());

    // Negative: a reconciled request whose lease had not lapsed.
    let mut reconciled_live = ob_reconciled.clone();
    reconciled_live["lease_lapsed"] = json!(false);
    let events = vec![lifecycle(json!([reconciled_live, rerun.clone()]))];
    assert!(
        !abandoned_requires_evidence(&events).is_passed(),
        "a reconciled request with a live lease must fail the evidence oracle"
    );

    // Negative: an Abandoned terminal with no writer — elapsed time alone.
    let no_writer = json!({
        "process_id": "ob-x", "disposition": "owner_bound", "started": true,
        "terminal_status": "abandoned", "reran": false,
        "lease_lapsed": true, "abandon_requested": false,
    });
    let events = vec![lifecycle(json!([no_writer]))];
    assert!(
        !abandoned_requires_evidence(&events).is_passed(),
        "an Abandoned terminal with no writer is elapsed-time-alone and must fail"
    );
}

#[test]
fn scheduler_owned_runtime_completion_oracle_rejects_missing_pending_evidence() {
    let verdict = scheduler_owned_runtime_completions(&[delivered_with_payload(
        0,
        "session-001:provider:001",
        "session-001",
        BoundaryKind::Provider,
        json!({}),
        json!({"provider_output": "answer"}),
    )]);

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
        let verdict = scheduler_owned_runtime_completions(&[delivered_with_payload(
            0,
            "session-001:provider:001",
            "session-001",
            BoundaryKind::Provider,
            json!({"runtime_completion": completion}),
            json!({"provider_output": "answer"}),
        )]);

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
        BoundaryKind::ProcessWake,
        BoundaryKind::ProcessLifecycle,
        BoundaryKind::Worker,
        BoundaryKind::Observer,
        BoundaryKind::Cancellation,
        BoundaryKind::Trigger,
        BoundaryKind::BackendFailure,
        BoundaryKind::ProviderMutation,
        BoundaryKind::LeaseTime,
    ];
    assert_eq!(all_kinds.len(), 16, "must test all sixteen boundary kinds");
    for kind in all_kinds {
        let serialized = serde_json::to_string(&kind).expect("serialization failed");
        let expected_name = serialized.trim_matches('"');
        assert_eq!(
            kind.name(),
            expected_name,
            "BoundaryKind::name() for {kind:?} must match its serde-serialized snake_case string"
        );
    }
}

#[test]
fn scheduler_owned_runtime_completion_oracle_rejects_missing_evidence_for_process_wake_and_observer()
 {
    for kind in [BoundaryKind::ProcessWake, BoundaryKind::Observer] {
        let verdict = scheduler_owned_runtime_completions(&[delivered_with_payload(
            0,
            "session-001:boundary:001",
            "session-001",
            kind,
            json!({}),
            json!({}),
        )]);

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
fn scheduler_owned_runtime_completion_oracle_passes_with_all_ten_kinds_present() {
    let events = vec![
        delivered_with_payload(
            0,
            "session-001:provider:001",
            "session-001",
            BoundaryKind::Provider,
            json!({"runtime_completion": runtime_completion("provider_turn_completion", 0)}),
            json!({}),
        ),
        delivered_with_payload(
            1,
            "session-001:cancellation:001",
            "session-001",
            BoundaryKind::Cancellation,
            json!({"runtime_completion": runtime_completion("queued_input_cancellation", 1)}),
            json!({}),
        ),
        delivered_with_payload(
            2,
            "session-001:backend-failure:001",
            "session-001",
            BoundaryKind::BackendFailure,
            json!({"runtime_completion": runtime_completion("backend_retry_or_failure", 2)}),
            json!({}),
        ),
        delivered_with_payload(
            3,
            "session-001:provider-mutation:001",
            "session-001",
            BoundaryKind::ProviderMutation,
            json!({"runtime_completion": runtime_completion("provider_script_mutation", 3)}),
            json!({}),
        ),
        delivered_with_payload(
            4,
            "session-001:tool:001",
            "session-001",
            BoundaryKind::Tool,
            json!({"runtime_completion": runtime_completion("tool_return", 4)}),
            json!({}),
        ),
        delivered_with_payload(
            5,
            "session-001:exec-code:001",
            "session-001",
            BoundaryKind::ExecCode,
            json!({"runtime_completion": runtime_completion("exec_result", 5)}),
            json!({}),
        ),
        delivered_with_payload(
            6,
            "session-001:durable:001",
            "session-001",
            BoundaryKind::DurableEffect,
            json!({"runtime_completion": runtime_completion("durable_effect_completion", 6)}),
            json!({}),
        ),
        delivered_with_payload(
            7,
            "worker-001:worker:001",
            "worker-001",
            BoundaryKind::Worker,
            json!({"runtime_completion": runtime_completion("worker_lease_completion", 7)}),
            json!({}),
        ),
        delivered_with_payload(
            8,
            "session-001:process-wake:001",
            "session-001",
            BoundaryKind::ProcessWake,
            json!({"runtime_completion": runtime_completion("process_wake", 8)}),
            json!({}),
        ),
        delivered_with_payload(
            9,
            "session-001:observer:001",
            "session-001",
            BoundaryKind::Observer,
            json!({"runtime_completion": runtime_completion("observer_snapshot", 9)}),
            json!({}),
        ),
    ];

    let verdict = scheduler_owned_runtime_completions(&events);
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
                json!({"runtime_completion": runtime_completion("some_family", seq)}),
                json!({}),
            ));
            seq += 1;
        }
        let verdict = scheduler_owned_runtime_completions(&events);
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
        json!({"runtime_completion": runtime_completion("provider_script_mutation", 2)}),
        provider_mutation_observed("malformed_sse_chunk"),
    );
    let provider = delivered_with_payload(
        1,
        "session-001:provider:001",
        "session-001",
        BoundaryKind::Provider,
        json!({"runtime_completion": runtime_completion("provider_turn_completion", 1)}),
        json!({"provider_output": "answer"}),
    );
    let failure_equal_sequence = delivered_with_payload(
        1,
        "session-001:backend-failure:001",
        "session-001",
        BoundaryKind::BackendFailure,
        json!({"runtime_completion": runtime_completion("backend_retry_or_failure", 1)}),
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
        json!({"runtime_completion": runtime_completion("tool_return", 0)}),
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
        json!({"runtime_completion": runtime_completion("backend_retry_or_failure", 0)}),
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
        json!({"runtime_completion": runtime_completion("provider_turn_completion", 2)}),
        json!({"provider_output": "answer"}),
    );
    let late_failure = delivered_with_payload(
        3,
        "session-001:backend-failure:002",
        "session-001",
        BoundaryKind::BackendFailure,
        json!({"runtime_completion": runtime_completion("backend_retry_or_failure", 3)}),
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
        json!({"runtime_completion": runtime_completion("backend_retry_or_failure", 0)}),
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
            json!({"runtime_completion": runtime_completion("exec_result", 0)}),
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
            json!({"runtime_completion": runtime_completion("provider_turn_completion", 1)}),
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
        json!({"runtime_completion": runtime_completion("exec_result", 1)}),
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
                json!({"runtime_completion": runtime_completion("provider_turn_completion", 1)}),
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
                json!({"runtime_completion": runtime_completion("provider_turn_completion", 2)}),
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
                json!({"runtime_completion": runtime_completion("tool_return", 2)}),
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
        json!({"runtime_completion": runtime_completion("provider_turn_completion", 2)}),
        json!({"provider_output": "continued"}),
    );
    let verdict = mini_rlm_lashlang_cell_exec_continues(&[exec, continued]);
    assert_eq!(verdict.status, crate::trace::OracleStatus::Passed);
}

#[test]
fn agent_mini_oracle_rejects_process_wake_without_join_session() {
    let summary = AbstractWorldSummary::with_digest(2, 2, Vec::new(), Vec::new(), Vec::new());
    let events = vec![
        delivered_with_payload(
            0,
            "session-001:process-wake:001",
            "session-001",
            BoundaryKind::ProcessWake,
            json!({"runtime_completion": runtime_completion("process_wake", 0)}),
            json!({
                "process_wake": true,
                "runtime_process_wake": {
                    "event_invocation": {
                        "subject": {
                            "process_id": "process-001"
                        }
                    }
                }
            }),
        ),
        delivered_with_payload(
            1,
            "worker-001:stale-completion",
            "worker-001",
            BoundaryKind::Worker,
            json!({"runtime_completion": runtime_completion("worker_lease_completion", 1)}),
            json!({"session": "session-001"}),
        ),
    ];

    let verdict = mini_agent_parallel_spawn_join(&events, &summary);

    assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);
    assert_eq!(verdict.oracle_id, SCENARIO_MINI_AGENT_PARALLEL_JOIN_ORACLE);
    assert!(
        verdict
            .message
            .contains("did not record deterministic process/worker ordering")
    );
}

#[test]
fn agent_durable_input_mini_oracle_requires_all_resolution_evidence() {
    let summary = AbstractWorldSummary::with_digest(2, 3, Vec::new(), Vec::new(), Vec::new());
    let durable = delivered_with_payload(
        0,
        "session-001:durable:001:replay",
        "session-001",
        BoundaryKind::DurableEffect,
        json!({"runtime_completion": runtime_completion("durable_effect_completion", 0)}),
        json!({"replayed": true, "runtime_effect": {}}),
    );
    let process_wake = delivered_with_payload(
        1,
        "session-001:process-wake:001",
        "session-001",
        BoundaryKind::ProcessWake,
        json!({"runtime_completion": runtime_completion("process_wake", 1)}),
        json!({
            "session": "session-001",
            "runtime_process_wake": {
                "event_invocation": {
                    "subject": {
                        "process_id": "process-001"
                    }
                }
            }
        }),
    );
    let observer = delivered_with_payload(
        2,
        "session-001:observer:reconnect:001",
        "session-001",
        BoundaryKind::Observer,
        json!({}),
        json!({"reconnected": true}),
    );

    for (name, events) in [
        (
            "missing durable",
            vec![process_wake.clone(), observer.clone()],
        ),
        (
            "missing process wake",
            vec![durable.clone(), observer.clone()],
        ),
        (
            "missing observer",
            vec![durable.clone(), process_wake.clone()],
        ),
        (
            "wrong durable kind",
            vec![
                delivered_with_payload(
                    0,
                    "session-001:tool:001",
                    "session-001",
                    BoundaryKind::Tool,
                    json!({"runtime_completion": runtime_completion("tool_return", 0)}),
                    json!({"replayed": true, "runtime_effect": {}}),
                ),
                process_wake.clone(),
                observer.clone(),
            ],
        ),
        (
            "durable not replayed",
            vec![
                delivered_with_payload(
                    0,
                    "session-001:durable:001:first",
                    "session-001",
                    BoundaryKind::DurableEffect,
                    json!({"runtime_completion": runtime_completion("durable_effect_completion", 0)}),
                    json!({"replayed": false, "runtime_effect": {}}),
                ),
                process_wake.clone(),
                observer.clone(),
            ],
        ),
    ] {
        let verdict = mini_agent_durable_input_resolution(&events);
        assert_eq!(verdict.status, crate::trace::OracleStatus::Failed, "{name}");
        assert_eq!(verdict.oracle_id, SCENARIO_MINI_AGENT_DURABLE_INPUT_ORACLE);
    }
    let verdict = mini_agent_durable_input_resolution(&[durable, process_wake, observer]);
    assert_eq!(verdict.status, crate::trace::OracleStatus::Passed);

    let parallel = mini_agent_parallel_spawn_join(
        &[
            delivered_with_payload(
                3,
                "session-001:process-wake:002",
                "session-001",
                BoundaryKind::ProcessWake,
                json!({"runtime_completion": runtime_completion("process_wake", 3)}),
                json!({"session": "session-001"}),
            ),
            delivered_with_payload(
                4,
                "worker-001:lease:002",
                "worker-001",
                BoundaryKind::Worker,
                json!({"runtime_completion": runtime_completion("worker_lease_completion", 4)}),
                json!({"session": "session-001"}),
            ),
        ],
        &summary,
    );
    assert_eq!(
        parallel.status,
        crate::trace::OracleStatus::Passed,
        "non-empty process wake session should satisfy join evidence"
    );

    let reversed = mini_agent_parallel_spawn_join(
        &[
            delivered_with_payload(
                6,
                "session-001:process-wake:003",
                "session-001",
                BoundaryKind::ProcessWake,
                json!({"runtime_completion": runtime_completion("process_wake", 6)}),
                json!({"session": "session-001"}),
            ),
            delivered_with_payload(
                5,
                "worker-001:lease:003",
                "worker-001",
                BoundaryKind::Worker,
                json!({"runtime_completion": runtime_completion("worker_lease_completion", 5)}),
                json!({"session": "session-001"}),
            ),
        ],
        &summary,
    );
    assert_eq!(reversed.status, crate::trace::OracleStatus::Failed);

    let duplicate_sequence = mini_agent_parallel_spawn_join(
        &[
            delivered_with_payload(
                7,
                "session-001:process-wake:004",
                "session-001",
                BoundaryKind::ProcessWake,
                json!({"runtime_completion": runtime_completion("process_wake", 7)}),
                json!({"session": "session-001"}),
            ),
            delivered_with_payload(
                7,
                "worker-001:lease:004",
                "worker-001",
                BoundaryKind::Worker,
                json!({"runtime_completion": runtime_completion("worker_lease_completion", 7)}),
                json!({"session": "session-001"}),
            ),
        ],
        &summary,
    );
    assert_eq!(
        duplicate_sequence.status,
        crate::trace::OracleStatus::Failed
    );
}
