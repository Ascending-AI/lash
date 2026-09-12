use super::*;

#[tokio::test]
async fn resumed_internal_process_dispatch_hidden_from_catalog_returns_tool_unavailable() {
    let executed = Arc::new(AtomicUsize::new(0));
    let mut context = exact_dispatch_context(Arc::new(InternalProbeTools {
        executed: Arc::clone(&executed),
    }));
    let controller = Arc::new(IntentReplayController::new(None));
    context.effect_controller = RuntimeEffectControllerHandle::shared(controller.clone());
    let manifest = crate::tool_dispatch::resolve_internal_manifest_by_id(
        &context,
        &crate::ToolId::from("tool:internal_probe"),
    )
    .expect("the process runner admits the internal tool before the catalog refresh");
    let prepared = crate::PreparedToolCall::identity(
        manifest.id.clone(),
        crate::sansio::PendingToolCall {
            call_id: "internal:probe:resumed".to_string(),
            tool_name: manifest.name.clone(),
            args: json!({}),
            replay: None,
        },
    );

    Arc::make_mut(&mut context.tool_catalog)
        .tools
        .retain(|entry| entry.manifest.id != manifest.id);
    assert!(context.tools.resolve_manifest_by_id(&manifest.id).is_some());
    assert!(
        crate::tool_dispatch::resolve_internal_manifest_by_id(&context, &manifest.id).is_none(),
        "the refreshed catalog hides the tool after process-runner admission"
    );
    assert!(controller.frame_sightings().is_empty());

    let tool_context = tool_context_for_prepared(&context, &prepared);
    let outcome =
        crate::tool_dispatch::execute_internal_process_tool(&context, prepared, tool_context).await;

    assert_eq!(
        outcome.record.call_id.as_deref(),
        Some("internal:probe:resumed")
    );
    let ToolCallOutcome::Failure(failure) = outcome.record.output.outcome else {
        panic!("hidden resumed internal tool must return a typed failure");
    };
    assert_eq!(failure.class, crate::ToolFailureClass::Unavailable);
    assert_eq!(failure.code, "tool_unavailable");
    assert_eq!(failure.source, crate::ToolFailureSource::Runtime);
    assert_eq!(failure.message, "Tool is unavailable in this session");
    assert_eq!(executed.load(Ordering::SeqCst), 0);
    assert!(outcome.attempts.is_empty());
    assert!(
        controller.frame_sightings().is_empty(),
        "no attempt frame is emitted"
    );
}

#[tokio::test]
async fn normal_dispatch_refuses_internal_activation_by_name_and_id() {
    let executed = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ToolProvider> = Arc::new(InternalProbeTools {
        executed: Arc::clone(&executed),
    });
    let context = exact_dispatch_context(provider);

    let outcome = dispatch_tool_call(
        &context,
        "internal_probe".to_string(),
        serde_json::json!({}),
    )
    .await;

    assert!(!outcome.record.output.is_success());
    assert_eq!(
        outcome.record.output.value_for_projection()["code"],
        "tool_unavailable"
    );
    assert_eq!(executed.load(Ordering::SeqCst), 0);
    assert!(
        resolve_callable_manifest_by_id(&context, &crate::ToolId::from("tool:internal_probe"))
            .is_none(),
        "normal by-id admission must not resolve Internal entries"
    );
}

#[tokio::test]
async fn frameless_internal_record_uses_manifest_name_when_prepared_call_is_renamed() {
    let executed = Arc::new(AtomicUsize::new(0));
    let context = exact_dispatch_context(Arc::new(InternalProbeTools {
        executed: Arc::clone(&executed),
    }));
    let prepared = crate::PreparedToolCall::from_parts(
        "internal-call",
        "tool:internal_probe",
        "provider_controlled_name",
        serde_json::json!({}),
        None,
        serde_json::Value::Null,
    );
    let tool_context = tool_context_for_prepared(&context, &prepared);

    let outcome =
        crate::tool_dispatch::execute_internal_process_tool(&context, prepared, tool_context).await;

    assert!(outcome.record.output.is_success());
    assert_eq!(outcome.record.tool, "internal_probe");
    assert_eq!(executed.load(Ordering::SeqCst), 1);
}
