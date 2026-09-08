use super::*;

#[test]
fn cancelled_stop_conversion_keeps_every_evidence_field() {
    let core = lash_core::facade_support::TurnCancellationEvidence {
        request_id: "cancel-exact".into(),
        origin: Some("host".into()),
        reason: Some("stop now".into()),
        undelivered: lash_core::facade_support::TurnCancelDisposition::Defer,
    };
    let expected = RemoteTurnCancellationEvidence::from(core.clone());
    let stop =
        RemoteTurnStop::from(lash_core::facade_support::TurnStop::Cancelled { evidence: core });
    assert_eq!(stop, RemoteTurnStop::Cancelled { evidence: expected });
}
