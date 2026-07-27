use crate::plugin::PluginError;

use super::ToolProcessEventContext;

pub(crate) async fn enqueue_wake_delivery(
    store: Option<std::sync::Arc<dyn crate::RuntimePersistence>>,
    session_store_factory: Option<&std::sync::Arc<dyn crate::SessionStoreFactory>>,
    wake_delivery: Option<crate::ProcessWakeDelivery>,
    trace_host: Option<&dyn crate::plugin::SessionGraphService>,
    queued_work_driver: Option<&crate::QueuedWorkDriver>,
) -> Result<(), PluginError> {
    let Some(wake_delivery) = wake_delivery else {
        return Ok(());
    };
    let target_session_id = wake_delivery.target_session_id.clone();
    let store = if let Some(factory) = session_store_factory {
        let request = crate::SessionStoreCreateRequest {
            session_id: target_session_id.clone(),
            relation: crate::SessionRelation::default(),
            policy: crate::SessionPolicy::default(),
        };
        let Some(store) = factory
            .open_existing_store(&request)
            .await
            .map_err(|err| PluginError::Session(err.to_string()))?
        else {
            return Ok(());
        };
        store
    } else {
        store.ok_or_else(|| {
            PluginError::Session(format!(
                "process wake for session `{target_session_id}` requires a runtime persistence store"
            ))
        })?
    };
    let enqueued = store
        .enqueue_queued_work(crate::process_wake_batch_draft(wake_delivery))
        .await
        .map_err(|err| PluginError::Session(err.to_string()))?;
    let target_session_id = enqueued.session_id.clone();
    if let Some(host) = trace_host
        && let Err(err) = host
            .emit_trace_event(
                lash_trace::TraceContext::default().for_session(enqueued.session_id.clone()),
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
        tracing::warn!("failed to emit process wake queue trace: {err}");
    }
    if let Some(driver) = queued_work_driver {
        let driver = driver.clone();
        let target_session_id = target_session_id.clone();
        crate::task::spawn(async move {
            driver
                .claim_and_run_pending(Some(&target_session_id), "process_wake")
                .await
        })
        .await
        .map_err(|err| {
            PluginError::Session(format!("process wake queued drive failed: {err}"))
        })??;
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
