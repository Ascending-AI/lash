//! FIG-5020 / ADR 0051: a host constructs facade records using facade paths.
use crate::remote::processes::*;
use crate::tracing::{TraceCarrier, TraceCause, TraceContext, TraceEvent, TraceRecord};

fn process() -> crate::ProcessId {
    crate::ProcessId::parse("p_0123456789ab7def8123456789abcdef").unwrap()
}
fn start() -> RemoteProcessStartRequest {
    RemoteProcessStartRequest::new(
        RemoteProcessStartTarget::Input(RemoteProcessInput::Engine {
            kind: "job".to_string(),
            payload: serde_json::json!({"job": 17}),
        }),
        RemoteStartLifetime::Detached,
        RemoteProcessOriginator::Host { scope: None },
    )
}
fn cause() -> TraceCause {
    TraceCause::Parent(
        TraceCarrier::parse_w3c(
            "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01",
            None,
        )
        .unwrap(),
    )
}

#[test]
fn host_owned_trace_records_supply_fresh_identity_and_timestamp() {
    let context = TraceContext {
        run_id: Some("host-run".into()),
        ..Default::default()
    };
    let event = TraceEvent::TurnStarted {
        metadata: Default::default(),
    };
    let before = std::time::SystemTime::now();
    let first = TraceRecord::host_owned(context.clone(), event.clone()).unwrap();
    let second = TraceRecord::host_owned(context.clone(), event.clone()).unwrap();
    assert_ne!(first.id, second.id);
    assert_eq!(first.id.len(), 32);
    assert_eq!(first.context, context);
    assert_eq!(first.event, event);
    assert!(std::time::SystemTime::from(first.timestamp) >= before);
    assert_eq!(
        serde_json::from_value::<TraceRecord>(serde_json::to_value(&first).unwrap()).unwrap(),
        first
    );
}

#[test]
fn process_start_constructor_matches_optional_wire_defaults() {
    let request = start();
    request.validate().unwrap();
    let wire = serde_json::to_value(&request).unwrap();
    for field in [
        "start_key",
        "env_ref",
        "identity",
        "wake_session_id",
        "observers",
        "event_types",
        "trace_cause",
    ] {
        assert!(wire.get(field).is_none(), "optional {field}");
    }
    assert_eq!(
        serde_json::from_value::<RemoteProcessStartRequest>(wire).unwrap(),
        request
    );
}

#[test]
fn process_start_trace_cause_preserves_wire_ancestry() {
    let request = start().with_trace_cause(cause());
    let decoded: RemoteProcessStartRequest =
        serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap();
    assert_eq!(decoded.trace_cause, cause());
}

#[test]
fn process_signal_constructor_matches_optional_wire_cause() {
    let request = RemoteProcessSignalRequest::new(
        process(),
        "ready",
        "signal-1",
        serde_json::json!({"value": 23}),
    );
    request.validate().unwrap();
    let wire = serde_json::to_value(&request).unwrap();
    assert!(wire.get("trace_cause").is_none());
    assert_eq!(
        serde_json::from_value::<RemoteProcessSignalRequest>(wire).unwrap(),
        request
    );
}

#[test]
fn process_signal_trace_cause_preserves_wire_ancestry() {
    let request =
        RemoteProcessSignalRequest::new(process(), "ready", "signal-1", serde_json::Value::Null)
            .with_trace_cause(cause());
    let decoded: RemoteProcessSignalRequest =
        serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap();
    assert_eq!(decoded.trace_cause, cause());
}

#[test]
fn process_cancel_constructor_names_process_and_requester() {
    let request = RemoteProcessCancelRequest::new(process(), "host");
    request.validate().unwrap();
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        serde_json::json!({"process_id": process(), "requester": "host"})
    );
}

#[test]
fn process_await_constructor_names_exact_lifetime() {
    let request = RemoteProcessAwaitRequest::new(process());
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        serde_json::json!({"process_id": process()})
    );
}

#[test]
fn process_events_constructor_starts_a_bounded_page() {
    let request = RemoteProcessEventsRequest::new(
        process(),
        std::num::NonZeroUsize::new(7).unwrap(),
        crate::process::ProcessEventQueryMode::Full,
    );
    request.validate().unwrap();
    assert!(request.cursor.is_none());
    assert_eq!(request.limit.get(), 7);
    assert_eq!(request.mode, crate::process::ProcessEventQueryMode::Full);
}

#[test]
fn process_observation_constructor_starts_without_a_cursor() {
    let request = crate::remote::observations::RemoteProcessObservationRequest::new(process());
    request.validate().unwrap();
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        serde_json::json!({"process_id": process()})
    );
}

#[test]
fn process_environment_constructor_keeps_complete_spec() {
    let spec = RemoteProcessExecutionEnvSpec::new(
        RemoteTurnBudget::Unbounded,
        std::num::NonZeroUsize::new(8).unwrap(),
    );
    let request = RemotePersistProcessEnvRequest::new(spec.clone());
    request.validate().unwrap();
    assert_eq!(request.env_spec, spec);
}

#[test]
fn run_identity_validates_and_retains_operation_spelling() {
    assert!(crate::RunId::parse(" ").is_err());
    let id = crate::RunId::parse("shift-operation:task-17").unwrap();
    assert_eq!(id.as_str(), "shift-operation:task-17");
    assert_eq!(
        serde_json::to_value(&id).unwrap(),
        serde_json::json!("shift-operation:task-17")
    );
    assert_eq!(crate::RunId::from(crate::TurnId::from(id.clone())), id);
}

#[test]
fn process_tool_accessor_selects_each_capability_contract() {
    use crate::process_controls::{ProcessControlTool as Tool, process_tool_definition};
    for (tool, name) in [
        (Tool::Start, "start_process"),
        (Tool::List, "list_process_handles"),
        (Tool::Await, "await_process"),
        (Tool::Signal, "signal_process"),
        (Tool::Emit, "emit_process_event"),
        (Tool::Get, "get_process_definition"),
        (Tool::Cancel, "cancel_process"),
    ] {
        assert_eq!(process_tool_definition(tool).name(), name);
    }
}

/// FIG-5020: a host refuses malformed content before model registry effects.
#[test]
fn remote_content_validation_needs_no_resolved_model() {
    use crate::remote::llm::*;
    let mut generation = RemoteGenerationOptions::default();
    validate_llm_request_content(&generation, &[], &[], None).unwrap();
    generation.output_token_cap = Some(0);
    assert!(validate_llm_request_content(&generation, &[], &[], None).is_err());
    generation.output_token_cap = None;
    let mut messages = vec![RemoteLlmMessage {
        role: RemoteLlmRole::User,
        starts_user_segment: true,
        content: vec![RemoteLlmContentBlock::Attachment {
            source: Box::new(RemoteAttachmentSource::ExternalUrl {
                media_type: "invalid-mime".into(),
                url: "https://example.test/content".into(),
            }),
        }],
    }];
    let refusal = validate_llm_request_content(&generation, &messages, &[], None).unwrap_err();
    assert!(
        refusal
            .to_string()
            .contains("syntactically valid type/subtype")
    );
    messages[0].content.clear();
    let refusal = validate_llm_request_content(&generation, &messages, &[], None).unwrap_err();
    assert!(refusal.to_string().contains("at least one block"));
}
