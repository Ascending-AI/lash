use super::*;

#[derive(serde::Deserialize)]
struct EmptyEnvelopeBody {}

pub(super) fn decode_empty_envelope(protocol_version: u32) -> Result<(), RemoteProtocolError> {
    let wire = serde_json::json!({ "protocol_version": protocol_version }).to_string();
    Envelope::<EmptyEnvelopeBody>::decode_json(wire.as_bytes()).map(drop)
}

/// Refusal witness (FIG-1123): the generation-61 decoder rejects its immediate
/// predecessor before attempting to decode the envelope body.
#[test]
fn historical_remote_protocol_generation_60_is_refused() {
    const PREDECESSOR: u32 = 60;
    assert_eq!(PREDECESSOR + 1, 61, "historical generation adjacency pin");
    let error = decode_empty_envelope(PREDECESSOR)
        .expect_err("generation-60 remote envelope must be refused");
    assert!(matches!(
        error,
        RemoteProtocolError::UnsupportedProtocolVersion {
            actual: PREDECESSOR,
            expected: REMOTE_PROTOCOL_VERSION,
        }
    ));
}

/// Captured by main's Envelope writer at 11f6b0eb40f6; no hand-edited wire bytes.
#[test]
fn historical_remote_protocol_generation_61_is_refused() {
    let bytes = include_bytes!("../../tests/fixtures/remote-envelope-v61.json");
    let predecessor: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(predecessor["protocol_version"], 61);
    assert_eq!(61 + 1, 62, "historical generation adjacency pin");
    assert!(matches!(
        Envelope::<EmptyEnvelopeBody>::decode_json(bytes),
        Err(RemoteProtocolError::UnsupportedProtocolVersion {
            actual: 61,
            expected: REMOTE_PROTOCOL_VERSION
        })
    ));
}

/// Captured by main's Envelope writer at 847ba3b0b342; no hand-edited wire bytes.
#[test]
fn immediate_predecessor_remote_protocol_generation_62_is_refused() {
    let bytes = include_bytes!("../../tests/fixtures/remote-envelope-v62.json");
    let predecessor: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(predecessor["protocol_version"], 62);
    assert_eq!(62 + 1, 63, "historical generation adjacency pin");
    assert!(matches!(
        Envelope::<EmptyEnvelopeBody>::decode_json(bytes),
        Err(RemoteProtocolError::UnsupportedProtocolVersion {
            actual: 62,
            expected: REMOTE_PROTOCOL_VERSION
        })
    ));
}

/// Captured by main's Envelope writer at 02339d7999b4; no hand-edited wire bytes.
#[test]
fn historical_remote_protocol_generation_63_is_refused() {
    const PREDECESSOR: u32 = 63;
    assert_eq!(PREDECESSOR + 1, 64, "historical generation adjacency pin");
    let bytes = include_bytes!("../../tests/fixtures/remote-envelope-v63.json");
    let predecessor: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(predecessor["protocol_version"], PREDECESSOR);
    let error = match Envelope::<EmptyEnvelopeBody>::decode_json(bytes) {
        Ok(_) => panic!("generation-63 remote envelope must be refused"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        RemoteProtocolError::UnsupportedProtocolVersion {
            actual: PREDECESSOR,
            expected: REMOTE_PROTOCOL_VERSION,
        }
    ));
}

/// Captured by the merged-main Envelope writer at 9bd3d967e05a; no hand-edited wire bytes.
#[test]
fn immediate_predecessor_remote_protocol_generation_64_is_refused() {
    const PREDECESSOR: u32 = 64;
    assert_eq!(
        PREDECESSOR + 1,
        REMOTE_PROTOCOL_VERSION,
        "remote-protocol generation adjacency pin"
    );
    let bytes = include_bytes!("../../tests/fixtures/remote-envelope-v64.json");
    let predecessor: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(predecessor["protocol_version"], PREDECESSOR);
    let error = match Envelope::<EmptyEnvelopeBody>::decode_json(bytes) {
        Ok(_) => panic!("generation-64 remote envelope must be refused"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        RemoteProtocolError::UnsupportedProtocolVersion {
            actual: PREDECESSOR,
            expected: REMOTE_PROTOCOL_VERSION,
        }
    ));
}

#[test]
fn pre_suppression_rename_remote_protocol_is_rejected_with_literal_versions() {
    assert!(matches!(
        decode_empty_envelope(33),
        Err(RemoteProtocolError::UnsupportedProtocolVersion {
            actual: 33,
            expected: 65,
        })
    ));
}
