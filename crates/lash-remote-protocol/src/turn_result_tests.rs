use std::collections::HashMap;

use super::*;

#[test]
fn remote_turn_status_names_a_parked_run_and_refuses_the_retired_queued_tag() {
    let parked = RemoteTurnStatus::Parked {
        run: TurnId::from("run"),
        park_id: 7,
        reason: RemoteTurnParkReason {
            code: "binding_drift".to_string(),
            message: "tool `search` changed".to_string(),
            profile_key: None,
        },
        since_ms: 1_000,
        attempts: 2,
    };
    let wire = serde_json::json!({
        "type": "parked",
        "run": "run",
        "park_id": 7,
        "reason": { "code": "binding_drift", "message": "tool `search` changed" },
        "since_ms": 1_000,
        "attempts": 2,
    });
    assert_eq!(serde_json::to_value(&parked).expect("encode parked"), wire);
    assert_eq!(
        serde_json::from_value::<RemoteTurnStatus>(wire).expect("decode parked"),
        parked
    );
    assert_eq!(
        serde_json::to_value(RemoteTurnStatus::Answered).expect("encode answered"),
        serde_json::json!({"type": "answered"})
    );
    // Window 100 retired the queued outcome and status: no turn answers
    // "queued" once the engine executes every accepted input.
    serde_json::from_value::<RemoteTurnStatus>(serde_json::json!({"type": "queued"}))
        .expect_err("queued is no longer a remote turn status");
    serde_json::from_value::<RemoteTurnOutcome>(serde_json::json!({"type": "queued", "ahead": 3}))
        .expect_err("queued is no longer a remote turn outcome");
}

#[test]
fn remote_turn_status_no_longer_accepts_in_progress_on_the_wire() {
    // Version 44 removed the variant; a version 43 peer can still emit the
    // literal, so pin that the decoder and the published schema both refuse
    // it rather than mapping it onto a terminal status.
    let error =
        serde_json::from_value::<RemoteTurnStatus>(serde_json::json!({"type": "in_progress"}))
            .expect_err("in_progress is no longer a remote turn status");
    assert!(
        error.to_string().contains("in_progress"),
        "decoder must name the refused value: {error}"
    );

    let schema = serde_json::to_value(schemars::schema_for!(RemoteTurnStatus))
        .expect("serialize turn status schema");
    assert!(
        !schema.to_string().contains("in_progress"),
        "published schema still advertises in_progress: {schema}"
    );
}

#[test]
fn issue_severity_is_required_and_has_pinned_wire_values() {
    for (severity, literal) in [
        (RemoteTurnIssueSeverity::Advisory, "advisory"),
        (RemoteTurnIssueSeverity::Blocking, "blocking"),
    ] {
        assert_eq!(
            serde_json::to_value(severity).unwrap(),
            serde_json::json!(literal)
        );
        let issue = RemoteTurnIssue {
            severity,
            kind: RemoteTurnFailureKind::Runtime,
            code: None,
            terminal_reason: None,
            message: "evidence".into(),
            raw: None,
            retryable: None,
            provider_failure_kind: None,
            plugin_failures: Vec::new(),
        };
        let mut wire = serde_json::to_value(&issue).unwrap();
        assert_eq!(
            serde_json::from_value::<RemoteTurnIssue>(wire.clone()).unwrap(),
            issue
        );
        wire.as_object_mut().unwrap().remove("severity");
        assert!(serde_json::from_value::<RemoteTurnIssue>(wire).is_err());
    }
    assert!(
        serde_json::from_value::<RemoteTurnIssueSeverity>(serde_json::json!("future")).is_err()
    );
}

