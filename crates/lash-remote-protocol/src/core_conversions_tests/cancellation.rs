use super::*;

#[test]
fn cancelled_stop_conversion_keeps_every_evidence_field() {
    let core = lash_core::facade_support::TurnCancellationEvidence {
        request_id: "cancel-exact".into(),
        origin: Some("host".into()),
        reason: Some("stop now".into()),
        undelivered: lash_core::facade_support::TurnCancelUndeliveredInputPolicy::Defer,
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
fn remote_cancel_roundtrip_preserves_all_modes_and_checkpoint_evidence() {
    use lash_core::facade_support::{
        TurnAddress, TurnCancelMode, TurnCancelOutcome, TurnCancelRequest,
        TurnCancelUndeliveredInputPolicy, TurnCancellationEvidence, TurnStop,
    };

    fn wire_roundtrip<T: serde::Serialize + serde::de::DeserializeOwned>(value: T) -> T {
        serde_json::from_slice(&serde_json::to_vec(&value).expect("encode cancellation"))
            .expect("decode cancellation")
    }

    let mut losses = Vec::new();
    for mode in [TurnCancelMode::Immediate, TurnCancelMode::AfterStep] {
        for undelivered in [
            TurnCancelUndeliveredInputPolicy::Defer,
            TurnCancelUndeliveredInputPolicy::Drop,
        ] {
            for origin in [None, Some("host-origin".to_string())] {
                let request = TurnCancelRequest::new(
                    TurnAddress::new("session", "turn"),
                    "cancel-request",
                    origin.clone(),
                )
                .with_reason("stop at the requested boundary")
                .undelivered(undelivered)
                .mode(mode);
                let remote = wire_roundtrip(RemoteTurnCancelRequest::from(request.clone()));
                remote.validate().expect("valid cancellation request");
                let actual = remote.try_into_core().expect("core cancellation request");
                if actual != request {
                    losses.push(format!("request: expected {request:?}, got {actual:?}"));
                }

                for honoured_after_step in [None, Some(0), Some(7)] {
                    let evidence = TurnCancellationEvidence {
                        request_id: request.request_id.clone(),
                        origin: origin.clone(),
                        reason: request.reason.clone(),
                        undelivered,
                        mode,
                        honoured_after_step,
                    };
                    let remote =
                        wire_roundtrip(RemoteTurnCancellationEvidence::from(evidence.clone()));
                    remote.validate().expect("valid cancellation evidence");
                    let actual = TurnCancellationEvidence::from(remote);
                    if actual != evidence {
                        losses.push(format!("evidence: expected {evidence:?}, got {actual:?}"));
                    }
                    let stop = TurnStop::Cancelled {
                        evidence: evidence.clone(),
                    };
                    let RemoteTurnStop::Cancelled { evidence: decoded } =
                        wire_roundtrip(RemoteTurnStop::from(stop.clone()))
                    else {
                        panic!("cancellation changed the terminal stop variant");
                    };
                    let actual = TurnStop::Cancelled {
                        evidence: decoded.into(),
                    };
                    if actual != stop {
                        losses.push(format!("terminal: expected {stop:?}, got {actual:?}"));
                    }
                    for outcome in [
                        TurnCancelOutcome::Requested(evidence.clone()),
                        TurnCancelOutcome::AlreadyRequested(evidence.clone()),
                        TurnCancelOutcome::Escalated(evidence.clone()),
                        TurnCancelOutcome::PolicyConflict {
                            requested: TurnCancelUndeliveredInputPolicy::Defer,
                            accepted: evidence.clone(),
                        },
                        TurnCancelOutcome::PolicyConflict {
                            requested: TurnCancelUndeliveredInputPolicy::Drop,
                            accepted: evidence.clone(),
                        },
                        TurnCancelOutcome::CompletionWonRace,
                        TurnCancelOutcome::UnknownOrRevoked,
                    ] {
                        let receipt = wire_roundtrip(RemoteTurnCancelReceipt::new(
                            "session",
                            "turn",
                            RemoteTurnCancelOutcome::from(outcome.clone()),
                        ));
                        receipt.validate().expect("valid cancellation receipt");
                        let actual = TurnCancelOutcome::from(receipt.outcome);
                        if actual != outcome {
                            losses.push(format!("outcome: expected {outcome:?}, got {actual:?}"));
                        }
                    }
                }
            }
        }
    }
    assert!(
        losses.is_empty(),
        "remote cancellation lost fields:\n{}",
        losses.join("\n")
    );
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
        let mut record = process_record(&lash_sansio::ProcessId::fixture("typed-cancel-transport"));
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

/// A context-overflow stop crosses the core boundary as its own remote
/// variant; it must not collapse back into `ProviderError` (FIG-1272).
#[test]
fn context_overflow_stop_converts_to_its_own_remote_variant() {
    assert_eq!(
        RemoteTurnStop::from(lash_core::facade_support::TurnStop::ContextOverflow),
        RemoteTurnStop::ContextOverflow
    );
    assert_eq!(
        RemoteTurnStop::from(lash_core::facade_support::TurnStop::ProviderError),
        RemoteTurnStop::ProviderError
    );
}
