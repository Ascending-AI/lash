// A granted attempt context may only be minted inside the runtime: a
// downstream caller who could construct one for grant A and then hand a
// different manifest to `ToolCall::new` would bypass the dispatcher's
// prepared-identity refusal. `run_tool_granted` is the sole entry.
fn main() {
    let context = lash::testing::mock_tool_context();
    let grant = lash::tools::ToolExecutionGrant::from_definition(lash::tools::ToolDefinition::raw(
        "tool:grant_only",
        "grant_only",
        "granted",
        serde_json::json!({ "type": "object" }),
        serde_json::json!({ "type": "object" }),
    ));
    let _ = lash::tools::AttemptContext::__for_granted_source(&context, "scope", &grant);
}
