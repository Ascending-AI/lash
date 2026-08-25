use super::*;

fn assert_streamed_envelope_contract<T>(
    body: T,
    encode: impl Fn(&T) -> Result<Vec<u8>, serde_json::Error>,
    decode: impl Fn(&[u8]) -> Result<T, RemoteProtocolError>,
) where
    T: std::fmt::Debug + PartialEq,
{
    let wire = encode(&body).expect("streamed envelope encodes");
    assert_eq!(
        std::str::from_utf8(&wire)
            .expect("envelope utf-8")
            .matches("\"protocol_version\"")
            .count(),
        1,
        "each wire message carries exactly one protocol version"
    );
    let value: serde_json::Value = serde_json::from_slice(&wire).expect("envelope json");
    assert_eq!(
        value["protocol_version"],
        serde_json::json!(REMOTE_PROTOCOL_VERSION)
    );
    assert_eq!(decode(&wire).expect("streamed envelope decodes"), body);

    let mut wrong_version = value;
    wrong_version["protocol_version"] = serde_json::json!(REMOTE_PROTOCOL_VERSION + 1);
    let error =
        decode(&serde_json::to_vec(&wrong_version).expect("wrong-version envelope serializes"))
            .expect_err("wrong-version envelope is refused");
    assert!(matches!(
        error,
        RemoteProtocolError::UnsupportedProtocolVersion { actual, expected }
            if actual == REMOTE_PROTOCOL_VERSION + 1 && expected == REMOTE_PROTOCOL_VERSION
    ));
}

fn streamed_llm_request() -> RemoteLlmRequest {
    RemoteLlmRequest {
        request_id: "request-stream".to_string(),
        scope: RemoteLlmRequestScope::new(
            "session-stream",
            "session-stream:frame:root",
            "request-stream",
        ),
        model_intent: RemoteModelIntent::new("model-stream"),
        messages: Vec::new(),
        attachments: Vec::new(),
        tools: Vec::new(),
        tool_choice: RemoteLlmToolChoice::Auto,
        output_spec: None,
        generation: RemoteGenerationOptions::default(),
        metadata: std::collections::HashMap::new(),
    }
}

fn streamed_turn_input() -> RemoteTurnInput {
    RemoteTurnInput::text("hello")
}

fn streamed_turn_request() -> RemoteTurnRequest {
    RemoteTurnRequest {
        session_id: "session-stream".to_string(),
        turn_id: "turn-stream".to_string(),
        idempotency_key: None,
        input: streamed_turn_input(),
        tool_grants: Vec::new(),
        metadata: std::collections::HashMap::new(),
    }
}

fn streamed_activity() -> RemoteTurnActivity {
    RemoteTurnActivity {
        sequence: 3,
        id: "activity-stream".to_string(),
        correlation_id: "correlation-stream".to_string(),
        event: RemoteTurnEvent::AssistantProseDelta {
            text: "hello".to_string(),
        },
    }
}

fn streamed_turn_report() -> RemoteTurnReport {
    RemoteTurnReport {
        session_id: "session-stream".to_string(),
        turn_id: "turn-stream".to_string(),
        status: RemoteTurnStatus::Completed,
        outcome: RemoteTurnOutcome::Finished {
            finish: RemoteTurnFinish::AssistantMessage {
                text: "done".to_string(),
            },
        },
        cancellation: None,
        assistant_output: RemoteAssistantOutput::default(),
        usage: RemoteTurnUsageReport::default(),
        execution: RemoteTurnExecutionMetrics::default(),
        tool_calls: Vec::new(),
        llm_calls: Vec::new(),
        issues: Vec::new(),
        activities: vec![streamed_activity()],
        metadata: std::collections::HashMap::new(),
    }
}

fn streamed_observation_event() -> RemoteSessionObservationEvent {
    RemoteSessionObservationEvent {
        session_id: "session-stream".to_string(),
        replay_incarnation_id: "incarnation-stream".to_string(),
        turn_id: Some("turn-stream".to_string()),
        revision: 5,
        cursor: "cursor-stream".to_string(),
        event: RemoteSessionObservationEventPayload::TurnActivity {
            activity: Box::new(streamed_activity()),
        },
    }
}

#[test]
fn streamed_llm_request_round_trips_and_gates_version() {
    assert_streamed_envelope_contract(
        streamed_llm_request(),
        RemoteLlmRequest::encode_json,
        RemoteLlmRequest::decode_json,
    );
}

#[test]
fn streamed_turn_input_round_trips_and_gates_version() {
    assert_streamed_envelope_contract(
        streamed_turn_input(),
        RemoteTurnInput::encode_json,
        RemoteTurnInput::decode_json,
    );
}

#[test]
fn streamed_turn_request_round_trips_and_gates_version() {
    assert_streamed_envelope_contract(
        streamed_turn_request(),
        RemoteTurnRequest::encode_json,
        RemoteTurnRequest::decode_json,
    );
}

#[test]
fn streamed_activity_round_trips_and_gates_version() {
    assert_streamed_envelope_contract(
        streamed_activity(),
        RemoteTurnActivity::encode_json,
        RemoteTurnActivity::decode_json,
    );
}

#[test]
fn streamed_turn_report_round_trips_and_gates_version() {
    assert_streamed_envelope_contract(
        streamed_turn_report(),
        RemoteTurnReport::encode_json,
        RemoteTurnReport::decode_json,
    );
}

#[test]
fn streamed_observation_event_round_trips_and_gates_version() {
    assert_streamed_envelope_contract(
        streamed_observation_event(),
        RemoteSessionObservationEvent::encode_json,
        RemoteSessionObservationEvent::decode_json,
    );
}

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
