//! The same deployment worker serves both process segments and protected intents.
use super::*;
#[async_trait::async_trait]
impl lash_core::tool_dispatch::ToolRealizer for RestateCoreProcessRunner {
    async fn realize(
        &self,
        request: lash_core::tool_dispatch::RealizationRequest,
        scoped: ScopedEffectController<'_>,
    ) -> Result<lash_core::tool_dispatch::RealizationReceipt, lash_core::RuntimeEffectControllerError>
    {
        if let ProcessWorkerSource::Slot(slot) = &self.worker {
            let realizer = slot
                .tool_realizer
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(realizer) = realizer {
                return realizer.realize(request, scoped).await;
            }
        }
        lash_core::tool_dispatch::ToolRealizer::realize(&self.worker()?, request, scoped).await
    }
}
