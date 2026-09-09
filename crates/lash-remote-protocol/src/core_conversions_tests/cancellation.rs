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
