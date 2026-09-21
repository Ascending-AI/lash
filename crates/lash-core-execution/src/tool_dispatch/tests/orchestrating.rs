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

#[tokio::test]
async fn batch_overflow_rows_come_from_the_parsed_specs_in_input_order() {
    // Over the 25-call cap the orchestrating lane still renders a failure row
    // per call, named from the parsed spec rather than a raw-argument re-read.
    let mut tool_calls = vec![
        json!({"tool": "batch", "parameters": {"tool_calls": []}}),
        json!({"tool": "ghost", "parameters": {}}),
    ];
    tool_calls.extend((0..23).map(|_| json!({"tool": "alpha", "parameters": {}})));
    tool_calls.push(json!({"tool": "batch", "parameters": {"tool_calls": []}}));
    tool_calls.push(json!({"tool": "ghost", "parameters": {}}));

    let outcome = dispatch_orchestrating_tool_call(
        &dispatch_context(),
        "batch",
        json!({ "tool_calls": tool_calls }),
    )
    .await;

    assert!(outcome.record.output.is_success());
    let value = outcome.record.output.value_for_projection();
    let results = value
        .get("results")
        .and_then(|value| value.as_array())
        .expect("results");
    assert_eq!(results.len(), 27);
    for (index, row) in results.iter().enumerate() {
        assert_eq!(row.get("index"), Some(&json!(index)), "row {index}");
    }
    assert_eq!(
        results[0],
        json!({
            "index": 0,
            "tool": "batch",
            "success": false,
            "duration_ms": 0,
            "error": "Tool 'batch' is not allowed inside batch"
        })
    );
    assert_eq!(
        results[1],
        json!({
            "index": 1,
            "tool": "ghost",
            "success": false,
            "duration_ms": 0,
            "error": "Tool 'ghost' is unavailable in this session"
        })
    );
    for row in &results[2..25] {
        assert_eq!(row.get("tool"), Some(&json!("tool:alpha")), "{row}");
        assert_eq!(
            row.get("success").and_then(|value| value.as_bool()),
            Some(false),
            "{row}"
        );
    }
    for (index, tool) in [(25, "batch"), (26, "ghost")] {
        assert_eq!(
            results[index],
            json!({
                "index": index,
                "tool": tool,
                "success": false,
                "duration_ms": 0,
                "error": "Maximum of 25 tool calls allowed in batch"
            })
        );
    }
}
