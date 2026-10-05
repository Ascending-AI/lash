//! FIG-5020 / ADR 0051: a host constructs facade records using facade paths.
use crate::remote::processes::*;
use crate::tracing::{TraceCarrier, TraceCause, TraceContext, TraceEvent, TraceRecord};

fn process() -> crate::ProcessId {
    crate::ProcessId::parse("p_0123456789ab7def8123456789abcdef").unwrap()
}
fn start() -> RemoteProcessStartRequest {
    RemoteProcessStartRequest::new(
        RemoteProcessStartTarget::Input(RemoteProcessInput::External {
            metadata: serde_json::json!({"job": 17}),
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

/// FIG-5020: every durable facade feed has a bounded page and an explicit
/// continuation, including an empty page; time cutoffs are validated instants.
#[tokio::test]
async fn facade_change_feeds_share_bounded_pages_and_typed_cutoffs() -> crate::Result<()> {
    let core = super::explicit_ephemeral_facets(crate::LashCore::standard_builder(
        super::double_backend().await,
    ))
    .serve_test_llm_profile(super::mock_provider(), super::mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let limit = std::num::NonZeroUsize::new(1).unwrap();
    let turns: crate::ChangePage<
        crate::persistence::TurnChange,
        crate::persistence::TurnChangeCursor,
    > = core
        .turns_changed_since(crate::persistence::TurnChangeCursor::initial(), limit)
        .await?;
    assert!(turns.changes.is_empty());
    assert_eq!(turns.next, crate::persistence::TurnChangeCursor::initial());
    assert_eq!(turns.retained_after, Some(turns.next));
    // Standing-fault pages retain the last item as their continuation; an
    // empty page retains the caller's cursor instead of restarting the feed.
    for label in ["fault-a", "fault-b"] {
        super::create_catalog_session(&core, label).await?;
        let record = crate::SessionFaultRecord::new(
            crate::SessionFaultOrigin::DriveAdmission,
            &crate::runtime::RuntimeError::new(
                crate::runtime::RuntimeErrorCode::RuntimeStoreCorrupt,
                "corrupt",
            ),
        );
        core.store_factory
            .record_session_fault(&crate::SessionId::parse(label).unwrap(), &record, 1)
            .await?;
    }
    let first = core.session_faults(None, limit).await?;
    assert_eq!(first.changes.len(), 1);
    assert_eq!(first.next.as_ref().unwrap().as_str(), "fault-a");
    let second = core.session_faults(first.next.as_ref(), limit).await?;
    assert_eq!(second.changes.len(), 1);
    assert_eq!(second.next.as_ref().unwrap().as_str(), "fault-b");
    let after = crate::SessionId::parse("last-fault").unwrap();
    let faults: crate::ChangePage<crate::SessionFault, Option<crate::SessionId>> =
        core.session_faults(Some(&after), limit).await?;
    assert!(faults.changes.is_empty());
    assert_eq!(faults.next, Some(after));
    assert_eq!(faults.retained_after, None);
    let triggers: crate::ChangePage<
        crate::triggers::TriggerSubscriptionChange,
        crate::triggers::TriggerSubscriptionChangeCursor,
    > = core
        .triggers()
        .changed_since(
            crate::triggers::TriggerSubscriptionChangeCursor::initial(),
            limit,
        )
        .await?;
    assert!(triggers.changes.is_empty());
    assert_eq!(
        triggers.next,
        crate::triggers::TriggerSubscriptionChangeCursor::initial()
    );
    assert_eq!(
        core.triggers()
            .compact_subscription_tombstones(std::time::UNIX_EPOCH)
            .await?,
        0
    );
    assert!(
        core.triggers()
            .compact_subscription_tombstones(
                std::time::UNIX_EPOCH - std::time::Duration::from_millis(1)
            )
            .await
            .is_err()
    );
    Ok(())
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
