//! FIG-6093: the standard protocol's real presentation is recorded once.
use lash::plugins::*;
use lash::testing::*;
use std::sync::Arc;
struct Echo;
#[lash::async_trait]
impl lash_tool_support::StaticToolExecute for Echo {
    async fn execute(&self, _: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!("echo")).into()
    }
}
struct Presentation;
#[lash::async_trait]
impl PluginScenario for Presentation {
    fn callback(&self) -> PluginCallbackIdentity {
        PluginCallbackIdentity {
            owner: super::STANDARD_PROTOCOL_PLUGIN_ID.into(),
            key: "presentation_presenter".into(),
        }
    }
    async fn prepare(&self, harness: &mut PluginHarness) -> Result<(), PluginHarnessError> {
        let tool = lash_core::ToolDefinition::raw(
            "echo",
            "tools.echo",
            "echo",
            serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
            serde_json::json!({"type":"string"}),
        )
        .map_err(|error| PluginError::Registration(error.to_string()))?
        .with_execution(std::time::Duration::from_secs(1));
        harness
            .install(Arc::new(StaticPluginFactory::new(
                PluginDeclaration::initial("echo"),
                PluginSpec::new().with_tool_provider(
                    lash::tool_provider_key!("tools"),
                    Arc::new(lash_tool_support::StaticToolProvider::new(vec![tool], Echo)),
                ),
            )))
            .await?;
        harness.create(PluginOptions::default()).await
    }
    fn input(&self) -> lash::TurnInput {
        lash::TurnInput::text("present echo")
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
async fn standard_protocol_passes_certification() {
    let mut policy = HostPolicy::standard();
    policy.plugin_role = PluginRole::Protocol;
    let scenario: Arc<dyn PluginScenario> = Arc::new(Presentation);
    let report = certify_plugin_with(
        Arc::new(super::StandardProtocolPluginFactory::new()),
        policy,
        &[scenario],
    )
    .await
    .unwrap();
    let failures: Vec<_> = report.failures().map(ToString::to_string).collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(
        report
            .reports
            .iter()
            .any(|report| report.callback.key == "presentation_presenter"
                && report.law == PluginLaw::ReplayInvokesNoRecordedCallback
                && report.result == PluginLawResult::Passed)
    );
}
