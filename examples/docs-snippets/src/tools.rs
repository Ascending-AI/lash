//! Compiled sources for the Rust snippets on `docs/tools.html`.

// docs:start:simple-fixed-tool
use std::sync::Arc;

use async_trait::async_trait;
use lash::tools::{
    StaticToolExecute, StaticToolProvider, ToolCall, ToolDefinition, ToolOutcome, ToolProvider,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, JsonSchema)]
struct WeatherArgs {
    city: String,
    units: Option<String>,
}

#[derive(Serialize, JsonSchema)]
struct WeatherReport {
    summary: String,
    temperature_c: f32,
}

struct WeatherTools;

#[async_trait]
impl StaticToolExecute for WeatherTools {
    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        match call.name {
            "weather_lookup" => {
                let args: WeatherArgs = match serde_json::from_value(call.args.clone()) {
                    Ok(args) => args,
                    Err(err) => return ToolOutcome::err_fmt(format_args!("invalid args: {err}")),
                };

                let report = lookup_weather(args).await;
                match serde_json::to_value(report) {
                    Ok(value) => ToolOutcome::ok(value),
                    Err(err) => ToolOutcome::err_fmt(format_args!("serialize output: {err}")),
                }
            }
            other => ToolOutcome::err_fmt(format_args!("unknown tool: {other}")),
        }
    }
}

pub fn weather_provider() -> Arc<dyn ToolProvider> {
    let definition = ToolDefinition::typed::<WeatherArgs, WeatherReport>(
        "tool:weather_lookup",
        "weather_lookup",
        "Look up the current weather for a city.",
    );

    Arc::new(StaticToolProvider::new(vec![definition], WeatherTools)) as Arc<dyn ToolProvider>
}

async fn lookup_weather(args: WeatherArgs) -> WeatherReport {
    let units = args.units.unwrap_or_else(|| "metric".to_string());
    WeatherReport {
        summary: format!("{} weather in {}", units, args.city),
        temperature_c: 21.0,
    }
}
// docs:end:simple-fixed-tool

#[test]
fn leaf_provider_copies_the_recorded_process_environment() {
    let tool = lash::testing::mock_tool_context();
    let attempt = lash::tools::AttemptContext::__for_testing(&tool, "docs-leaf-attempt");
    let stable_ref = attempt
        .process_execution_env_spec()
        .stable_ref()
        .expect("recorded process environment is serializable");

    assert!(stable_ref.to_string().starts_with("process-env:v4:blake3:"));
}

#[cfg(test)]
mod cross_lane_collision {
    use super::*;

    struct LeafBatchCollisionTool;

    #[async_trait]
    impl ToolProvider for LeafBatchCollisionTool {
        fn tool_manifests(&self) -> Vec<lash::tools::ToolManifest> {
            vec![
                ToolDefinition::raw(
                    "tool:batch",
                    "batch",
                    "A leaf provider colliding with an orchestrating registration.",
                    serde_json::json!({ "type": "object" }),
                    serde_json::json!({}),
                )
                .manifest(),
            ]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<lash::tools::ToolContract>> {
            (name == "batch").then(|| {
                Arc::new(
                    ToolDefinition::raw(
                        "tool:batch",
                        "batch",
                        "A leaf provider colliding with an orchestrating registration.",
                        serde_json::json!({ "type": "object" }),
                        serde_json::json!({}),
                    )
                    .contract(),
                )
            })
        }

        async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
            ToolOutcome::ok(serde_json::json!({ "unreachable": true }))
        }
    }

    #[test]
    fn leaf_and_orchestrating_tool_ids_cannot_collide() {
        let error = match lash::tools::ToolRegistry::from_tool_provider_with_orchestrating_tools(
            Arc::new(LeafBatchCollisionTool),
            vec![lash_protocol_standard::standard_batch_orchestrating_tool()],
        ) {
            Ok(_) => panic!("leaf and orchestrating registrations must have disjoint ids"),
            Err(error) => error,
        };
        let lash::tools::ReconfigureError::CrossLaneToolIdCollision {
            tool_id,
            leaf_source_id,
        } = error
        else {
            panic!("a cross-lane id collision must produce the typed rejection");
        };
        assert_eq!(tool_id.as_str(), "tool:batch");
        assert_eq!(leaf_source_id, "plugins");
    }
}

#[tokio::test]
async fn test_helper_runs_a_provider_through_the_facade() {
    let provider = weather_provider();
    let outcome = lash::testing::run_tool(
        provider.as_ref(),
        "weather_lookup",
        &serde_json::json!({ "city": "Berlin", "units": "metric" }),
    )
    .await;

    assert!(outcome.is_success());
    assert_eq!(
        outcome.value_for_projection(),
        serde_json::json!({
            "summary": "metric weather in Berlin",
            "temperature_c": 21.0
        })
    );
}
