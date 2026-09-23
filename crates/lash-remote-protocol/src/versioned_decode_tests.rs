use super::*;
use lash_sansio::SessionId;
use lash_sansio::TurnId;

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
        instructions: None,
        request_id: "request-stream".to_string(),
        scope: RemoteLlmRequestScope::new(
            "session-stream",
            "session-stream:frame:root",
            "request-stream",
        ),
        model_intent: RemoteModelIntent::new("model-stream"),
        messages: Vec::new(),
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
        session_id: SessionId::from("session-stream"),
        turn_id: TurnId::from("turn-stream"),
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
            block: lash_sansio::llm::types::StreamBlockIdentity::new("text:0", 0),
        },
    }
}

fn streamed_turn_report() -> RemoteTurnReport {
    RemoteTurnReport {
        session_id: SessionId::from("session-stream"),
        turn_id: TurnId::from("turn-stream"),
        outcome: RemoteTurnOutcome::Finished {
            finish: RemoteTurnFinish::AssistantMessage {
                text: "done".to_string(),
            },
        },
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
        session_id: SessionId::from("session-stream"),
        replay_incarnation_id: "incarnation-stream".to_string(),
        turn_id: Some(TurnId::from("turn-stream")),
        revision: 5,
        cursor: "cursor-stream".to_string(),
        event: RemoteSessionObservationEventPayload::TurnActivity {
            activity: Box::new(streamed_activity()),
        },
    }
}

fn process_node_record() -> lash_trace::TraceRecord {
    lash_trace::TraceRecord::new(
        lash_trace::TraceContext::default(),
        lash_trace::TraceEvent::LanguageExecution {
            language: "lashlang".to_string(),
            event: lash_trace::TraceLanguageExecution {
                event_key: "process:wire:node:1:started".to_string(),
                identity: lash_trace::TraceLanguageExecutionIdentity {
                    scope: lash_trace::TraceRuntimeScope::none(),
                    subject: lash_trace::TraceRuntimeSubject::Process {
                        process_id: lash_sansio::ProcessId::from("process:wire"),
                    },
                    source_identity: "source".to_string(),
                    module_ref: "module".to_string(),
                    entry_kind: "main".to_string(),
                    entry_ref: None,
                    entry_name: "main".to_string(),
                    restate_invocation_id: None,
                    generation: Some(lash_trace::TraceLanguageExecutionGeneration::new(1, 1)),
                },
                payload: lash_trace::TraceLanguageExecutionPayload::NodeStarted {
                    node_id: "node".to_string(),
                    node_kind: lash_sansio::ExecutionNodeKind::Call,
                    label: "call()".to_string(),
                    occurrence: 1,
                    call_id: None,
                },
            },
        },
    )
}

/// The published observation-item schema: the snapshot graph and the node
/// event record are typed trace shapes, not opaque JSON.
fn published_observation_item_schema() -> jsonschema::JSONSchema {
    let schema: serde_json::Value = serde_json::from_str(include_str!(
        "../../../schemas/host/remote-process-observation-item/v91.schema.json"
    ))
    .expect("published observation item schema parses");
    assert_eq!(
        schema["x-lash-schema-version"],
        serde_json::json!(REMOTE_PROTOCOL_VERSION)
    );
    jsonschema::JSONSchema::compile(&schema).expect("published observation item schema compiles")
}

