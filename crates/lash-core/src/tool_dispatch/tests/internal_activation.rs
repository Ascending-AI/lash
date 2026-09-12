use super::*;

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
