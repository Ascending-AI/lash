#[test]
fn product_event_log_rejects_future_format_with_expected_and_found_versions() {
    let data_dir = tempfile::tempdir().expect("future product event tempdir");
    let path = data_dir.path().join("product-events.json");
    std::fs::write(&path, r#"{"format_version":2,"histories":{}}"#)
        .expect("write future product event log");

    let error = match SessionEventRegistry::persistent(path, 4) {
        Ok(_) => panic!("a future product event format must be rejected"),
        Err(error) => error,
    };
    let typed = error
        .downcast_ref::<ProductEventLogLoadError>()
        .expect("product event load failures remain typed");
    assert!(matches!(
        typed.source,
        ProductEventLogDecodeError::FormatVersionMismatch {
            expected: 1,
            found: 2
        }
    ));
    let rendered = error.to_string();
    assert!(rendered.contains("expected 1"), "actual error: {rendered}");
    assert!(rendered.contains("found 2"), "actual error: {rendered}");
}

#[test]
fn product_event_log_decode_error_names_histories_and_the_nested_cause() {
    let data_dir = tempfile::tempdir().expect("malformed product event tempdir");
    let path = data_dir.path().join("product-events.json");
    std::fs::write(
        &path,
        r#"{
            "format_version": 1,
            "histories": {
                "session": {
                    "cursor": 1,
                    "events": [{
                        "event_id": "call",
                        "sequence": 1,
                        "type": "model_call_recorded",
                        "record": {"call_id": "call"}
                    }]
                }
            }
        }"#,
    )
    .expect("write malformed product event log");

    let error = match SessionEventRegistry::persistent(path, 4) {
        Ok(_) => panic!("a malformed model-call record must be rejected"),
        Err(error) => error,
    };
    let rendered = error.to_string();
    assert!(rendered.contains("histories"), "actual error: {rendered}");
    assert!(rendered.contains("attempts"), "actual error: {rendered}");
}

#[test]
fn released_unversioned_product_event_histories_still_load() {
    let data_dir = tempfile::tempdir().expect("released product event tempdir");
    let path = data_dir.path().join("product-events.json");
    std::fs::write(
        &path,
        r#"{
            "released-session": {
                "cursor": 1,
                "events": [{
                    "event_id": "released-message",
                    "sequence": 1,
                    "type": "message",
                    "message": {
                        "id": "message",
                        "role": "assistant",
                        "text": "released main",
                        "at": ""
                    }
                }],
                "event_ids": ["released-message"]
            }
        }"#,
    )
    .expect("write released unversioned product event history");

    let registry = SessionEventRegistry::persistent(path, 4)
        .expect("released unversioned product event history remains compatible");
    let snapshot = registry.snapshot("released-session");
    assert_eq!(snapshot.cursor, 1);
    assert!(matches!(
        &snapshot.events[0].item,
        StreamItem::Message { message } if message.text == "released main"
    ));
}

#[test]
fn released_unversioned_log_allows_format_version_as_a_session_id() {
    let data_dir = tempfile::tempdir().expect("released collision tempdir");
    let path = data_dir.path().join("product-events.json");
    std::fs::write(
        &path,
        r#"{
            "format_version": {
                "cursor": 1,
                "events": [{
                    "event_id": "released-message",
                    "sequence": 1,
                    "type": "message",
                    "message": {
                        "id": "message",
                        "role": "assistant",
                        "text": "released collision",
                        "at": ""
                    }
                }],
                "event_ids": ["released-message"]
            }
        }"#,
    )
    .expect("write released session-id collision");

    let registry = SessionEventRegistry::persistent(path, 4)
        .expect("released root-key session id remains compatible");
    let snapshot = registry.snapshot("format_version");
    assert!(matches!(
        &snapshot.events[0].item,
        StreamItem::Message { message } if message.text == "released collision"
    ));
}

