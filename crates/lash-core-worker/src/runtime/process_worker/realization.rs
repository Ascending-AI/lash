//! The deployment worker supplies ports for an independently journaled realization.
use super::*;
#[lash_core::async_trait]
impl lash_core::tool_dispatch::ToolRealizer for DurableProcessWorker {
    async fn realize(
        &self,
        request: lash_core::tool_dispatch::RealizationRequest,
        scoped: crate::ScopedEffectController<'_>,
    ) -> Result<lash_core::tool_dispatch::RealizationReceipt, crate::RuntimeEffectControllerError>
    {
        Box::pin(lash_core::core_internal::realize_tool_intents(
            ProcessRuntimePorts {
                host: self.config.runtime_host.clone(),
                plugin_host: Arc::clone(&self.config.plugin_host),
                process_work: self.process_wiring(),
                queued_work: Arc::clone(&self.config.queued_work),
                lease_owner: self.config.lease_owner.clone(),
                turn_phase_probe: None,
            },
            request,
            scoped,
        ))
        .await
    }
}
