use super::*;

#[test]
fn cancelled_stop_conversion_keeps_every_evidence_field() {
    let core = lash_core::facade_support::TurnCancellationEvidence {
        request_id: "cancel-exact".into(),
        origin: Some("host".into()),
        reason: Some("stop now".into()),
        undelivered: lash_core::facade_support::TurnCancelDisposition::Defer,
        mode: lash_core::facade_support::TurnCancelMode::Immediate,
        honoured_after_step: None,
    };
    let expected = RemoteTurnCancellationEvidence::from(core.clone());
    let stop =
        RemoteTurnStop::from(lash_core::facade_support::TurnStop::Cancelled { evidence: core });
    assert_eq!(stop, RemoteTurnStop::Cancelled { evidence: expected });
}

#[test]
fn cancelled_mid_call_record_converts_and_validates() {
    assert_terminal_call_record_converts_and_validates(
        synthetic_terminal_call_record(
            "cancelled-call",
            lash_core::AttemptOutcome::Aborted,
            lash_core::ProviderFailureKind::Unknown,
            "cancelled",
            true,
        ),
        true,
    );
}

#[test]
fn turn_cancel_core_conversions_round_trip_every_envelope() {
    let core_request = lash_core::facade_support::TurnCancelRequest::new(
        lash_core::facade_support::TurnAddress::new("session", "turn"),
        "cancel-request",
        Some("queue-superseder".to_string()),
    )
    .with_reason("newer input arrived");
    let remote_request = RemoteTurnCancelRequest::from(core_request.clone());
    remote_request
        .validate()
        .expect("valid remote cancel request");
    let round_trip = remote_request.try_into_core().expect("core cancel request");
    assert_eq!(round_trip, core_request);

    let evidence = lash_core::facade_support::TurnCancellationEvidence {
        request_id: "cancel-request".to_string(),
        origin: Some("workbench-user".to_string()),
        reason: Some("stop button".to_string()),
        undelivered: lash_core::facade_support::TurnCancelDisposition::Defer,
        mode: lash_core::facade_support::TurnCancelMode::Immediate,
        honoured_after_step: None,
    };
    let remote_evidence = RemoteTurnCancellationEvidence::from(evidence.clone());
    assert_eq!(
        lash_core::facade_support::TurnCancellationEvidence::from(remote_evidence),
        evidence
    );
    let evidence_without_origin = lash_core::facade_support::TurnCancellationEvidence {
        request_id: "cancel-without-origin".to_string(),
        origin: None,
        reason: None,
        undelivered: lash_core::facade_support::TurnCancelDisposition::Defer,
        mode: lash_core::facade_support::TurnCancelMode::Immediate,
        honoured_after_step: None,
    };
    let remote_evidence = RemoteTurnCancellationEvidence::from(evidence_without_origin.clone());
    assert_eq!(
        lash_core::facade_support::TurnCancellationEvidence::from(remote_evidence),
        evidence_without_origin
    );

    for core_outcome in [
        lash_core::facade_support::TurnCancelOutcome::Requested(evidence.clone()),
        lash_core::facade_support::TurnCancelOutcome::AlreadyRequested(evidence.clone()),
        lash_core::facade_support::TurnCancelOutcome::PolicyConflict {
            requested: lash_core::facade_support::TurnCancelDisposition::Drop,
            accepted: evidence.clone(),
        },
        lash_core::facade_support::TurnCancelOutcome::CompletionWonRace,
        lash_core::facade_support::TurnCancelOutcome::UnknownOrRevoked,
    ] {
        let remote = RemoteTurnCancelOutcome::from(core_outcome.clone());
        let round_trip = lash_core::facade_support::TurnCancelOutcome::from(remote);
        assert_eq!(round_trip, core_outcome);
    }
}
