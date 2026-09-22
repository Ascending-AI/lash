//! Model-call ledger validation tests, split out of `tests.rs` so both files
//! stay inside the repository test-file size budget (FIG-2985). No test is
//! renamed, removed or changed here.

use super::*;

#[test]
fn model_call_records_are_validated_from_result_and_activity_envelopes() {
    let valid_record = RemoteLlmCallRecord {
        call_id: "llm-call".to_string(),
        label: None,
        replay_drops: Vec::new(),
        attempts: vec![RemoteAttemptRecord {
            ordinal: 1,
            started_at_ms: 0,
            duration_ms: 0,
            outcome: RemoteAttemptOutcome::Completed,
            protocol_position: RemoteProtocolPosition::TerminalObserved,
            retry_budget_consumed: false,
            retry_decision: None,
            error: None,
            evidence: None,
            generation_disposition: None,
            usage: None,
            usage_disposition: Default::default(),
        }],
    };
    let mut activity = RemoteTurnActivity {
        sequence: 1,
        id: "event".to_string(),
        correlation_id: "correlation".to_string(),
        event: RemoteTurnEvent::ModelCallRecorded {
            record: valid_record.clone(),
        },
    };
    activity.validate().expect("valid model-call activity");
    let RemoteTurnEvent::ModelCallRecorded { record } = &mut activity.event else {
        unreachable!("constructed model-call activity")
    };
    record.call_id.clear();
    assert!(activity.validate().is_err());

    let mut result = RemoteTurnReport {
        session_id: SessionId::from("session"),
        turn_id: TurnId::from("turn"),
        outcome: RemoteTurnOutcome::Finished {
            finish: RemoteTurnFinish::AssistantMessage {
                text: "done".to_string(),
            },
        },
        assistant_output: RemoteAssistantOutput::default(),
        usage: RemoteTurnUsageReport::default(),
        execution: RemoteTurnExecutionMetrics::default(),
        tool_calls: Vec::new(),
        llm_calls: vec![valid_record.clone()],
        issues: Vec::new(),
        activities: vec![RemoteTurnActivity {
            sequence: 1,
            id: "model-call".to_string(),
            correlation_id: "llm-call".to_string(),
            event: RemoteTurnEvent::ModelCallRecorded {
                record: valid_record.clone(),
            },
        }],
        metadata: HashMap::new(),
    };
    result.validate().expect("valid model-call result");
    result.llm_calls[0].attempts.clear();
    assert!(result.validate().is_err());
    result.llm_calls[0] = valid_record.clone();
    result.llm_calls[0].attempts[0].ordinal = 0;
    assert!(result.validate().is_err());
    result.llm_calls[0] = valid_record;
    result.llm_calls[0].attempts[0].error = Some(RemoteNormalizedError {
        class: String::new(),
        code: None,
        http_status: None,
        provider_request_id: None,
        retry_after_ms: None,
    });
    assert!(result.validate().is_err());
}

#[test]
fn turn_result_rejects_conflicting_summary_and_activity_for_the_same_model_call() {
    let summary = RemoteLlmCallRecord {
        call_id: "same-call".to_string(),
        label: Some("foreground".to_string()),
        replay_drops: Vec::new(),
        attempts: vec![RemoteAttemptRecord {
            ordinal: 1,
            started_at_ms: 7,
            duration_ms: 9,
            outcome: RemoteAttemptOutcome::Completed,
            protocol_position: RemoteProtocolPosition::TerminalObserved,
            retry_budget_consumed: true,
            retry_decision: None,
            error: None,
            evidence: Some(RemoteExecutionEvidence {
                served_model: Some("served-model".to_string()),
                provider_response_id: Some("provider-response".to_string()),
                ..RemoteExecutionEvidence::default()
            }),
            generation_disposition: None,
            usage: None,
            usage_disposition: Default::default(),
        }],
    };
    let activity_record = RemoteLlmCallRecord {
        attempts: vec![RemoteAttemptRecord {
            ordinal: 1,
            started_at_ms: 7,
            duration_ms: 9,
            outcome: RemoteAttemptOutcome::Failed,
            protocol_position: RemoteProtocolPosition::NoResponse,
            retry_budget_consumed: true,
            retry_decision: None,
            error: Some(RemoteNormalizedError {
                class: "transport".to_string(),
                code: Some(lash_sansio::FailureCode::provider("connection_failed")),
                http_status: None,
                provider_request_id: None,
                retry_after_ms: None,
            }),
            evidence: None,
            generation_disposition: None,
            usage: None,
            usage_disposition: Default::default(),
        }],
        ..summary.clone()
    };
    let result = RemoteTurnReport {
        session_id: SessionId::from("session"),
        turn_id: TurnId::from("turn"),
        outcome: RemoteTurnOutcome::Finished {
            finish: RemoteTurnFinish::AssistantMessage {
                text: "done".to_string(),
            },
        },
        assistant_output: RemoteAssistantOutput::default(),
        usage: RemoteTurnUsageReport::default(),
        execution: RemoteTurnExecutionMetrics::default(),
        tool_calls: Vec::new(),
        llm_calls: vec![summary],
        issues: Vec::new(),
        activities: vec![RemoteTurnActivity {
            sequence: 1,
            id: "model-call".to_string(),
            correlation_id: "same-call".to_string(),
            event: RemoteTurnEvent::ModelCallRecorded {
                record: activity_record,
            },
        }],
        metadata: HashMap::new(),
    };

    assert!(matches!(
        result.validate(),
        Err(RemoteProtocolError::ConflictingLlmCallRecord { call_id })
            if call_id == "same-call"
    ));
}

