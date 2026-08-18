use crate::{PluginError, TriggerEmitReport, TriggerOccurrenceRequest};

use super::ToolContext;

#[derive(Clone)]
pub struct ToolTriggerClient<'run> {
    pub(super) context: ToolContext<'run>,
}

impl<'run> ToolTriggerClient<'run> {
    pub async fn emit(
        &self,
        request: TriggerOccurrenceRequest,
    ) -> Result<TriggerEmitReport, PluginError> {
        let Some(dispatch) = self.context.runtime_dispatch.clone() else {
            return Err(PluginError::Session(
                "trigger emission is unavailable outside runtime tool execution".to_string(),
            ));
        };
        let router = dispatch.trigger_router.as_ref().ok_or_else(|| {
            PluginError::Session("trigger store is unavailable in this runtime".to_string())
        })?;
        let outcome = request.clone();
        let report = router
            .emit(request, dispatch.effect_controller.controller())
            .await?;
        dispatch
            .trigger_outcomes
            .enqueue(crate::tool_dispatch::ToolTriggerEffectOutcome {
                source_type: outcome.source_type,
                source_key: outcome.source_key,
                occurrence_id: report.occurrence_id.clone(),
                payload: outcome.payload,
                idempotency_key: outcome.idempotency_key,
                source: outcome.source,
                deliveries: report.deliveries.clone(),
            });
        Ok(report)
    }
}
