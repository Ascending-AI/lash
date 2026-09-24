use super::*;

#[derive(serde::Deserialize)]
struct EmptyEnvelopeBody {}

pub(super) fn decode_empty_envelope(protocol_version: u32) -> Result<(), RemoteProtocolError> {
    let wire = serde_json::json!({ "protocol_version": protocol_version }).to_string();
    Envelope::<EmptyEnvelopeBody>::decode_json(wire.as_bytes()).map(drop)
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
fn historical_remote_protocol_generation_64_is_refused() {
    const PREDECESSOR: u32 = 64;
    assert_eq!(PREDECESSOR + 1, 65, "historical generation adjacency pin");
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

/// Captured by main's Envelope writer at 9680a9bd87bd; no hand-edited wire bytes.
#[test]
fn historical_remote_protocol_generation_65_is_refused() {
    const PREDECESSOR: u32 = 65;
    assert_eq!(PREDECESSOR + 1, 66, "historical generation adjacency pin");
    let bytes = include_bytes!("../../tests/fixtures/remote-envelope-v65.json");
    let predecessor: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(predecessor["protocol_version"], PREDECESSOR);
    let error = match Envelope::<EmptyEnvelopeBody>::decode_json(bytes) {
        Ok(_) => panic!("generation-65 remote envelope must be refused"),
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

/// Captured by main's Envelope writer at 24736fac5; no hand-edited wire bytes.
#[test]
fn historical_remote_protocol_generation_67_is_refused() {
    const PREDECESSOR: u32 = 67;
    let bytes = include_bytes!("../../tests/fixtures/remote-envelope-v67.json");
    let predecessor: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(predecessor["protocol_version"], PREDECESSOR);
    let error = match Envelope::<EmptyEnvelopeBody>::decode_json(bytes) {
        Ok(_) => panic!("generation-67 remote envelope must be refused"),
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
                Err(RemoteProtocolError::UnsupportedProtocolVersion {
                    actual,
                    expected: REMOTE_PROTOCOL_VERSION,
                }) if actual == predecessor
            ),
            "generation {predecessor} must be refused"
        );
    }
}
