use crate::plugin::PluginError;

use super::ToolProcessEventContext;

pub(crate) async fn enqueue_wake_delivery(
    registry: std::sync::Arc<dyn crate::ProcessRegistry>,
    _store: Option<std::sync::Arc<dyn crate::RuntimePersistence>>,
    session_store_factory: Option<&std::sync::Arc<dyn crate::SessionStoreFactory>>,
    wake_delivery: Option<crate::ProcessWakeDelivery>,
    _trace_host: Option<&dyn crate::plugin::SessionGraphService>,
    queued_work_driver: Option<&crate::QueuedWorkDriver>,
) -> Result<(), PluginError> {
    if wake_delivery.is_none() {
        return Ok(());
    }
    let Some(factory) = session_store_factory else {
        // The outbox row is durable. A host with no target-store resolver
        // cannot deliver it inline; an external driver can invoke the public
        // runbook lever once that resolver is available.
        return Ok(());
    };
    if let Err(error) = crate::WakeDeliveryDriver::drive_pending_once(
        registry,
        std::sync::Arc::clone(factory),
        queued_work_driver.cloned(),
        std::sync::Arc::new(crate::SystemClock),
        32,
    )
    .await
    {
        tracing::warn!(error = %error, "post-append process wake nudge failed");
    }
    Ok(())
}

#[derive(Clone)]
pub struct ToolProcessEventClient {
    pub(super) context: Option<ToolProcessEventContext>,
}

impl ToolProcessEventClient {
    pub async fn wait_event_after(
        &self,
        event_type: &str,
        after_sequence: u64,
    ) -> Result<crate::ProcessEvent, PluginError> {
        let Some(process) = self.context.as_ref() else {
            return Err(PluginError::Session(
                "process event waiting is unavailable outside a durable process".to_string(),
            ));
        };
        process
            .awaiter
            .await_event(&process.process_id, event_type, after_sequence)
            .await
    }

    pub async fn emit(
        &self,
        event_type: impl Into<String>,
        payload: serde_json::Value,
    ) -> Result<crate::ProcessEvent, PluginError> {
        self.emit_request(crate::ProcessEventAppendRequest::new(event_type, payload))
            .await
    }

    pub async fn emit_request(
        &self,
        request: crate::ProcessEventAppendRequest,
    ) -> Result<crate::ProcessEvent, PluginError> {
        let Some(process) = self.context.as_ref() else {
            return Err(PluginError::Session(
                "process event emission is unavailable outside a durable process".to_string(),
            ));
        };
        let result = process
            .registry
            .append_event_with_authority(
                &process.process_id,
                request,
                &process.execution_write_authority,
            )
            .await?;
        enqueue_wake_delivery(
            std::sync::Arc::clone(&process.registry),
            process.store.clone(),
            process.session_store_factory.as_ref(),
            result.wake_delivery,
            Some(process.session_graph.as_ref()),
            process.queued_work_driver.as_ref(),
        )
        .await?;
        Ok(result.event)
    }
}
