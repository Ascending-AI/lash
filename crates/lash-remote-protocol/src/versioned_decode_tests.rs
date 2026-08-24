use super::*;

#[test]
fn observation_decode_checks_version_before_unknown_payload_tag() {
    let wire = serde_json::json!({
        "protocol_version": REMOTE_PROTOCOL_VERSION + 1,
        "session_id": "future-session",
        "replay_incarnation_id": "future-incarnation",
        "revision": 1,
        "cursor": "future-cursor",
        "type": "future_observation",
    });

    let error = RemoteSessionObservationEvent::decode_json(wire.to_string().as_bytes())
        .expect_err("newer observation payload must be refused");
    assert!(
        matches!(
            error,
            RemoteProtocolError::UnsupportedProtocolVersion { actual, expected }
                if actual == REMOTE_PROTOCOL_VERSION + 1 && expected == REMOTE_PROTOCOL_VERSION
        ),
        "{error:?}"
    );
}

#[test]
fn observation_decode_reports_unknown_current_payload_tag_as_message_decode_failure() {
    let wire = serde_json::json!({
        "protocol_version": REMOTE_PROTOCOL_VERSION,
        "session_id": "current-session",
        "replay_incarnation_id": "current-incarnation",
        "revision": 1,
        "cursor": "current-cursor",
        "type": "unknown_observation",
    });

    assert!(matches!(
        RemoteSessionObservationEvent::decode_json(wire.to_string().as_bytes()),
        Err(RemoteProtocolError::MessageDecode(_))
    ));
}

#[test]
fn turn_report_decode_checks_version_before_unknown_payload_tag() {
    let wire = serde_json::json!({
        "protocol_version": REMOTE_PROTOCOL_VERSION + 1,
        "session_id": "future-session",
        "turn_id": "future-turn",
        "status": "future_status",
        "outcome": {
            "type": "future_outcome"
        },
        "assistant_output": {},
    });

    let error = RemoteTurnReport::decode_json(wire.to_string().as_bytes())
        .expect_err("newer turn-report payload must be refused");
    assert!(
        matches!(
            error,
            RemoteProtocolError::UnsupportedProtocolVersion { actual, expected }
                if actual == REMOTE_PROTOCOL_VERSION + 1 && expected == REMOTE_PROTOCOL_VERSION
        ),
        "{error:?}"
    );
}

#[test]
fn turn_report_decode_reports_unknown_current_payload_tag_as_message_decode_failure() {
    let wire = serde_json::json!({
        "protocol_version": REMOTE_PROTOCOL_VERSION,
        "session_id": "current-session",
        "turn_id": "current-turn",
        "status": "future_status",
        "outcome": {
            "type": "future_outcome"
        },
        "assistant_output": {},
    });

    assert!(matches!(
        RemoteTurnReport::decode_json(wire.to_string().as_bytes()),
        Err(RemoteProtocolError::MessageDecode(_))
    ));
}
