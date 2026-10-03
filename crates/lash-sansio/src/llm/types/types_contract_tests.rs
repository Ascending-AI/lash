use super::*;

fn response_started() -> LlmStreamEvidence {
    LlmStreamEvidence {
        response_started: true,
        ..LlmStreamEvidence::default()
    }
}

#[test]
fn execution_identity_and_reasoning_counts_are_monotonic_at_the_shared_seam() {
    let mut evidence = response_started();
    evidence
        .merge(LlmStreamEvidence {
            execution_evidence: Some(ExecutionEvidence {
                served_model: Some("served-a".to_string()),
                provider_response_id: Some("response-a".to_string()),
                reasoning_output_tokens: Some(17),
                ..ExecutionEvidence::default()
            }),
            ..LlmStreamEvidence::default()
        })
        .expect("first provider facts establish shared evidence");
    evidence
        .merge(LlmStreamEvidence {
            execution_evidence: Some(ExecutionEvidence {
                served_model: Some("served-a".to_string()),
                provider_response_id: Some("response-a".to_string()),
                reasoning_output_tokens: Some(0),
                ..ExecutionEvidence::default()
            }),
            ..LlmStreamEvidence::default()
        })
        .expect("a trailing explicit zero cannot regress a positive count");
    let conflict = evidence
        .merge(LlmStreamEvidence {
            execution_evidence: Some(ExecutionEvidence {
                served_model: Some("served-b".to_string()),
                provider_response_id: Some("response-b".to_string()),
                ..ExecutionEvidence::default()
            }),
            ..LlmStreamEvidence::default()
        })
        .expect_err("identity drift must fail at the shared seam");

    assert!(matches!(
        conflict,
        ExecutionEvidenceMergeError::IdentityConflict {
            field: "served_model",
            ..
        }
    ));
    assert_eq!(conflict.code(), "stream_evidence_identity_conflict");

    let merged = evidence.execution_evidence.expect("execution evidence");
    assert_eq!(merged.served_model.as_deref(), Some("served-a"));
    assert_eq!(merged.provider_response_id.as_deref(), Some("response-a"));
    assert_eq!(merged.reasoning_output_tokens, Some(17));
}

#[test]
fn execution_evidence_cannot_precede_response_establishment() {
    let mut evidence = LlmStreamEvidence::default();
    let error = evidence
        .merge(LlmStreamEvidence {
            http_summary: Some("incidental request diagnostic".to_string()),
            execution_evidence: Some(ExecutionEvidence {
                provider_response_id: Some("too-early".to_string()),
                ..ExecutionEvidence::default()
            }),
            ..LlmStreamEvidence::default()
        })
        .expect_err("provider facts before response start must fail");

    assert_eq!(error, ExecutionEvidenceMergeError::BeforeResponseStart);
    assert_eq!(error.code(), "stream_evidence_before_response_start");
    assert!(!evidence.response_started);
    assert_eq!(evidence.http_summary, None);
    assert_eq!(evidence.execution_evidence, None);
}

#[test]
fn response_establishment_and_execution_evidence_can_arrive_together_without_a_summary() {
    let mut evidence = LlmStreamEvidence::default();
    evidence
        .merge(LlmStreamEvidence {
            response_started: true,
            execution_evidence: Some(ExecutionEvidence {
                provider_response_id: Some("response-a".to_string()),
                ..ExecutionEvidence::default()
            }),
            ..LlmStreamEvidence::default()
        })
        .expect("the response marker admits evidence in the same event");

    assert!(evidence.response_started);
    assert_eq!(evidence.http_summary, None);
    assert_eq!(
        evidence
            .execution_evidence
            .as_ref()
            .and_then(|evidence| evidence.provider_response_id.as_deref()),
        Some("response-a")
    );
}

#[test]
fn response_establishment_is_monotonic_and_admits_later_no_summary_evidence() {
    let mut evidence = LlmStreamEvidence::default();
    evidence
        .merge(response_started())
        .expect("response establishment has no dependent evidence");
    evidence
        .merge(LlmStreamEvidence {
            execution_evidence: Some(ExecutionEvidence {
                served_model: Some("served-a".to_string()),
                ..ExecutionEvidence::default()
            }),
            ..LlmStreamEvidence::default()
        })
        .expect("established response accepts later evidence without metadata");

    assert!(evidence.response_started);
    assert_eq!(evidence.http_summary, None);
    assert_eq!(
        evidence
            .execution_evidence
            .as_ref()
            .and_then(|evidence| evidence.served_model.as_deref()),
        Some("served-a")
    );
}

#[test]
fn execution_identity_conflict_leaves_the_collector_atomic() {
    let mut evidence = LlmStreamEvidence {
        response_started: true,
        provider_usage: Some(serde_json::json!({"input_tokens": 3})),
        request_body: Some("original request".to_string()),
        http_summary: Some("original summary".to_string()),
        execution_evidence: Some(ExecutionEvidence {
            served_model: Some("served-a".to_string()),
            provider_response_id: Some("response-a".to_string()),
            ..ExecutionEvidence::default()
        }),
        response_metadata: std::collections::BTreeMap::from([(
            "header:x-original".to_string(),
            serde_json::json!("kept"),
        )]),
        ..LlmStreamEvidence::default()
    };

    let error = evidence
        .merge(LlmStreamEvidence {
            provider_usage: Some(serde_json::json!({"input_tokens": 99})),
            request_body: Some("replacement request".to_string()),
            http_summary: Some("replacement summary".to_string()),
            execution_evidence: Some(ExecutionEvidence {
                served_model: Some("served-b".to_string()),
                provider_response_id: Some("response-b".to_string()),
                ..ExecutionEvidence::default()
            }),
            response_metadata: std::collections::BTreeMap::from([(
                "header:x-replacement".to_string(),
                serde_json::json!("discarded"),
            )]),
            ..LlmStreamEvidence::default()
        })
        .expect_err("identity drift rejects the whole incoming evidence event");

    assert_eq!(error.code(), "stream_evidence_identity_conflict");
    assert!(evidence.response_started);
    assert_eq!(
        evidence.provider_usage,
        Some(serde_json::json!({"input_tokens": 3}))
    );
    assert_eq!(evidence.request_body.as_deref(), Some("original request"));
    assert_eq!(evidence.http_summary.as_deref(), Some("original summary"));
    assert_eq!(
        evidence
            .execution_evidence
            .as_ref()
            .and_then(|evidence| evidence.served_model.as_deref()),
        Some("served-a")
    );
    assert_eq!(
        evidence.response_metadata,
        std::collections::BTreeMap::from([(
            "header:x-original".to_string(),
            serde_json::json!("kept"),
        )])
    );
}
#[test]
fn sealed_retry_decision_refuses_the_contradictory_boolean_shape() {
    let bytes = serde_json::json!({
        "scheduled": false,
        "delay": {"secs": 0, "nanos": 0},
        "reason": "not_retryable"
    });
    assert!(serde_json::from_value::<super::RetryDecision>(bytes).is_err());
}

#[test]
fn sealed_normalized_error_refuses_unknown_failure_classes() {
    let bytes = serde_json::json!({"class": "rate_limited"});
    assert!(serde_json::from_value::<super::NormalizedError>(bytes).is_err());
}
