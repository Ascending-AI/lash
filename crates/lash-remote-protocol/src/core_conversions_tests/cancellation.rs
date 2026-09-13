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

#[test]
fn every_cancellation_origin_survives_record_observation_and_output_transport() {
    for origin in [
        lash_sansio::CancelOrigin::TurnStopped,
        lash_sansio::CancelOrigin::ParentEnded,
        lash_sansio::CancelOrigin::OperatorRequested,
        lash_sansio::CancelOrigin::ModelRequested,
        lash_sansio::CancelOrigin::StartFailed,
    ] {
        let request = lash_sansio::CancelRequest::new(origin, "actor:transport-law", 11);
        let mut record = process_record(&ProcessId::from("typed-cancel-transport"));
        assert!(record.cancel_request.is_none());
        record.cancel_request = Some(Box::new(request.clone()));
        record.status = lash_core::ProcessStatus::Cancelled;
        record.outcome = Some(lash_core::ProcessAwaitOutput::from_tool_output(
            lash_core::ToolCallOutput::cancelled(
                lash_core::ToolCancellation::runtime("runner settled").with_origin(origin),
            ),
        ));
        let remote = RemoteProcessRecord::try_from(record.clone()).expect("encode record");
        assert_eq!(remote.cancel_request, Some(request.clone()));
        let returned = lash_core::ProcessRecord::try_from(remote).expect("decode record");
        assert_eq!(returned.cancel_request.as_deref(), Some(&request));
        assert!(matches!(returned.outcome,
            Some(lash_core::ProcessAwaitOutput::Settled { output })
                if matches!(&output.outcome, lash_core::ToolCallOutcome::Cancelled(cancellation)
                    if cancellation.origin == Some(origin))
        ));

        let mut observed = observed_process();
        assert!(observed.cancel_request.is_none());
        observed.cancel_request = Some(request.clone());
        let remote = RemoteObservedProcess::try_from(observed).expect("encode observation");
        assert_eq!(remote.cancel_request, Some(request.clone()));
        let returned = lash_core::facade_support::ObservedProcess::try_from(remote)
            .expect("decode observation");
        assert_eq!(returned.cancel_request, Some(request));
    }
}
