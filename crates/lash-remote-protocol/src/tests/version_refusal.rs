use super::*;

#[derive(serde::Deserialize)]
struct EmptyEnvelopeBody {}

pub(super) fn decode_empty_envelope(protocol_version: u32) -> Result<(), RemoteProtocolError> {
    let wire = serde_json::json!({ "protocol_version": protocol_version }).to_string();
    Envelope::<EmptyEnvelopeBody>::decode_json(wire.as_bytes(), crate::REMOTE_PROTOCOL).map(drop)
}

/// Exact-match negotiation accepts one number and refuses every other, landed
/// or not: 33 is the pre-suppression-rename generation, 60 is generation 61's
/// landed predecessor (FIG-1123), windows 68-70 are claimed by bump members
/// that never landed here, and 72-73 are the two generations that window
/// replaced.
#[test]
fn retired_and_unlanded_remote_protocol_generations_are_refused() {
    for predecessor in [33, 60, 68, 69, 70, 72, 73] {
        assert!(
            matches!(
                decode_empty_envelope(predecessor),
                Err(RemoteProtocolError::Unsupported { peer, local })
                    if peer == crate::VersionRange::exactly(predecessor)
                        && local == crate::REMOTE_PROTOCOL
            ),
            "generation {predecessor} must be refused"
        );
    }
}
