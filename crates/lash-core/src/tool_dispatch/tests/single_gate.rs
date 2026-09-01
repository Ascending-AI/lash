use super::*;

#[tokio::test]
async fn authority_hidden_tool_executes_on_pinned_registry_but_is_absent_from_catalog() {
    let contracts_resolved = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ToolProvider> = Arc::new(HiddenDispatchTools {
        contracts_resolved: Arc::clone(&contracts_resolved),
        executed: Arc::clone(&executed),
    });
    let plugins = PluginHost::new(vec![Arc::new(StaticPluginFactory::new(
        "test_tools",
        crate::PluginSpec::new().with_tool_provider(provider),
    ))])
    .build_session("root")
    .expect("plugin session");
    let session = crate::Session::new(crate::RuntimeServices::new(plugins), "root")
        .await
        .expect("runtime session");
    let mut tool_access = crate::SessionToolAccess::default();
    tool_access.hidden_tools.insert("hidden".to_string());
    let pinned = session
        .pin_tool_surface("root", &tool_access, None)
        .expect("authority-hidden pinned surface");

    assert!(
        !pinned.tool_catalog().has_callable_tool("hidden"),
        "the pinned catalog is the sole authority gate"
    );
    let tools = pinned.tools();
    let manifest = tools
        .resolve_manifest("hidden")
        .expect("authority-hidden tool remains registry-resolvable");
    let outcome = tools
        .execute_by_id(
            &manifest.id,
            &json!({ "value": "ok" }),
            &crate::testing::mock_attempt_context(),
        )
        .await;

    assert!(
        outcome.is_success(),
        "registry dispatch must not duplicate the catalog authority gate: {outcome:?}"
    );
    assert_eq!(contracts_resolved.load(Ordering::SeqCst), 0);
    assert_eq!(executed.load(Ordering::SeqCst), 1);
}
