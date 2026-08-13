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