#[test]
fn process_status_sets_pin_vocabulary_and_refuse_removed_fields() {
    use crate::{RemoteProcessListFilter, RemoteProcessStatus, RemoteProcessStatusFilter};
    for (status, literal) in [
        (RemoteProcessStatus::Running, "running"),
        (RemoteProcessStatus::Waiting, "waiting"),
        (RemoteProcessStatus::Completed, "completed"),
        (RemoteProcessStatus::Failed, "failed"),
        (RemoteProcessStatus::Cancelled, "cancelled"),
        (RemoteProcessStatus::Abandoned, "abandoned"),
    ] {
        let filter = RemoteProcessStatusFilter::any_of([status]);
        let core: lash_core::ProcessStatusFilter = filter.clone().into();
        assert_eq!(core.labels(), Some(vec![literal]));
        assert_eq!(
            serde_json::to_value(&filter).unwrap(),
            serde_json::json!({"in":[literal]})
        );
        assert_eq!(
            serde_json::from_value::<RemoteProcessStatusFilter>(
                serde_json::json!({"in":[literal,literal]})
            )
            .unwrap(),
            filter
        );
    }
    assert_eq!(
        serde_json::to_value(RemoteProcessStatusFilter::Any).unwrap(),
        "any"
    );
    assert_eq!(
        serde_json::from_value::<RemoteProcessListFilter>(serde_json::json!({}))
            .unwrap()
            .status,
        RemoteProcessStatusFilter::any_of([RemoteProcessStatus::Running])
    );
    for bad in [
        serde_json::json!({"waiting":true}),
        serde_json::json!({"status":"running"}),
        serde_json::json!({"status":{"in":["future"]}}),
        serde_json::json!({"status":{"not":["waiting"]}}),
    ] {
        assert!(serde_json::from_value::<RemoteProcessListFilter>(bad).is_err());
    }
    let core = lash_core::ProcessStatusFilter::decode(Some(
        &serde_json::json!({"in":["running","waiting"]}),
    ))
    .unwrap();
    assert!(core.matches(lash_core::ProcessStatus::Running));
    assert!(core.matches(lash_core::ProcessStatus::Waiting));
    assert!(!core.matches(lash_core::ProcessStatus::Completed));
    assert!(lash_core::ProcessListFilter::decode(&serde_json::json!({"waiting":false})).is_err());
}

#[test]
fn remote_provider_failure_kind_refuses_future_literals() {
    assert!(
        serde_json::from_value::<RemoteProviderFailureKind>(serde_json::json!("future_kind"))
            .is_err()
    );
    for (kind, literal) in [
        (RemoteProviderFailureKind::Transport, "transport"),
        (RemoteProviderFailureKind::Timeout, "timeout"),
        (RemoteProviderFailureKind::Http, "http"),
        (RemoteProviderFailureKind::Stream, "stream"),
        (RemoteProviderFailureKind::Auth, "auth"),
        (RemoteProviderFailureKind::Validation, "validation"),
        (RemoteProviderFailureKind::Quota, "quota"),
        (RemoteProviderFailureKind::Unsupported, "unsupported"),
        (RemoteProviderFailureKind::Unknown, "unknown"),
    ] {
        assert_eq!(serde_json::to_value(kind).unwrap(), literal);
        assert_eq!(
            serde_json::from_value::<RemoteProviderFailureKind>(serde_json::json!(literal))
                .unwrap(),
            kind
        );
    }
}

