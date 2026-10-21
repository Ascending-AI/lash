//! Recorded, Repeatable operations over immutable inline input.
use crate::evaluator::{Batch, Failure};
use lash::plugins::*;
use lash::tools::*;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

pub fn definitions() -> Result<Vec<ToolDefinition>, PluginError> {
    ["run", "explain"]
        .into_iter()
        .map(|operation| {
            ToolDefinition::raw(
                format!("example:dataframe_{operation}"),
                format!("dataframe.{operation}"),
                "Evaluate a data-only expression run over inline rows.",
                json!({}),
                json!({}),
            )
            .map(|draft| {
                draft
                    .declared_contract(lash::workflow::document::ContractId::new(format!(
                        "example.dataframe.{operation}/1"
                    )))
                    .with_execution(Duration::from_secs(30))
                    .with_execution_policy(ExecutionPolicy::repeatable(
                        std::num::NonZeroU32::MIN,
                        0,
                        0,
                    ))
            })
            .map_err(|e| PluginError::Registration(e.to_string()))
        })
        .collect()
}

#[derive(Default)]
pub struct Runner;

impl Runner {
    pub fn evaluate(
        operation: &str,
        args: Value,
        cancelled: impl Fn() -> bool,
    ) -> Result<Value, Failure> {
        if cancelled() {
            return Err(Failure::Cancelled);
        }
        let batch: Batch =
            serde_json::from_value(args).map_err(|e| Failure::InvalidPlan(e.to_string()))?;
        match operation {
            "dataframe.run" => crate::evaluator::execute(batch, cancelled),
            "dataframe.explain" => Ok(json!({"platform":"host", "plan":batch.plan})),
            _ => Err(Failure::InvalidPlan("unknown operation".into())),
        }
    }
}

#[lash::async_trait]
impl ToolProvider for Runner {
    fn tool_manifests(&self, _: &OwnerConfigView) -> Result<Vec<ToolManifest>, PluginError> {
        Ok(definitions()?.into_iter().map(|t| t.manifest()).collect())
    }
    fn resolve_contract(&self, name: &str, _: &OwnerConfigView) -> Option<Arc<ToolContract>> {
        definitions()
            .ok()?
            .into_iter()
            .find(|t| t.manifest.tool() == name || t.id().as_str() == name)
            .map(|t| Arc::new(t.contract()))
    }
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        let result = Self::evaluate(&call.name(), call.args.clone(), || {
            call.context
                .cancellation_token()
                .is_some_and(|token| token.is_cancelled())
        });
        match result {
            Ok(value) => ToolOutcome::ok(value).into(),
            Err(error) => {
                ToolOutcome::failure(ToolFailure::new(error.code(), error.to_string())).into()
            }
        }
    }
}