fn assert_process_observation_wire_contract(item: RemoteProcessObservationItem) {
    let wire = item.encode_json().expect("encode observation item");
    assert_eq!(
        RemoteProcessObservationItem::decode_json(&wire).expect("decode item"),
        item
    );
    let validator = published_observation_item_schema();
    let mut body: serde_json::Value = serde_json::from_slice(&wire).expect("wire json");
    body.as_object_mut()
        .expect("item object")
        .remove("protocol_version");
    if let Err(errors) = validator.validate(&body) {
        panic!(
            "published schema rejected a real observation item:\n{}",
            errors
                .map(|error| format!("{} at {}", error, error.instance_path))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    let mut untyped = body.clone();
    let typed_field = match &item {
        RemoteProcessObservationItem::Snapshot { .. } => Some("/projection/graph/status"),
        RemoteProcessObservationItem::Event { .. } => Some("/record/type"),
        RemoteProcessObservationItem::Gap { .. } => None,
    };
    if let Some(pointer) = typed_field {
        *untyped.pointer_mut(pointer).expect("typed trace field") =
            serde_json::json!("future_variant");
        assert!(
            !validator.is_valid(&untyped),
            "the published schema types {pointer} as a closed trace enum"
        );
    }
    let mut extra = body;
    extra["retired_field"] = serde_json::json!(true);
    assert!(
        !validator.is_valid(&extra),
        "item schema refuses unknown fields"
    );
    let mut value: serde_json::Value = serde_json::from_slice(&wire).expect("wire json");
    value["protocol_version"] = serde_json::json!(REMOTE_PROTOCOL_VERSION - 1);
    value["type"] = serde_json::json!("unknown_future_item");
    assert!(matches!(
        RemoteProcessObservationItem::decode_json(value.to_string().as_bytes()),
        Err(RemoteProtocolError::UnsupportedProtocolVersion { actual, expected })
            if actual == REMOTE_PROTOCOL_VERSION - 1 && expected == REMOTE_PROTOCOL_VERSION
    ));
    value["protocol_version"] = serde_json::json!(REMOTE_PROTOCOL_VERSION);
    assert!(RemoteProcessObservationItem::decode_json(value.to_string().as_bytes()).is_err());
    let mut value: serde_json::Value = serde_json::from_slice(&wire).expect("wire json");
    value["retired_field"] = serde_json::json!(true);
    assert!(RemoteProcessObservationItem::decode_json(value.to_string().as_bytes()).is_err());
}

#[test]
fn process_observation_cursor_wire_contract() {
    let request = RemoteProcessObservationRequest {
        process_id: lash_sansio::ProcessId::from("process:wire"),
        incarnation: 1,
        cursor: Some("lashpc1:epoch:1:1:process:wire".to_string()),
    };
    let wire = request.encode_json().expect("request wire");
    assert_eq!(
        RemoteProcessObservationRequest::decode_json(&wire).expect("request"),
        request
    );
    let mut value: serde_json::Value = serde_json::from_slice(&wire).expect("wire json");
    value["protocol_version"] = serde_json::json!(REMOTE_PROTOCOL_VERSION - 1);
    value["retired_cursor"] = serde_json::json!("old");
    assert!(matches!(
        RemoteProcessObservationRequest::decode_json(value.to_string().as_bytes()),
        Err(RemoteProtocolError::UnsupportedProtocolVersion { .. })
    ));
    value["protocol_version"] = serde_json::json!(REMOTE_PROTOCOL_VERSION);
    assert!(RemoteProcessObservationRequest::decode_json(value.to_string().as_bytes()).is_err());
}

#[test]
fn process_observation_snapshot_wire_contract() {
    let record = process_node_record();
    let graph = lash_trace::TraceLashlangGraphStore::fold(None, &[record]).expect("graph");
    assert_process_observation_wire_contract(RemoteProcessObservationItem::Snapshot {
        process_id: lash_sansio::ProcessId::from("process:wire"),
        incarnation: 1,
        cursor: "lashpc1:epoch:1:1:process:wire".to_string(),
        projection: RemoteProcessObservationProjection {
            graph: Some(graph),
            completeness: RemoteProcessObservationCompleteness::Complete,
        },
    });
}

#[test]
fn process_observation_event_wire_contract() {
    assert_process_observation_wire_contract(RemoteProcessObservationItem::Event {
        process_id: lash_sansio::ProcessId::from("process:wire"),
        incarnation: 1,
        cursor: "lashpc1:epoch:1:1:process:wire".to_string(),
        record: Box::new(process_node_record()),
    });
    let foreign = RemoteProcessObservationItem::Event {
        process_id: lash_sansio::ProcessId::from("process:other"),
        incarnation: 1,
        cursor: "lashpc1:epoch:1:1:process:other".to_string(),
        record: Box::new(process_node_record()),
    };
    assert!(
        RemoteProcessObservationItem::decode_json(
            &foreign.encode_json().expect("foreign event wire")
        )
        .is_err()
    );
}

#[test]
fn process_observation_gap_wire_contract() {
    for reason in [
        RemoteProcessObservationGapReason::Overflow,
        RemoteProcessObservationGapReason::Expired,
        RemoteProcessObservationGapReason::SubscriberLagged,
        RemoteProcessObservationGapReason::PublisherReplaced,
        RemoteProcessObservationGapReason::RoutingUnavailable,
        RemoteProcessObservationGapReason::ProcessIdReused,
        RemoteProcessObservationGapReason::CrossProcess,
        RemoteProcessObservationGapReason::InvalidCursor,
    ] {
        assert_process_observation_wire_contract(RemoteProcessObservationItem::Gap {
            process_id: lash_sansio::ProcessId::from("process:wire"),
            incarnation: 1,
            requested_cursor: Some("lashpc1:old:1:0:process:wire".to_string()),
            latest_cursor: None,
            projection: RemoteProcessObservationProjection {
                graph: None,
                completeness: RemoteProcessObservationCompleteness::Incomplete { reason },
            },
            reason,
        });
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

#[test]
fn paged_process_events_wire_contract() {
    let request = RemoteProcessEventsRequest {
        process_id: lash_sansio::ProcessId::from("process:wire"),
        incarnation: 1,
        limit: std::num::NonZeroUsize::new(2).expect("nonzero limit"),
        mode: lash_core::ProcessEventQueryMode::Lite,
        continuation: None,
    };
    let wire = request.encode_json().expect("request wire");
    assert_eq!(
        RemoteProcessEventsRequest::decode_json(&wire).expect("request"),
        request
    );
    let mut value: serde_json::Value = serde_json::from_slice(&wire).expect("wire json");
    value["protocol_version"] = serde_json::json!(REMOTE_PROTOCOL_VERSION - 1);
    value["mode"] = serde_json::json!("future_mode");
    assert!(matches!(
        RemoteProcessEventsRequest::decode_json(value.to_string().as_bytes()),
        Err(RemoteProtocolError::UnsupportedProtocolVersion { .. })
    ));
    value["protocol_version"] = serde_json::json!(REMOTE_PROTOCOL_VERSION);
    assert!(RemoteProcessEventsRequest::decode_json(value.to_string().as_bytes()).is_err());
    value["mode"] = serde_json::json!("lite");
    value["after_sequence"] = serde_json::json!(0);
    assert!(RemoteProcessEventsRequest::decode_json(value.to_string().as_bytes()).is_err());

    let response = RemoteProcessEventsResponse {
        process_id: request.process_id.clone(),
        incarnation: 1,
        outcome: lash_core::ProcessEventReadOutcome::Retained(lash_core::ProcessEventPage {
            events: lash_core::ProcessEventPageEvents::Lite(vec![lash_core::ProcessEventLite {
                sequence: 4,
                event_type: "process.waiting".to_string(),
            }]),
            more: lash_core::ProcessEventPageMore::Complete,
        }),
    };
    let wire = response.encode_json().expect("response wire");
    assert_eq!(
        RemoteProcessEventsResponse::decode_json(&wire).expect("response"),
        response
    );
    let mut value: serde_json::Value = serde_json::from_slice(&wire).expect("wire json");
    assert_eq!(
        value["outcome"]["value"]["events"]["events"][0]["event_type"],
        "process.waiting",
    );
    value["outcome"]["value"]["events"]["events"][0]["payload"] = serde_json::json!(null);
    assert!(
        RemoteProcessEventsResponse::decode_json(value.to_string().as_bytes()).is_err(),
        "lite event must refuse a payload field"
    );
    let retention = RemoteProcessEventsResponse {
        process_id: request.process_id,
        incarnation: 1,
        outcome: lash_core::ProcessEventReadOutcome::NoLongerRetained(
            lash_core::ProcessEventHistoryRetention::Pruned {
                terminal_label: "completed".to_string(),
                pruned_at_ms: 42,
            },
        ),
    };
    let wire = retention.encode_json().expect("retention wire");
    assert_eq!(
        RemoteProcessEventsResponse::decode_json(&wire).expect("retention"),
        retention
    );

    let token_payload = serde_json::json!({
        "process_id": "process:wire",
        "process_incarnation": 1,
        "after_sequence": 4,
        "mode": "lite",
    });
    let encoded = token_payload
        .to_string()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let token: lash_core::ProcessEventPageToken = serde_json::from_value(serde_json::json!(
        format!("process-event-page:v1:{encoded}")
    ))
    .expect("bound continuation token");
    let more = RemoteProcessEventsResponse {
        process_id: lash_sansio::ProcessId::from("process:wire"),
        incarnation: 1,
        outcome: lash_core::ProcessEventReadOutcome::Retained(lash_core::ProcessEventPage {
            events: lash_core::ProcessEventPageEvents::Lite(vec![lash_core::ProcessEventLite {
                sequence: 4,
                event_type: "process.waiting".to_string(),
            }]),
            more: lash_core::ProcessEventPageMore::More {
                continuation: token.clone(),
            },
        }),
    };
    let wire = more.encode_json().expect("continuation response wire");
    assert_eq!(
        RemoteProcessEventsResponse::decode_json(&wire).expect("continuation"),
        more
    );
    let continued = RemoteProcessEventsRequest {
        process_id: more.process_id.clone(),
        incarnation: 1,
        limit: std::num::NonZeroUsize::new(2).expect("nonzero limit"),
        mode: lash_core::ProcessEventQueryMode::Lite,
        continuation: Some(token),
    };
    let wire = continued.encode_json().expect("continuation request wire");
    assert_eq!(
        RemoteProcessEventsRequest::decode_json(&wire).expect("continuation"),
        continued
    );
    let mut wrong_mode = continued;
    wrong_mode.mode = lash_core::ProcessEventQueryMode::Full;
    assert!(
        RemoteProcessEventsRequest::decode_json(
            &wrong_mode.encode_json().expect("wrong mode wire")
        )
        .is_err()
    );
}
