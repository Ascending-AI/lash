use std::collections::HashMap;

use super::*;
use crate::REMOTE_PROTOCOL_VERSION;
use crate::registry_errors::RemoteProtocolError;

#[test]
fn remote_turn_status_projects_explicit_stopped_outcome_as_failed() {
    assert_eq!(
        RemoteTurnStatus::from(&RemoteTurnOutcome::Stopped {
            stop: RemoteTurnStop::Incomplete,
        }),
        RemoteTurnStatus::Failed
    );
}

#[test]
fn remote_turn_status_no_longer_accepts_in_progress_on_the_wire() {
    // Version 44 removed the variant; a version 43 peer can still emit the
    // literal, so pin that the decoder and the published schema both refuse
    // it rather than mapping it onto a terminal status.
    let error = serde_json::from_value::<RemoteTurnStatus>(serde_json::json!("in_progress"))
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
fn in_progress_turn_report_is_refused_by_version_negotiation_before_body_decode() {
    // A version 43 report is refused before its removed status value reaches
    // the current body decoder.
    let mut payload = serde_json::to_value(RemoteTurnReport {
        session_id: "session".to_string(),
        turn_id: "turn".to_string(),
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
    })
    .expect("serialize version 43 report");
    payload["protocol_version"] = serde_json::json!(43);

    let wire = serde_json::to_vec(&payload).expect("serialize version 43 report");
    assert!(matches!(
        RemoteTurnReport::decode_json(&wire),
        Err(RemoteProtocolError::UnsupportedProtocolVersion {
            actual: 43,
            expected: REMOTE_PROTOCOL_VERSION,
        })
    ));

    payload["status"] = serde_json::json!("in_progress");
    let wire = serde_json::to_vec(&payload).expect("serialize version 43 report");
    assert!(matches!(
        RemoteTurnReport::decode_json(&wire),
        Err(RemoteProtocolError::UnsupportedProtocolVersion {
            actual: 43,
            expected: REMOTE_PROTOCOL_VERSION,
        })
    ));
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
            kind: "runtime".into(),
            code: None,
            terminal_reason: None,
            message: "evidence".into(),
            raw: None,
            retryable: None,
            provider_failure_kind: None,
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
        (RemoteProcessStatus::CallerDeparted, "caller_departed"),
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
