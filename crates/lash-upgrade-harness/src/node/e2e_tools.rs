//! H1's bodies: outside keyed writes, declared state and reported retries.
//! No body writes a plugin namespace or controls an engine turn.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use lash::plugins::{
    PluginDeclaration, PluginError, PluginFactory, PluginRegistrar, PluginSessionContext,
    SessionPlugin, StateCommands,
};
use lash::tools::{
    StaticToolExecute, StaticToolProvider, ToolAttemptOutcome, ToolBinding, ToolCall,
    ToolDefinition, ToolDefinitionBindingExt, ToolFailureClass, ToolOutcome, ToolOutcomeDone,
    ToolRetryPolicy,
};
use serde::{Deserialize, Serialize};

use crate::e2e::provider_http::ledger::{EffectAcceptance, EffectDelivery};

pub const PLUGIN: &str = "h1-provider-tools";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolFixtureArgs {
    pub effect_url: String,
    /// An out-of-journal body-delivery log, retained over host incarnations.
    /// It is execution evidence only, never an X/D/V durability oracle.
    pub bodies: PathBuf,
    pub backoff_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BodyDelivery {
    pub tool: String,
    pub label: String,
    pub delivery: EffectDelivery,
}

impl PluginFactory for ToolFixtureArgs {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn declaration(&self) -> PluginDeclaration {
        PluginDeclaration::initial(PLUGIN)
    }

    fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

impl SessionPlugin for ToolFixtureArgs {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), PluginError> {
        registrar.state_reducer(
            "increment",
            Arc::new(|reduction: lash::plugins::StateReduction<'_>| {
                let current = reduction
                    .current
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                Ok(Some(serde_json::json!(current + 1)))
            }),
        )?;
        let definitions = ["h1_effect", "h1_state", "h1_retry"].into_iter().map(|name| {
            let definition = ToolDefinition::raw(
                format!("tool:{name}"), name, "H1 deterministic correctness fixture.",
                serde_json::json!({
                    "type": "object", "properties": { "label": { "type": "string" }, "value": { "type": "string" } },
                    "required": ["label", "value"], "additionalProperties": false,
                }),
                serde_json::json!({ "type": "object" }),
            ).map_err(|error| PluginError::Session(error.to_string()))?
                .with_tool_binding(ToolBinding::new(["h1"], "write"));
            Ok(if name == "h1_retry" {
                definition.with_retry_policy(ToolRetryPolicy::safe(2, self.backoff_ms, self.backoff_ms))
            } else { definition })
        }).collect::<Result<Vec<_>, PluginError>>()?;
        registrar
            .tools()
            .provider(Arc::new(StaticToolProvider::new(definitions, self.clone())))?;
        Ok(())
    }
}

#[lash_core::async_trait]
impl StaticToolExecute for ToolFixtureArgs {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        match self.execute_body(&call).await {
            Ok(outcome) => outcome,
            Err(error) => ToolOutcome::err_fmt(error).into(),
        }
    }
}

impl ToolFixtureArgs {
    async fn execute_body(&self, call: &ToolCall<'_>) -> Result<ToolAttemptOutcome> {
        let _phase = call.context.named_phase("h1-body-entered");
        let run = call
            .context
            .logical_run()
            .ok_or_else(|| anyhow::anyhow!("H1 fixture requires a session Run"))?;
        let delivery = EffectDelivery {
            // The owner is tagged JSON, never a delimiter-joined projection.
            owner: serde_json::to_string(call.context.owner())?,
            run: run.to_string(),
            call_id: call.context.call_id().to_string(),
            attempt: call.context.attempt_number(),
            payload: call.args.clone(),
        };
        let label = call.args["label"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("fixture has no label"))?;
        let mut bytes = serde_json::to_vec(&BodyDelivery {
            tool: call.name().into(),
            label: label.into(),
            delivery: delivery.clone(),
        })?;
        bytes.push(b'\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.bodies)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        match call.name() {
            "h1_retry" if delivery.attempt == 1 => Ok(ToolOutcome::retryable_failure(
                ToolFailureClass::External,
                "h1-reported-retry",
                "first attempt reports a retryable failure",
                Some(self.backoff_ms),
            )
            .into()),
            "h1_retry" => Ok(ToolOutcome::ok(
                serde_json::json!({ "label": label, "value": call.args["value"] }),
            )
            .into()),
            "h1_state" => Ok(ToolAttemptOutcome::done_without_intents(
                ToolOutcomeDone::ok(
                    serde_json::json!({ "label": label, "value": call.args["value"] }),
                )
                .with_state(StateCommands::new().apply(
                    "total",
                    "increment",
                    serde_json::Value::Null,
                )),
            )),
            "h1_effect" => {
                let acceptance: EffectAcceptance = reqwest::Client::new()
                    .post(&self.effect_url)
                    .json(&delivery)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                anyhow::ensure!(
                    acceptance.delivery == delivery,
                    "outside service returned another call's acceptance"
                );
                Ok(ToolOutcome::ok(acceptance.result).into())
            }
            other => anyhow::bail!("unknown H1 fixture body {other}"),
        }
    }
}
