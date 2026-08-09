use crate::plugin::PluginError;

use super::ToolProcessEventContext;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn enqueue_wake_delivery(
    registry: std::sync::Arc<dyn crate::ProcessRegistry>,
    _store: Option<std::sync::Arc<dyn crate::RuntimePersistence>>,
    session_store_factory: Option<&std::sync::Arc<dyn crate::SessionStoreFactory>>,
    wake_delivery: Option<crate::ProcessWakeDelivery>,
    trace_host: Option<&dyn crate::plugin::SessionGraphService>,
    queued_work_driver: Option<&crate::QueuedWorkDriver>,
    clock: std::sync::Arc<dyn crate::Clock>,
    wake_turn_policy: &crate::WakeTurnPolicy,
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
    if let Err(error) = crate::WakeDeliveryDriver::drive_pending_once_with_policy(
        registry,
        std::sync::Arc::clone(factory),
        queued_work_driver.cloned(),
        clock,
        wake_turn_policy,
        32,
    )
    .await
    {
        tracing::warn!(error = %error, "post-append process wake nudge failed");
    }
    if let Some(host) = trace_host {
        let target_session_id = wake_delivery.target_session_id.clone();
        let request = crate::SessionStoreCreateRequest {
            session_id: target_session_id.clone(),
            relation: crate::SessionRelation::default(),
            policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        };
        if let Ok(Some(store)) = factory.open_existing_store(&request).await {
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
                                "slot_policy": enqueued.slot_policy,
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
            std::sync::Arc::clone(&process.clock),
            &process.wake_turn_policy,
        )
        .await?;
        Ok(result.event)
    }
}
