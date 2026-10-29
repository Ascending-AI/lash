//! Inline reads over a process's recorded routes and component registry.

use std::sync::Arc;

use lash_execution_contracts::process::EngineEffectReads;
use lash_kernel_value::names::OperationUse;
use tokio_util::sync::CancellationToken;

use super::{OwnerConfigView, PluginError, ProcessComposition, ProcessEnv};

struct ProcessReads {
    composition: Arc<ProcessComposition>,
    env: Arc<ProcessEnv>,
    clock: Arc<dyn crate::Clock>,
    process: crate::ProcessId,
    observe: Option<Arc<dyn Fn(crate::TurnEvent) + Send + Sync>>,
}

impl ProcessComposition {
    /// Readers of this composition, under the environment the process
    /// recorded. No session is needed and no execution is admitted.
    pub fn effect_reads(
        &self,
        env: Arc<ProcessEnv>,
        clock: Arc<dyn crate::Clock>,
        process: crate::ProcessId,
        observe: Option<Arc<dyn Fn(crate::TurnEvent) + Send + Sync>>,
    ) -> Arc<dyn EngineEffectReads> {
        Arc::new(ProcessReads {
            composition: Arc::new(self.clone()),
            env,
            clock,
            process,
            observe,
        })
    }
}

#[async_trait::async_trait]
impl EngineEffectReads for ProcessReads {
    async fn read(
        &self,
        use_: &OperationUse,
        input: Result<serde_json::Value, String>,
        at: lash_execution_contracts::process::EngineReadSite<'_>,
        cancel: &CancellationToken,
        parent_remaining: std::time::Duration,
    ) -> Result<lash_protocol_types::ReadResult, PluginError> {
        use crate::tool_reads::{ReadContext, ReadFailure, ReadRoute, ReadTarget, run_read};
        let effect = format!("{}:{}", use_.link, use_.slot);
        let unavailable = |why: String| PluginError::Session(format!("read of `{effect}`: {why}"));
        let fixed = self
            .env
            .execution()
            .fixed(use_)
            .ok_or_else(|| unavailable("the recorded environment has no route".into()))?;
        let link = &fixed.route;
        if fixed.mode != lash_kernel_value::OperationMode::Read || link.dispatches() {
            return Err(unavailable("the recorded invocation is not a read".into()));
        }
        let args_bytes = input.as_ref().map_or(0, |args| args.to_string().len());
        let metadata = crate::ToolReadMetadata {
            caller: crate::ToolReadCaller::Process {
                process_id: self.process.clone(),
            },
            provider: link.provider.clone(),
            ordinal: at.ordinal,
            site: at.site.to_string(),
            origin: at.origin.to_string(),
            elapsed_us: 0,
            args_bytes,
            answer_bytes: 0,
        };
        let definition = self
            .composition
            .served_link(&self.env, link)
            .map_err(|(codec, why)| unavailable(format!("codec `{codec}` is unreadable: {why}")))?;
        let resolved = match crate::judge(&fixed.requirement.contract, link, definition.as_ref()) {
            crate::Judgement::Missing => Err(crate::ToolFailure::runtime(
                crate::ToolFailureClass::InvalidRequest,
                "linked_tool_missing",
                format!("the linked tool `{}` is no longer served", link.tool),
            )
            .with_cause(crate::ToolFailureCause::LinkedToolMissing {
                effect,
                tool: link.tool.clone(),
            })),
            judgement => {
                let definition =
                    definition.ok_or_else(|| unavailable("the tool is missing".into()))?;
                let reader = self
                    .composition
                    .registry
                    .reads(&link.provider)
                    .ok_or_else(|| {
                        unavailable(format!("provider `{}` registers no reader", link.provider))
                    })?;
                let drift = match judgement {
                    crate::Judgement::ContractChanged { pinned, live } => {
                        Some(crate::operation_contract_changed(
                            effect.as_str(),
                            &link.tool,
                            &pinned,
                            &live,
                        ))
                    }
                    _ => None,
                };
                Ok(
                    ReadRoute::new(link.provider.clone(), definition, Arc::clone(reader))
                        .with_drift(drift),
                )
            }
        };
        let target = match &resolved {
            Ok(route) => ReadTarget::Ready(route),
            Err(failure) => ReadTarget::Refused {
                tool: link.tool.to_string(),
                failure: ReadFailure::Tool(Box::new(failure.clone())),
            },
        };
        let config = OwnerConfigView::components(Arc::clone(&self.env));
        Ok(run_read(
            target,
            input.map_err(ReadFailure::Arguments),
            ReadContext {
                history: None,
                config: &config,
                clock: self.clock.as_ref(),
                cancel,
                parent_remaining,
                metadata,
                observe: self.observe.as_deref(),
            },
        )
        .await)
    }
}
