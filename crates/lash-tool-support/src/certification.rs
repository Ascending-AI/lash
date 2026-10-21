//! FIG-6093: selected tools run through admission, recording and reopen.
use lash::plugins::{PluginCallbackIdentity, PluginFactory, PluginOptions};
use lash::testing::*;
use std::sync::Arc;
struct Echo;
#[lash::async_trait]
impl crate::StaticToolExecute for Echo {
    async fn execute(&self, _: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!("echo")).into()
    }
}
struct Scenario;
#[lash::async_trait]
impl PluginScenario for Scenario {
    fn callback(&self) -> PluginCallbackIdentity {
        PluginCallbackIdentity::tool_provider("selected", "tools")
    }
    async fn prepare(&self, harness: &mut PluginHarness) -> Result<(), PluginHarnessError> {
        harness.create(PluginOptions::default()).await
    }
    fn input(&self) -> lash::TurnInput {
        lash::TurnInput::text("echo")
    }
    fn responses(&self) -> Vec<lash::provider::LlmResponse> {
        vec![
            lash::provider::LlmResponse {
                parts: vec![lash::direct::LlmOutputPart::ToolCall {
                    call_id: "echo".into(),
                    tool_name: "tools__echo".into(),
                    input_json: "{}".into(),
                    replay: None,
                }],
                ..Default::default()
            },
            text_response("done"),
        ]
    }
    fn pause_after_responses(&self) -> usize {
        1
    }
}
#[tokio::test]
async fn selected_tools_pass_certification() {
    let definition = lash_vocabulary::tool_contract::ToolDefinition::raw(
        "echo",
        "tools.echo",
        "echo",
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        serde_json::json!({"type":"string"}),
    )
    .unwrap()
    .with_execution(std::time::Duration::from_secs(1));
    let factory: Arc<dyn PluginFactory> = Arc::new(crate::SelectedToolsPlugin::new(
        "selected",
        Arc::new(crate::StaticToolProvider::new(vec![definition], Echo)),
    ));
    let scenario: Arc<dyn PluginScenario> = Arc::new(Scenario);
    let report = certify_plugin_with(factory, HostPolicy::standard(), &[scenario])
        .await
        .unwrap();
    let failures: Vec<_> = report.failures().map(ToString::to_string).collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(report.reports.iter().any(|report| report.law
        == PluginLaw::ReplayInvokesNoRecordedCallback
        && report.result == PluginLawResult::Passed));
}
