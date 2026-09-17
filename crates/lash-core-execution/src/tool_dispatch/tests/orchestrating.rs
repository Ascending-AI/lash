use super::*;

#[tokio::test]
async fn resumed_orchestrating_dispatch_hidden_from_catalog_returns_tool_unavailable() {
    let mut context = dispatch_context();
    context.tool_registry = Some(context.plugins.tool_registry());
    let manifest = super::resolve_callable_manifest(&context, "batch")
        .expect("batch starts admitted before the resumed dispatch");
    let prepared = crate::PreparedToolCall::identity(
        manifest.id.clone(),
        crate::sansio::PendingToolCall {
            call_id: "orchestrating:batch:resumed".to_string(),
            tool_name: manifest.name.clone(),
            args: json!({"tool_calls": []}),
            replay: None,
        },
    );

    Arc::make_mut(&mut context.tool_catalog)
        .tools
        .retain(|entry| entry.manifest.id != manifest.id);
    assert!(
        context.is_orchestrating_tool(&manifest.id),
        "the production runner still selects the registry-owned orchestration lane"
    );
    assert!(
        super::resolve_callable_manifest_by_id(&context, &manifest.id).is_none(),
        "the refreshed access-filtered catalog hides the resumed tool"
    );

    let tool_context = ToolContext::from_dispatch(Arc::new(context.clone()))
        .prepared_call(&prepared)
        .build();
    let outcome =
        crate::tool_dispatch::execute_orchestrating_tool(&context, prepared, tool_context).await;
    assert_eq!(
        outcome.record.call_id.as_deref(),
        Some("orchestrating:batch:resumed")
    );
    let ToolCallOutcome::Failure(failure) = outcome.record.output.outcome else {
        panic!("hidden resumed orchestration must return a typed failure");
    };
    assert_eq!(failure.class, crate::ToolFailureClass::Unavailable);
    assert_eq!(failure.code, "tool_unavailable");
    assert_eq!(failure.message, "Tool is unavailable in this session");
    assert_eq!(failure.source, crate::ToolFailureSource::Runtime);
}