#[test]
fn turn_result_requires_one_summary_and_one_activity_per_model_call() {
    fn reconciled_result() -> RemoteTurnReport {
        let record = RemoteLlmCallRecord {
            call_id: "call-1".to_string(),
            label: None,
            replay_drops: Vec::new(),
            attempts: vec![RemoteAttemptRecord {
                ordinal: 1,
                started_at_ms: 1,
                duration_ms: 2,
                outcome: RemoteAttemptOutcome::Completed,
                protocol_position: RemoteProtocolPosition::TerminalObserved,
                retry_budget_consumed: true,
                retry_decision: None,
                error: None,
                evidence: None,
                generation_disposition: None,
                usage: None,
                usage_disposition: Default::default(),
            }],
        };
        RemoteTurnReport {
            session_id: SessionId::from("session"),
            turn_id: TurnId::from("turn"),
            outcome: RemoteTurnOutcome::Finished {
                finish: RemoteTurnFinish::AssistantMessage {
                    text: "done".to_string(),
                },
            },
            assistant_output: RemoteAssistantOutput::default(),
            usage: RemoteTurnUsageReport::default(),
            execution: RemoteTurnExecutionMetrics::default(),
            tool_calls: Vec::new(),
            llm_calls: vec![record.clone()],
            issues: Vec::new(),
            activities: vec![RemoteTurnActivity {
                sequence: 1,
                id: "event".to_string(),
                correlation_id: "call-1".to_string(),
                event: RemoteTurnEvent::ModelCallRecorded { record },
            }],
            metadata: HashMap::new(),
        }
    }

    let mut missing_activity = reconciled_result();
    missing_activity.activities.clear();
    assert!(matches!(
        missing_activity.validate(),
        Err(RemoteProtocolError::MissingLlmCallActivity { call_id }) if call_id == "call-1"
    ));

    let mut missing_summary = reconciled_result();
    missing_summary.llm_calls.clear();
    assert!(matches!(
        missing_summary.validate(),
        Err(RemoteProtocolError::MissingLlmCallSummary { call_id }) if call_id == "call-1"
    ));

    let mut duplicate_summary = reconciled_result();
    duplicate_summary
        .llm_calls
        .push(duplicate_summary.llm_calls[0].clone());
    assert!(matches!(
        duplicate_summary.validate(),
        Err(RemoteProtocolError::DuplicateLlmCallSummary { call_id }) if call_id == "call-1"
    ));

    let mut duplicate_activity = reconciled_result();
    duplicate_activity
        .activities
        .push(duplicate_activity.activities[0].clone());
    assert!(matches!(
        duplicate_activity.validate(),
        Err(RemoteProtocolError::DuplicateLlmCallActivity { call_id }) if call_id == "call-1"
    ));
}

#[test]
fn contradictory_model_call_ledgers_are_rejected_from_both_envelopes() {
    fn valid_attempt() -> RemoteAttemptRecord {
        RemoteAttemptRecord {
            ordinal: 1,
            started_at_ms: 0,
            duration_ms: 0,
            outcome: RemoteAttemptOutcome::Completed,
            protocol_position: RemoteProtocolPosition::TerminalObserved,
            retry_budget_consumed: true,
            retry_decision: None,
            error: None,
            evidence: None,
            generation_disposition: None,
            usage: None,
            usage_disposition: Default::default(),
        }
    }

    fn assert_rejected(attempt: RemoteAttemptRecord) {
        let record = RemoteLlmCallRecord {
            call_id: "llm-call".to_string(),
            label: None,
            replay_drops: Vec::new(),
            attempts: vec![attempt],
        };
        let activity = RemoteTurnActivity {
            sequence: 1,
            id: "event".to_string(),
            correlation_id: "correlation".to_string(),
            event: RemoteTurnEvent::ModelCallRecorded {
                record: record.clone(),
            },
        };
        assert!(activity.validate().is_err(), "activity accepted {record:?}");

        let result = RemoteTurnReport {
            session_id: SessionId::from("session"),
            turn_id: TurnId::from("turn"),
            outcome: RemoteTurnOutcome::Finished {
                finish: RemoteTurnFinish::AssistantMessage {
                    text: "done".to_string(),
                },
            },
            assistant_output: RemoteAssistantOutput::default(),
            usage: RemoteTurnUsageReport::default(),
            execution: RemoteTurnExecutionMetrics::default(),
            tool_calls: Vec::new(),
            llm_calls: vec![record.clone()],
            issues: Vec::new(),
            activities: Vec::new(),
            metadata: HashMap::new(),
        };
        assert!(result.validate().is_err(), "result accepted {record:?}");
    }

    let normalized_error = || RemoteNormalizedError {
        class: "provider".to_string(),
        code: None,
        http_status: None,
        provider_request_id: None,
        retry_after_ms: None,
    };

    let mut completed_with_error = valid_attempt();
    completed_with_error.error = Some(normalized_error());
    assert_rejected(completed_with_error);

    let mut completed_with_retry = valid_attempt();
    completed_with_retry.retry_decision = Some(RemoteRetryDecision {
        scheduled: true,
        delay_ms: Some(1),
        reason: Some("retry".to_string()),
    });
    assert_rejected(completed_with_retry);

    let mut completed_before_terminal = valid_attempt();
    completed_before_terminal.protocol_position = RemoteProtocolPosition::OutputStarted;
    assert_rejected(completed_before_terminal);

    let mut failed_without_error = valid_attempt();
    failed_without_error.outcome = RemoteAttemptOutcome::Failed;
    failed_without_error.protocol_position = RemoteProtocolPosition::NoResponse;
    assert_rejected(failed_without_error);
}