#[test]
fn persisted_attempt_rows_round_trip_non_default_outcomes_positions_and_facts() {
    use lash::remote::llm::{RemoteAttemptOutcome, RemoteProtocolPosition};

    let data_dir = tempfile::tempdir().expect("attempt row product event tempdir");
    let path = data_dir.path().join("product-events.json");
    let registry =
        SessionEventRegistry::persistent(path.clone(), 4).expect("persistent product events");
    let expected_record = lash::remote::llm::RemoteLlmCallRecord {
                call_id: "boundary-call".to_string(),
                label: Some("boundary".to_string()),
                replay_drops: Vec::new(),
                attempts: vec![
                    lash::remote::llm::RemoteAttemptRecord {
                        ordinal: 1,
                        started_at_ms: 10,
                        duration_ms: 3,
                        outcome: RemoteAttemptOutcome::Aborted,
                        protocol_position: RemoteProtocolPosition::ResponseObserved,
                        retry_budget_consumed: false,
                        retry_decision: Some(lash::remote::llm::RemoteRetryDecision {
                            scheduled: false,
                            delay_ms: Some(0),
                            reason: Some("cancelled".to_string()),
                        }),
                        error: Some(lash::remote::llm::RemoteNormalizedError {
                            class: "cancelled".to_string(),
                            provider_code: Some("request_cancelled".to_string()),
                            http_status: Some(499),
                            provider_request_id: Some("request-1".to_string()),
                            retry_after_ms: Some(25),
                        }),
                        evidence: Some(lash::remote::llm::RemoteExecutionEvidence {
                            provider_request_id: Some("request-1".to_string()),
                            collection_interruption: Some(
                                lash::remote::llm::RemoteExecutionEvidenceCollectionInterruption::ProtocolAbort,
                            ),
                            ..Default::default()
                        }),
                        generation_disposition: Some(
                            lash::remote::llm::RemoteGenerationDisposition {
                                output_token_cap: lash::remote::llm::RemoteGenerationOptionDisposition::ClampedToCapacity,
                                temperature: lash::remote::llm::RemoteGenerationOptionDisposition::OmittedSamplingPinned,
                                seed: lash::remote::llm::RemoteGenerationOptionDisposition::OmittedUnsupported,
                                stop_sequences: lash::remote::llm::RemoteGenerationOptionDisposition::SuppressedProtocolOwned,
                                cache: lash::remote::llm::RemoteGenerationOptionDisposition::Applied,
                            },
                        ),
                        usage: Some(lash::remote::usage::RemoteUsage {
                            input_tokens: 11,
                            output_tokens: 7,
                            cache_read_input_tokens: 3,
                            cache_write_input_tokens: 2,
                            reasoning_output_tokens: 5,
                        }),
                    },
                    lash::remote::llm::RemoteAttemptRecord {
                        ordinal: 2,
                        started_at_ms: 20,
                        duration_ms: 4,
                        outcome: RemoteAttemptOutcome::Interrupted,
                        protocol_position: RemoteProtocolPosition::OutputStarted,
                        retry_budget_consumed: true,
                        retry_decision: None,
                        error: Some(lash::remote::llm::RemoteNormalizedError {
                            class: "stream".to_string(),
                            provider_code: Some("eof".to_string()),
                            http_status: None,
                            provider_request_id: None,
                            retry_after_ms: None,
                        }),
                        evidence: None,
                        generation_disposition: None,
                        usage: None,
                    },
                ],
            };
    registry.publish_identified(
        "session",
        "model-call",
        StreamItem::ModelCallRecorded {
            record: expected_record.clone(),
        },
    );
    drop(registry);

    let reopened = SessionEventRegistry::persistent(path, 4).expect("reopen attempt rows");
    let snapshot = reopened.snapshot("session");
    let StreamItem::ModelCallRecorded { record } = &snapshot.events[0].item else {
        panic!("persisted event remains a model-call record");
    };
    assert_eq!(record, &expected_record);
    let aborted = &record.attempts[0];
    assert_eq!(aborted.outcome, RemoteAttemptOutcome::Aborted);
    let aborted_position = aborted.protocol_position;
    assert_eq!(aborted_position, RemoteProtocolPosition::ResponseObserved);
    let error = aborted.error.as_ref().expect("aborted error facts");
    assert_eq!(error.provider_code.as_deref(), Some("request_cancelled"));
    assert_eq!(error.http_status, Some(499));
    assert_eq!(error.provider_request_id.as_deref(), Some("request-1"));
    assert_eq!(error.retry_after_ms, Some(25));
    let generation = aborted
        .generation_disposition
        .expect("generation disposition");
    assert_eq!(
        generation.output_token_cap,
        lash::remote::llm::RemoteGenerationOptionDisposition::ClampedToCapacity
    );
    assert_eq!(
        aborted.usage.as_ref().expect("attempt usage").output_tokens,
        7
    );
    let interrupted = &record.attempts[1];
    assert_eq!(interrupted.outcome, RemoteAttemptOutcome::Interrupted);
    let interrupted_position = interrupted.protocol_position;
    assert_eq!(interrupted_position, RemoteProtocolPosition::OutputStarted);
}
