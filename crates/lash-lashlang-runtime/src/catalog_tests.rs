use super::*;

#[test]
fn conflicting_tool_catalog_lashlang_bindings_return_an_error() {
    let tool = |id, name, description| {
        lash_core::ToolDefinition::raw(
            id,
            name,
            description,
            lash_core::ToolDefinition::default_input_schema(),
            serde_json::Value::Null,
        )
        .with_tool_binding(ToolBinding::new(["shared"], "run").with_authority_type("Shared"))
    };
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![
        tool("tool:native/first", "native_first", "first native tool"),
        tool("tool:plugin/second", "plugin_second", "second plugin tool"),
    ]);

    let error = lashlang_resources_from_tool_catalog(&catalog)
        .expect_err("conflicting host-supplied dispatch must be a typed error");

    assert!(matches!(
        error,
        ToolBindingError::ConflictingBinding {
            source: lashlang::LashlangHostCatalogError::ConflictingModuleOperation {
                module,
                operation,
                existing,
                incoming,
            }
        } if module == "shared"
            && operation == "run"
            && existing == "tool:native/first"
            && incoming == "tool:plugin/second"
    ));
}
