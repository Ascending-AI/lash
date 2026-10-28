//! Inline reads over a process's recorded routes and component registry.

use std::sync::Arc;
use std::time::Instant;

use lash_execution_contracts::process::{EngineEffectReads, EngineReadError};
use lash_kernel_value::names::OperationUse;
use tokio_util::sync::CancellationToken;

use super::{OwnerConfigView, PluginError, ProcessComposition, ProcessEnv};

struct ProcessReads {
    composition: Arc<ProcessComposition>,
    env: Arc<ProcessEnv>,
    clock: Arc<dyn crate::Clock>,
}

impl ProcessComposition {
    /// Readers of this composition, under the environment the process
    /// recorded. No session is needed and no execution is admitted.
    pub fn effect_reads(
        &self,
        env: Arc<ProcessEnv>,
        clock: Arc<dyn crate::Clock>,
    ) -> Arc<dyn EngineEffectReads> {
        Arc::new(ProcessReads {
            composition: Arc::new(self.clone()),
            env,
            clock,
        })
    }
}

#[async_trait::async_trait]
impl EngineEffectReads for ProcessReads {
    async fn read(
        &self,
        use_: &OperationUse,
        input: Result<serde_json::Value, String>,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<Result<serde_json::Value, EngineReadError>, PluginError> {
        let effect = format!("{}:{}", use_.link, use_.slot);
        let unavailable = |why: String| PluginError::Session(format!("read of `{effect}`: {why}"));
        let Some(fixed) = self.env.execution().fixed(use_) else {
            return Err(unavailable(
                "the recorded environment has no route".to_owned(),
            ));
        };
        let link = &fixed.route;
        let contract = &fixed.requirement.contract;
        if link.policy != crate::ExecutionPolicy::Read || link.dispatches() {
            return Err(unavailable("the recorded route is not a read".to_owned()));
        }
        if !self.composition.registers(&link.provider) {
            return Err(unavailable(format!(
                "provider `{}` is not registered",
                link.provider
            )));
        }
        let definition = self
            .composition
            .served_link(&self.env, link)
            .map_err(|(codec, why)| unavailable(format!("codec `{codec}` is unreadable: {why}")))?;
        match crate::judge(contract, link, definition.as_ref()) {
            crate::Judgement::Served => {}
            crate::Judgement::Missing => {
                return Ok(Err(EngineReadError::Tool(Box::new(
                    crate::ToolFailure::runtime(
                        crate::ToolFailureClass::InvalidRequest,
                        "linked_tool_missing",
                        format!("the linked tool `{}` is no longer served", link.tool),
                    )
                    .with_cause(crate::ToolFailureCause::LinkedToolMissing {
                        effect: effect.to_string(),
                        tool: link.tool.clone(),
                    }),
                ))));
            }
            crate::Judgement::ContractChanged { pinned, live } => {
                return Ok(Err(EngineReadError::Tool(Box::new(
                    crate::operation_contract_changed(effect.as_str(), &link.tool, &pinned, &live),
                ))));
            }
        }
        let definition = definition.ok_or_else(|| unavailable("the tool is missing".to_owned()))?;
        let reader = self
            .composition
            .registry
            .reads(&link.provider)
            .ok_or_else(|| {
                unavailable(format!("provider `{}` registers no reader", link.provider))
            })?;
        let input = match input {
            Ok(input) => input,
            Err(message) => return Ok(Err(EngineReadError::Arguments(message))),
        };
        if let Err(source) = lash_vocabulary::validate_tool_input(&definition.contract, &input) {
            return Ok(Err(EngineReadError::Tool(Box::new(
                crate::ToolFailure::runtime(
                    crate::ToolFailureClass::InvalidRequest,
                    "invalid_tool_args",
                    source.to_string(),
                )
                .with_cause(crate::ToolFailureCause::ValueMismatch { source }),
            ))));
        }
        let config = OwnerConfigView::components(Arc::clone(&self.env));
        let deadline = deadline.min(Instant::now() + definition.manifest.execution());
        // The manifest retains the route's policy and argument projection.
        // Process operands are plain tool JSON, with no protocol wrappers.
        let read = reader.read(crate::ReadCall {
            history: None,
            clock: self.clock.as_ref(),
            manifest: &definition.manifest,
            args: &input,
            config: &config,
            cancel,
            deadline,
        });
        Ok(tokio::select! {
            biased;
            () = cancel.cancelled() => Err(EngineReadError::Cancelled),
            () = tokio::time::sleep_until(deadline.into()) => Err(EngineReadError::TimedOut),
            answer = read => answer.map_err(|error| EngineReadError::Reader {
                message: error.message, data: error.data,
            }),
        })
    }
}
