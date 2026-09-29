use crate::plugin::PluginError;

use super::ToolProcessEventContext;

#[allow(clippy::too_many_arguments)]
pub async fn enqueue_wake_delivery(
    registry: std::sync::Arc<dyn crate::ProcessRegistry>,
    _store: Option<std::sync::Arc<dyn crate::RuntimeStore>>,
    session_store_factory: Option<&std::sync::Arc<dyn crate::DeploymentStore>>,
    wake_delivery: Option<crate::ProcessWakeDelivery>,
    trace_host: Option<&dyn crate::plugin::SessionGraphService>,
    queued_work: std::sync::Arc<dyn crate::SessionWorkEngine>,
    process_wake_delivery_policy: crate::DeliveryPolicy,
    clock: std::sync::Arc<dyn crate::Clock>,
) -> Result<(), PluginError> {
    let Some(wake_delivery) = wake_delivery else {
        return Ok(());
    };
    let Some(factory) = session_store_factory else {
        // The outbox row is durable. A host with no target-store resolver
        // cannot deliver it inline; an external driver can invoke the public
        // runbook lever once that resolver is available.
        return Ok(());
    };
    if let Err(error) = crate::WakeDeliveryDriver::drive_pending_once_with_delivery_policy(
        registry,
        std::sync::Arc::clone(factory),
        queued_work,
        clock,
        process_wake_delivery_policy,
        32,
    )
    .await
    {
        tracing::warn!(error = %error, "post-append process wake nudge failed");
    }
    if let Some(host) = trace_host {
        let target_session_id = wake_delivery.target_session_id.clone();
        if let Ok(true) = crate::session_is_live(factory.as_ref(), &target_session_id).await {
            let store = factory;
            let source_key =
                crate::process_wake_source_key(&wake_delivery.process_id, wake_delivery.sequence);
            if let Ok(batches) = store.list_queued_work(&target_session_id).await
                && let Some(enqueued) = batches
                    .into_iter()
                    .find(|batch| batch.source_key.as_deref() == Some(source_key.as_str()))
                && let Err(error) = host
                    .emit_trace_event(
                        lash_trace::TraceContext::default()
                            .for_session(enqueued.session_id.clone()),
                        lash_trace::TraceEvent::Custom {
                            name: "queued_work.enqueued".to_string(),
                            payload: serde_json::json!({
                                "batch_id": enqueued.batch_id,
                                "source_key": enqueued.source_key,
                                "delivery_policy": enqueued.delivery_policy,
                                "work_kind": enqueued.kind,
                                "authority": enqueued.authority,
                                "merge_key": enqueued.merge_key,
                                "payload_types": ["process_wake"],
                            }),
                        },
                    )
                    .await
            {
                tracing::warn!(error = %error, "failed to emit process wake queue trace");
            }
        }
    }
    Ok(())
}

impl ToolProcessEventContext {
    /// Append `request` to the journal of the process this call runs inside,
    /// then nudge the wake delivery the append queued.
    pub(crate) async fn append(
        &self,
        request: crate::ProcessEventAppendRequest,
    ) -> Result<crate::ProcessEvent, PluginError> {
        let result = self
            .process_work
            .registry()
            .append_event_with_authority(&self.process_id, request, &self.execution_write_authority)
            .await?;
        enqueue_wake_delivery(
            std::sync::Arc::clone(self.process_work.registry()),
            self.store.clone(),
            self.session_store_factory.as_ref(),
            result.wake_delivery,
            Some(self.session_graph.as_ref()),
            std::sync::Arc::clone(&self.queued_work),
            self.process_wake_delivery_policy,
            std::sync::Arc::clone(&self.clock),
        )
        .await?;
        Ok(result.event)
    }
}