/// FIG-3094: a host distinguishes the failure classes it reacts to by matching
/// typed arms, never by substring-matching the display message, and the wire
/// spellings are exactly the ones the untyped field carried.
#[test]
fn turn_issue_failure_vocabulary_is_typed_and_wire_stable() {
    let cases = [
        (
            RemoteTurnFailureKind::LlmProvider,
            RemoteFailureCode::lash(lash_sansio::TurnFailureCode::ContextOverflow),
            "llm_provider",
            "lash:context_overflow",
        ),
        (
            RemoteTurnFailureKind::LlmProvider,
            RemoteFailureCode::lash(lash_sansio::TurnFailureCode::ContentFilter),
            "llm_provider",
            "lash:content_filter",
        ),
        (
            RemoteTurnFailureKind::Runtime,
            RemoteFailureCode::lash(lash_sansio::TurnFailureCode::ReconfigureFailed),
            "runtime",
            "lash:reconfigure_failed",
        ),
        (
            RemoteTurnFailureKind::TokenUsageAccounting,
            RemoteFailureCode::lash(lash_sansio::TurnFailureCode::TokenUsageOverflow),
            "token_usage_accounting",
            "lash:token_usage_overflow",
        ),
    ];
    for (kind, code, kind_wire, code_wire) in cases {
        let issue = RemoteTurnIssue {
            severity: RemoteTurnIssueSeverity::Blocking,
            kind: kind.clone(),
            code: Some(code.clone()),
            terminal_reason: None,
            message: "human prose a host must not parse".into(),
            raw: None,
            retryable: None,
            provider_failure_kind: None,
            plugin_failures: Vec::new(),
        };
        let wire = serde_json::to_value(&issue).expect("serialize issue");
        assert_eq!(wire["kind"], serde_json::json!(kind_wire));
        assert_eq!(wire["code"], serde_json::json!(code_wire));
        let decoded: RemoteTurnIssue = serde_json::from_value(wire).expect("decode issue");
        assert_eq!(decoded.kind, kind);
        assert_eq!(decoded.code, Some(code));
    }

    // A spelling this build does not own — a provider error code, or an arm a
    // newer peer added — round-trips through the open arms without loss.
    let foreign = serde_json::json!({
        "severity": "blocking",
        "kind": "a_kind_from_a_newer_peer",
        "code": "429",
        "message": "rate limited",
    });
    let decoded: RemoteTurnIssue =
        serde_json::from_value(foreign.clone()).expect("decode foreign issue");
    assert_eq!(
        decoded.kind,
        RemoteTurnFailureKind::Unknown("a_kind_from_a_newer_peer".to_string())
    );
    assert_eq!(decoded.code, Some(RemoteFailureCode::provider("429")));
    assert_eq!(
        serde_json::to_value(&decoded).expect("re-encode foreign issue")["kind"],
        serde_json::json!("a_kind_from_a_newer_peer")
    );
}

fn answered_report() -> RemoteTurnReport {
    RemoteTurnReport {
        session_id: SessionId::from("session"),
        turn_id: TurnId::from("run"),
        outcome: RemoteTurnOutcome::Finished {
            finish: RemoteTurnFinish::AssistantMessage {
                text: "done".to_string(),
            },
        },
        assistant_output: RemoteAssistantOutput::default(),
        usage: RemoteTurnUsageReport::default(),
        execution: RemoteTurnExecutionMetrics::default(),
        tool_calls: Vec::new(),
        llm_calls: Vec::new(),
        issues: Vec::new(),
        activities: Vec::new(),
        metadata: HashMap::new(),
    }
}

