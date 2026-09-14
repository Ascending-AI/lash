//! Wire conformance for the context-overflow turn stop (FIG-1272).

use super::*;

/// The context-overflow stop is its own wire tag, distinct from
/// `provider_error`, and it crosses the protocol under the current version.
#[test]
fn remote_context_overflow_stop_is_its_own_wire_tag() {
    let stop = RemoteTurnStop::ContextOverflow;
    let wire = serde_json::to_value(&stop).unwrap();
    assert_eq!(wire, serde_json::json!({"type": "context_overflow"}));
    assert_eq!(
        serde_json::from_value::<RemoteTurnStop>(wire).unwrap(),
        stop
    );
    assert_ne!(stop, RemoteTurnStop::ProviderError);

    let outcome = RemoteTurnOutcome::Stopped { stop };
    assert_eq!(RemoteTurnStatus::from(&outcome), RemoteTurnStatus::Failed);

    // A peer that does not speak this generation has no tolerant-decode arm:
    // the envelope version refuses it before the body is read.
    let body = serde_json::to_value(&outcome).unwrap();
    let stale = serde_json::json!({
        "protocol_version": REMOTE_PROTOCOL_VERSION - 1,
        "outcome": body,
    });
    #[derive(serde::Deserialize)]
    struct OutcomeBody {
        #[allow(dead_code)]
        outcome: RemoteTurnOutcome,
    }
    assert!(matches!(
        Envelope::<OutcomeBody>::decode_json(stale.to_string().as_bytes()),
        Err(RemoteProtocolError::UnsupportedProtocolVersion {
            actual,
            expected: REMOTE_PROTOCOL_VERSION,
        }) if actual == REMOTE_PROTOCOL_VERSION - 1
    ));
}
