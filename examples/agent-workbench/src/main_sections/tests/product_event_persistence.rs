use super::*;
use lash::SessionId;

#[test]
fn product_event_log_rejects_future_format_with_expected_and_found_versions() {
    let data_dir = tempfile::tempdir().expect("future product event tempdir");
    let path = data_dir.path().join("product-events.json");
    std::fs::write(&path, r#"{"format_version":3,"histories":{}}"#)
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
            expected: 2,
            found: 3
        }
    ));
    let rendered = error.to_string();
    assert!(rendered.contains("expected 2"), "actual error: {rendered}");
    assert!(rendered.contains("found 3"), "actual error: {rendered}");
}

#[test]
fn product_event_log_decode_error_names_histories_and_the_nested_cause() {
    let data_dir = tempfile::tempdir().expect("malformed product event tempdir");
    let path = data_dir.path().join("product-events.json");
    std::fs::write(
        &path,
        r#"{
            "format_version": 2,
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
fn product_event_log_rejects_unversioned_product_event_root_with_clear_error() {
    let data_dir = tempfile::tempdir().expect("unversioned product event tempdir");
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
    .expect("write unversioned product event history");

    let error = match SessionEventRegistry::persistent(path.clone(), 4) {
        Ok(_) => panic!("an unversioned product event root must be rejected"),
        Err(error) => error,
    };
    let typed = error
        .downcast_ref::<ProductEventLogLoadError>()
        .expect("product event load failures remain typed");
    assert!(matches!(
        typed.source,
        ProductEventLogDecodeError::UnversionedRoot
    ));
    let rendered = error.to_string();
    assert!(
        rendered.contains(
            "unversioned product event log is not supported; expected a root object with `format_version` and `histories`"
        ),
        "actual error: {rendered}"
    );
}

#[test]
fn active_turns_reject_bare_legacy_set_with_clear_error() {
    let data_dir = tempfile::tempdir().expect("legacy active turns tempdir");
    let path = data_dir.path().join("active-turns.json");
    std::fs::write(&path, r#"[["session", "turn"]]"#).expect("write bare active turns set");

    let error = match ActiveTurns::persistent(path.clone()) {
        Ok(_) => panic!("a bare active-turns set must be rejected"),
        Err(error) => error,
    };
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("decode active turns")
            && rendered.contains(
                "legacy bare active turn set is no longer supported; expected an object with `turns` and `prompts`"
            ),
        "actual error: {rendered}"
    );
}

#[test]
fn persisted_attempt_rows_round_trip_non_default_outcomes_positions_and_facts() {
    use lash::provider::{AttemptOutcome, ProtocolPosition};

    let data_dir = tempfile::tempdir().expect("attempt row product event tempdir");
    let path = data_dir.path().join("product-events.json");
    let registry =
        SessionEventRegistry::persistent(path.clone(), 4).expect("persistent product events");
    let expected_record = lash::LlmCallRecord {
        call_id: lash::LlmCallId("boundary-call".to_string()),
        label: Some("boundary".to_string()),
        replay_drops: Vec::new(),
        attempts: vec![
            lash::AttemptRecord {
                ordinal: 1,
                outcome: AttemptOutcome::Aborted,
                protocol_position: ProtocolPosition::ResponseObserved,
                retry_budget_consumed: false,
                retry_decision: Some(lash::provider::RetryDecision::Declined(
                    lash::provider::RetryDeclineCause::NotRetryable,
                )),
                error: Some(lash::provider::NormalizedError {
                    class: lash::provider::ProviderFailureKind::Unknown,
                    code: Some(lash::provider::FailureCode::provider("request_cancelled")),
                    http_status: Some(499),
                    provider_request_id: Some("request-1".to_string()),
                    retry_after: Some(Duration::from_millis(25)),
                }),
                evidence: Some(lash::provider::ExecutionEvidence {
                    provider_request_id: Some("request-1".to_string()),
                    collection_interruption: Some(
                        lash::provider::ExecutionEvidenceCollectionInterruption::ProtocolAbort,
                    ),
                    ..Default::default()
                }),
                generation_disposition: Some(lash::direct::GenerationReceipt {
                    output_token_cap: lash::direct::GenerationOptionOutcome::ClampedToCapacity,
                    temperature: lash::direct::GenerationOptionOutcome::Applied,
                    seed: lash::direct::GenerationOptionOutcome::NotRequested,
                    stop_sequences: lash::direct::GenerationOptionOutcome::SuppressedProtocolOwned,
                    cache: lash::direct::GenerationOptionOutcome::OmittedUnsupported,
                    ..Default::default()
                }),
                usage: Some(lash::usage::LlmUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    cache_read_input_tokens: 3,
                    cache_write_input_tokens: 2,
                    reasoning_output_tokens: 5,
                }),
            },
            lash::AttemptRecord {
                ordinal: 2,
                outcome: AttemptOutcome::Interrupted,
                protocol_position: ProtocolPosition::OutputStarted,
                retry_budget_consumed: true,
                retry_decision: None,
                error: Some(lash::provider::NormalizedError {
                    class: lash::provider::ProviderFailureKind::Stream,
                    code: Some(lash::provider::FailureCode::provider("eof")),
                    http_status: None,
                    provider_request_id: None,
                    retry_after: None,
                }),
                evidence: None,
                generation_disposition: None,
                usage: None,
            },
        ],
    };
    registry.publish_identified(
        &SessionId::from("session"),
        "model-call",
        StreamItem::ModelCallRecorded {
            record: expected_record.clone(),
        },
    );
    drop(registry);

    let reopened = SessionEventRegistry::persistent(path, 4).expect("reopen attempt rows");
    let snapshot = reopened.snapshot(&SessionId::from("session"));
    let StreamItem::ModelCallRecorded { record } = &snapshot.events[0].item else {
        panic!("persisted event remains a model-call record");
    };
    assert_eq!(record, &expected_record);
    let aborted = &record.attempts[0];
    assert_eq!(aborted.outcome, AttemptOutcome::Aborted);
    let aborted_position = aborted.protocol_position;
    assert_eq!(aborted_position, ProtocolPosition::ResponseObserved);
    let error = aborted.error.as_ref().expect("aborted error facts");
    assert_eq!(
        error.code.as_ref().map(|code| code.namespaced()),
        Some("provider:request_cancelled".to_string())
    );
    assert_eq!(error.http_status, Some(499));
    assert_eq!(error.provider_request_id.as_deref(), Some("request-1"));
    assert_eq!(error.retry_after, Some(Duration::from_millis(25)));
    let generation = aborted
        .generation_disposition
        .expect("generation disposition");
    assert_eq!(
        generation.output_token_cap,
        lash::direct::GenerationOptionOutcome::ClampedToCapacity
    );
    assert_eq!(
        aborted.usage.as_ref().expect("attempt usage").output_tokens,
        7
    );
    let interrupted = &record.attempts[1];
    assert_eq!(interrupted.outcome, AttemptOutcome::Interrupted);
    let interrupted_position = interrupted.protocol_position;
    assert_eq!(interrupted_position, ProtocolPosition::OutputStarted);
}

/// The handover successor opens the same data dir while the drained host is
/// still writing, so two `ActiveTurns` share one `active-turns.json`. Each
/// write stages under its own name before the rename: a shared staging name
/// let a peer's rename move the file out from under this write and panic the
/// persisting thread (ENOENT on the staged path).
#[test]
fn two_generations_persist_active_turns_to_one_shared_file() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let data_dir = tempfile::tempdir().expect("shared active turns tempdir");
    let path = data_dir.path().join("active-turns.json");
    let drained = ActiveTurns::persistent(path.clone()).expect("draining active turns");
    let successor = ActiveTurns::persistent(path.clone()).expect("successor active turns");
    // A two-party rendezvous per write so the peers' staging and renames
    // overlap; a panicking writer flags `failed` so its peer stops rather than
    // wait on a rendezvous that never comes.
    let failed = Arc::new(AtomicBool::new(false));
    let epochs = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
    let writes = 200;
    let mut threads = Vec::new();
    for (side, (handle, prefix)) in [(drained, "drained"), (successor, "successor")]
        .into_iter()
        .enumerate()
    {
        let failed = Arc::clone(&failed);
        let epochs = Arc::clone(&epochs);
        threads.push(std::thread::spawn(move || {
            let wrote = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                for index in 0..writes {
                    epochs[side].store(index + 1, Ordering::Relaxed);
                    while epochs[1 - side].load(Ordering::Relaxed) < index + 1
                        && !failed.load(Ordering::Relaxed)
                    {
                        std::thread::yield_now();
                    }
                    if failed.load(Ordering::Relaxed) {
                        return;
                    }
                    handle.insert(
                        SessionId::fixture(format!("{prefix}-session-{index}")),
                        TurnId::fixture(format!("{prefix}-turn-{index}")),
                        WorkbenchTurnKind::User,
                    );
                }
            }));
            if wrote.is_err() {
                failed.store(true, Ordering::Relaxed);
            }
        }));
    }
    for thread in threads {
        thread.join().expect("writer thread joined");
    }
    assert!(
        !failed.load(Ordering::Relaxed),
        "a shared-data-dir write panicked"
    );
    let bytes = std::fs::read(&path).expect("read shared active turns");
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .expect("a shared active-turns file stays decodable");
    assert!(
        data_dir
            .path()
            .read_dir()
            .expect("list shared data dir")
            .all(|entry| !entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")),
        "a completed write leaves no staged files"
    );
}