#[test]
fn a_remote_send_outcome_carries_only_its_variants_data() {
    let session_id = SessionId::from("session");
    let input_id = "ti:input".to_string();
    let settled = RemoteSendOutcome::Settled {
        session_id: session_id.clone(),
        input_id: input_id.clone(),
        report: Box::new(answered_report()),
        gaps: Vec::new(),
    };
    let mut failed_report = answered_report();
    failed_report.outcome = RemoteTurnOutcome::Stopped {
        stop: RemoteTurnStop::Incomplete,
    };
    let failed = RemoteSendOutcome::Settled {
        session_id: session_id.clone(),
        input_id: input_id.clone(),
        report: Box::new(failed_report),
        gaps: Vec::new(),
    };
    let mut cancelled_report = answered_report();
    cancelled_report.outcome = RemoteTurnOutcome::Stopped {
        stop: RemoteTurnStop::Cancelled {
            evidence: RemoteTurnCancellationEvidence {
                request_id: "cancel-fixture".into(),
                origin: Some("fixture".into()),
                reason: None,
                undelivered: crate::RemoteTurnCancelUndeliveredInputPolicy::Defer,
                mode: crate::RemoteTurnCancelMode::Immediate,
                honoured_after_step: None,
            },
        },
    };
    let cancelled = RemoteSendOutcome::Settled {
        session_id: session_id.clone(),
        input_id: input_id.clone(),
        report: Box::new(cancelled_report),
        gaps: Vec::new(),
    };
    let parked = RemoteSendOutcome::Parked {
        session_id: session_id.clone(),
        input_id: input_id.clone(),
        parked: RemoteParkedTurn {
            run: "run".into(),
            park_id: 7,
            reason: RemoteTurnParkReason {
                code: "binding_drift".into(),
                message: "tool changed".into(),
                profile_key: None,
            },
            since_ms: 1000,
            attempts: 2,
        },
        gaps: Vec::new(),
    };
    let stalled = RemoteSendOutcome::Stalled {
        session_id: session_id.clone(),
        input_id: input_id.clone(),
        stalled: RemoteStalledDelivery {
            reason: "attempts_exhausted".into(),
            attempts: 3,
            code: Some("runtime_store".into()),
            last_error: Some("delivery failed".into()),
            stalled_at_ms: 1000,
        },
        gaps: Vec::new(),
    };
    let refused = RemoteSendOutcome::Refused {
        session_id: session_id.clone(),
        input_id: input_id.clone(),
        run: Some("run".into()),
        refusal: Box::new(
            lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::PluginFinalizeTurn,
                "the finalize hook refused",
            )
            .into(),
        ),
        gaps: Vec::new(),
    };
    let withdrawn = RemoteSendOutcome::Withdrawn {
        session_id: session_id.clone(),
        input_id: input_id.clone(),
        gaps: Vec::new(),
    };
    let not_accepted = RemoteSendOutcome::NotAccepted {
        session_id,
        input_id,
        gaps: Vec::new(),
    };
    assert_eq!(settled.status(), RemoteTurnStatus::Answered);
    assert_eq!(settled.run(), Some(&TurnId::from("run")));
    assert!(matches!(parked.status(), RemoteTurnStatus::Parked { .. }));
    assert!(matches!(stalled.status(), RemoteTurnStatus::Stalled { .. }));
    assert_eq!(refused.status(), RemoteTurnStatus::Failed);
    assert_eq!(refused.run(), Some(&TurnId::from("run")));
    assert_eq!(withdrawn.status(), RemoteTurnStatus::Cancelled);
    assert_eq!(not_accepted.status(), RemoteTurnStatus::NotAccepted);
    let operation = RemoteSendOutcome::OperationSettled {
        session_id: SessionId::from("session"),
        input_id: "operation".into(),
        run: TurnId::from("shift-operation:batch"),
        outcome: RemoteOperationOutcome::Completed {
            plugin_id: "accept".into(),
            output: serde_json::json!("done"),
            events: vec![lash_sansio::PluginRuntimeEvent::Status {
                key: "task".into(),
                label: "done".into(),
                detail: None,
            }],
            pending_input_ids: vec![lash_sansio::InputId::from("ti:child")],
        },
        gaps: Vec::new(),
    };
    assert_eq!(operation.status(), RemoteTurnStatus::Answered);
    assert_eq!(
        operation.run(),
        Some(&TurnId::from("shift-operation:batch"))
    );
    let schema = serde_json::to_value(schemars::schema_for!(RemoteSendOutcome)).expect("schema");
    let validator = jsonschema::validator_for(&schema).expect("validator");
    for outcome in [
        settled,
        failed,
        cancelled,
        parked,
        stalled,
        refused,
        withdrawn,
        not_accepted,
        operation,
    ] {
        outcome.validate().expect("variant validates");
        let value = serde_json::to_value(&outcome).expect("serialize");
        assert!(validator.is_valid(&value), "{value}");
        assert!(value.get("status").is_none());
        assert!(value.get("run_id").is_none());
        let wire = outcome
            .encode_json(&crate::negotiation::test_negotiated())
            .expect("encode");
        assert_eq!(
            RemoteSendOutcome::decode_json(&wire).expect("decode"),
            outcome
        );
        if let Some(field) = match &outcome {
            RemoteSendOutcome::OperationSettled { .. } => Some("outcome"),
            RemoteSendOutcome::Settled { .. } => Some("report"),
            RemoteSendOutcome::Parked { .. } => Some("parked"),
            RemoteSendOutcome::Stalled { .. } => Some("stalled"),
            RemoteSendOutcome::Refused { .. } => Some("refusal"),
            RemoteSendOutcome::Withdrawn { .. } | RemoteSendOutcome::NotAccepted { .. } => None,
        } {
            let mut missing = value.clone();
            missing.as_object_mut().expect("object").remove(field);
            assert!(!validator.is_valid(&missing));
            assert!(serde_json::from_value::<RemoteSendOutcome>(missing).is_err());
        }
        let mut wrong = value;
        wrong["status"] = serde_json::json!({"type": "failed"});
        assert!(!validator.is_valid(&wrong));
        assert!(serde_json::from_value::<RemoteSendOutcome>(wrong).is_err());
    }
}