#[test]
fn valid_panic_partial_and_retry_ledgers_are_accepted_from_both_envelopes() {
    fn normalized_error(class: &str) -> RemoteNormalizedError {
        RemoteNormalizedError {
            class: class.to_string(),
            code: None,
            http_status: None,
            provider_request_id: None,
            retry_after_ms: None,
        }
    }

    fn assert_accepted(attempts: Vec<RemoteAttemptRecord>) {
        let record = RemoteLlmCallRecord {
            call_id: "llm-call".to_string(),
            label: None,
            replay_drops: Vec::new(),
            attempts,
        };
        RemoteTurnActivity {
            sequence: 1,
            id: "event".to_string(),
            correlation_id: "correlation".to_string(),
            event: RemoteTurnEvent::ModelCallRecorded {
                record: record.clone(),
            },
        }
        .validate()
        .expect("valid ledger in activity envelope");
        RemoteTurnReport {
            session_id: SessionId::from("session"),
            turn_id: TurnId::from("turn"),
            outcome: RemoteTurnOutcome::Finished {
                finish: RemoteTurnFinish::AssistantMessage {
                    text: "done".to_string(),
                },
            },
            assistant_output: RemoteAssistantOutput::default(),
            usage: RemoteTurnUsageReport::default(),
            execution: RemoteTurnExecutionMetrics::default(),
            tool_calls: Vec::new(),
            llm_calls: vec![record.clone()],
            issues: Vec::new(),
            activities: vec![RemoteTurnActivity {
                sequence: 1,
                id: "event".to_string(),
                correlation_id: "correlation".to_string(),
                event: RemoteTurnEvent::ModelCallRecorded { record },
            }],
            metadata: HashMap::new(),
        }
        .validate()
        .expect("valid ledger in result envelope");
    }

    assert_accepted(vec![RemoteAttemptRecord {
        ordinal: 1,
        started_at_ms: 0,
        duration_ms: 1,
        outcome: RemoteAttemptOutcome::Failed,
        protocol_position: RemoteProtocolPosition::NoResponse,
        retry_budget_consumed: false,
        retry_decision: None,
        error: Some(normalized_error("provider_panicked")),
        evidence: None,
        generation_disposition: None,
        usage: None,
        usage_disposition: Default::default(),
    }]);
    assert_accepted(vec![RemoteAttemptRecord {
        ordinal: 1,
        started_at_ms: 0,
        duration_ms: 1,
        outcome: RemoteAttemptOutcome::Interrupted,
        protocol_position: RemoteProtocolPosition::OutputStarted,
        retry_budget_consumed: false,
        retry_decision: None,
        error: Some(normalized_error("stream_interrupted")),
        evidence: Some(RemoteExecutionEvidence {
            collection_interruption: Some(
                RemoteExecutionEvidenceCollectionInterruption::ProtocolAbort,
            ),
            ..RemoteExecutionEvidence::default()
        }),
        generation_disposition: None,
        usage: None,
        usage_disposition: Default::default(),
    }]);
    assert_accepted(vec![
        RemoteAttemptRecord {
            ordinal: 1,
            started_at_ms: 0,
            duration_ms: 1,
            outcome: RemoteAttemptOutcome::Failed,
            protocol_position: RemoteProtocolPosition::NoResponse,
            retry_budget_consumed: true,
            retry_decision: Some(RemoteRetryDecision {
                scheduled: true,
                delay_ms: Some(1),
                reason: Some("retry".to_string()),
            }),
            error: Some(normalized_error("transport")),
            evidence: None,
            generation_disposition: None,
            usage: None,
            usage_disposition: Default::default(),
        },
        RemoteAttemptRecord {
            ordinal: 2,
            started_at_ms: 2,
            duration_ms: 1,
            outcome: RemoteAttemptOutcome::Completed,
            protocol_position: RemoteProtocolPosition::TerminalObserved,
            retry_budget_consumed: true,
            retry_decision: None,
            error: None,
            evidence: None,
            generation_disposition: None,
            usage: None,
            usage_disposition: Default::default(),
        },
    ]);
}
