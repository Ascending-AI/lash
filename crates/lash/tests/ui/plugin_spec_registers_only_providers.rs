// A plugin registers tools as providers and nothing else: there is no second
// registration kind whose body the runtime would drive, so a bare tool
// definition is not a registration.
fn main() {
    let definition = lash::tools::ToolDefinition::raw(
        "tool:bare",
        "bare",
        "a definition with no provider",
        serde_json::json!({ "type": "object" }),
        serde_json::json!({ "type": "object" }),
    );
    let _ = lash::plugins::PluginSpec::new().with_tool_provider(definition);
}
