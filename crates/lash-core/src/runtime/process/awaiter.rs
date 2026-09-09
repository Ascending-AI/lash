use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use super::events::{
    ProcessAwaitOutput, ProcessCompletionAuthority, ProcessEvent, ProcessEventAppendReceipt,
    ProcessEventAppendRequest,
};
use super::model::{
    AbandonRequest, ProcessCompletionOutcome, ProcessExecutionWriteAuthority, ProcessExternalRef,
    ProcessLease, ProcessRecord, ProcessRegistration, ProcessStarted, SessionId, WaitState,
};
use super::registry::ProcessRegistry;
use super::registry_delegate::{
    delegate_process_leases, delegate_process_observer_registry, delegate_process_query,
    delegate_process_retention, delegate_process_tool_intents, delegate_process_wake_outbox,
};
use crate::PluginError;

mod change_hub;
#[path = "awaiter/event_sink.rs"]
mod event_sink;
mod registry_support;
pub use change_hub::ProcessChangeHub;
pub use event_sink::ProcessEventSink;

/// [`ProcessRegistry`] decorator: publishes in-process change ticks on every
/// mutation (so native process waits wake without polling) and, when a
/// [`ProcessEventSink`] is installed, emits each appended event to it.
///
/// The sink is installed once at wrap time via
/// [`watch_process_registry_with_sink`]; there is no post-hoc mutation and no
/// double-wrapping.
struct WatchedProcessRegistry {
    inner: Arc<dyn ProcessRegistry>,
    hub: ProcessChangeHub,
    sink: Option<Arc<dyn ProcessEventSink>>,
    event_paths: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

/// A process registry paired with the change hub published by its decorator.
///
/// The fields are private so consumers cannot combine a registry from one
/// watch with the hub from another.
#[derive(Clone)]
pub struct WatchedRegistry {
    registry: Arc<dyn ProcessRegistry>,
    hub: ProcessChangeHub,
}

impl WatchedRegistry {
    fn new(inner: Arc<dyn ProcessRegistry>, sink: Option<Arc<dyn ProcessEventSink>>) -> Self {
        let hub = ProcessChangeHub::new();
        let registry: Arc<dyn ProcessRegistry> = Arc::new(WatchedProcessRegistry {
            inner: Arc::clone(&inner),
            hub: hub.clone(),
            sink: sink.clone(),
            event_paths: Mutex::new(HashMap::new()),
        });
        Self { registry, hub }
    }

    /// The watched registry handle.
    pub fn registry(&self) -> &Arc<dyn ProcessRegistry> {
        &self.registry
    }

    /// The change hub paired with this watched registry.
    pub fn hub(&self) -> &ProcessChangeHub {
        &self.hub
    }
}

/// Wrap `inner` in a change-publishing registry decorator with no event sink.
///
/// The decorated handle publishes change ticks to the returned
/// [`ProcessChangeHub`]. Use [`watch_process_registry_with_sink`] to also feed a
/// host-facing [`ProcessEventSink`].
pub fn watch_process_registry(inner: Arc<dyn ProcessRegistry>) -> WatchedRegistry {
    watch_process_registry_with_sink(inner, None)
}

/// Wrap `inner` in a change-publishing registry decorator, optionally
/// installing a [`ProcessEventSink`] that receives every appended event.
///
/// The sink is best-effort freshness, not truth — see [`ProcessEventSink`].
pub fn watch_process_registry_with_sink(
    inner: Arc<dyn ProcessRegistry>,
    sink: Option<Arc<dyn ProcessEventSink>>,
) -> WatchedRegistry {
    WatchedRegistry::new(inner, sink)
}

delegate_process_query!(WatchedProcessRegistry, inner);

#[async_trait::async_trait]
impl super::registry::ProcessRegistrar for WatchedProcessRegistry {
    async fn register_process_with_observers(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<ProcessRecord, PluginError> {
        let process_id = registration.id.clone();
        let record = self
            .inner
            .register_process_with_observers(registration, observers)
            .await?;
        self.hub.notify(&process_id);
        Ok(record)
    }

    fn bind_effect_host(&self, effect_host: &Arc<dyn crate::EffectHost>) {
        self.inner.bind_effect_host(effect_host);
    }

    async fn set_external_ref(
        &self,
        process_id: &str,
        external_ref: ProcessExternalRef,
    ) -> Result<ProcessRecord, PluginError> {
        let event_path = self.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(process_id).await;
        let record = self
            .inner
            .set_external_ref(process_id, external_ref)
            .await?;
        self.hub.notify(process_id);
        self.emit_events_after(process_id, sink_cursor).await;
        Ok(record)
    }
}

delegate_process_observer_registry!(WatchedProcessRegistry, inner);

#[async_trait::async_trait]
impl super::registry::ProcessEventLog for WatchedProcessRegistry {
    async fn append_event(
        &self,
        process_id: &str,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        let event_path = self.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(process_id).await;
        let result = self.inner.append_event(process_id, request).await?;
        self.hub.notify(process_id);
        self.emit_events_after(process_id, sink_cursor).await;
        Ok(result)
    }

    async fn append_event_with_authority(
        &self,
        process_id: &str,
        request: ProcessEventAppendRequest,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        let event_path = self.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(process_id).await;
        let result = self
            .inner
            .append_event_with_authority(process_id, request, authority)
            .await?;
        self.hub.notify(process_id);
        self.emit_events_after(process_id, sink_cursor).await;
        Ok(result)
    }

    async fn events_after(
        &self,
        process_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        self.inner.events_after(process_id, after_sequence).await
    }

    async fn count_events_through(
        &self,
        process_id: &str,
        event_type: &str,
        up_to_sequence: u64,
    ) -> Result<u64, PluginError> {
        self.inner
            .count_events_through(process_id, event_type, up_to_sequence)
            .await
    }

    async fn recent_events(
        &self,
        process_id: &str,
        limit: usize,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        self.inner.recent_events(process_id, limit).await
    }
}

#[async_trait::async_trait]
impl super::registry::ProcessLifecycle for WatchedProcessRegistry {
    async fn complete_process(
        &self,
        process_id: &str,
        await_output: ProcessAwaitOutput,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        let event_path = self.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(process_id).await;
        let outcome = self
            .inner
            .complete_process(process_id, await_output, authority)
            .await?;
        self.hub.notify(process_id);
        self.emit_events_after(process_id, sink_cursor).await;
        Ok(outcome)
    }

    async fn complete_process_with_parent_end(
        &self,
        process_id: &str,
        await_output: ProcessAwaitOutput,
        authority: ProcessCompletionAuthority,
        actions: Vec<crate::ToolIntentParentEndAction>,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        let event_path = self.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(process_id).await;
        let outcome = self
            .inner
            .complete_process_with_parent_end(process_id, await_output, authority, actions)
            .await?;
        self.hub.notify(process_id);
        self.emit_events_after(process_id, sink_cursor).await;
        Ok(outcome)
    }

    async fn complete_process_with_lease(
        &self,
        lease: &ProcessLease,
        await_output: ProcessAwaitOutput,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        let event_path = self.event_path(&lease.process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(&lease.process_id).await;
        let outcome = self
            .inner
            .complete_process_with_lease(lease, await_output)
            .await?;
        self.hub.notify(&lease.process_id);
        self.emit_events_after(&lease.process_id, sink_cursor).await;
        Ok(outcome)
    }

    async fn complete_process_with_lease_and_parent_end(
        &self,
        lease: &ProcessLease,
        await_output: ProcessAwaitOutput,
        actions: Vec<crate::ToolIntentParentEndAction>,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        let event_path = self.event_path(&lease.process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(&lease.process_id).await;
        let outcome = self
            .inner
            .complete_process_with_lease_and_parent_end(lease, await_output, actions)
            .await?;
        self.hub.notify(&lease.process_id);
        self.emit_events_after(&lease.process_id, sink_cursor).await;
        Ok(outcome)
    }

    async fn list_pending_parent_end_plans(
        &self,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<super::ProcessParentEndPlan>, PluginError> {
        self.inner.list_pending_parent_end_plans(limit).await
    }

    async fn get_pending_parent_end_plan(
        &self,
        process_id: &str,
    ) -> Result<Option<crate::ProcessParentEndPlan>, PluginError> {
        self.inner.get_pending_parent_end_plan(process_id).await
    }

    async fn complete_parent_end_plan(&self, process_id: &str) -> Result<(), PluginError> {
        self.inner.complete_parent_end_plan(process_id).await
    }

    async fn record_first_started_with_authority(
        &self,
        process_id: &str,
        started: ProcessStarted,
        authority: &crate::ProcessExecutionWriteAuthority,
    ) -> Result<crate::ProcessStartOutcome, PluginError> {
        let event_path = self.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(process_id).await;
        let outcome = self
            .inner
            .record_first_started_with_authority(process_id, started, authority)
            .await?;
        self.hub.notify(process_id);
        self.emit_events_after(process_id, sink_cursor).await;
        Ok(outcome)
    }

    async fn request_process_abandon(
        &self,
        process_id: &str,
        request: AbandonRequest,
    ) -> Result<ProcessRecord, PluginError> {
        let event_path = self.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(process_id).await;
        let record = self
            .inner
            .request_process_abandon(process_id, request)
            .await?;
        self.hub.notify(process_id);
        self.emit_events_after(process_id, sink_cursor).await;
        Ok(record)
    }

    async fn record_caller_departure(
        &self,
        process_id: &str,
    ) -> Result<ProcessRecord, PluginError> {
        let event_path = self.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(process_id).await;
        let record = self.inner.record_caller_departure(process_id).await?;
        self.hub.notify(process_id);
        self.emit_events_after(process_id, sink_cursor).await;
        Ok(record)
    }

    async fn set_process_wait_with_authority(
        &self,
        process_id: &str,
        wait: WaitState,
        authority: &crate::ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        let event_path = self.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(process_id).await;
        let record = self
            .inner
            .set_process_wait_with_authority(process_id, wait, authority)
            .await?;
        self.hub.notify(process_id);
        self.emit_events_after(process_id, sink_cursor).await;
        Ok(record)
    }

    async fn clear_process_wait_with_authority(
        &self,
        process_id: &str,
        authority: &crate::ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        let event_path = self.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(process_id).await;
        let record = self
            .inner
            .clear_process_wait_with_authority(process_id, authority)
            .await?;
        self.hub.notify(process_id);
        self.emit_events_after(process_id, sink_cursor).await;
        Ok(record)
    }
}

delegate_process_tool_intents!(WatchedProcessRegistry, inner);

delegate_process_wake_outbox!(WatchedProcessRegistry, inner);

delegate_process_leases!(WatchedProcessRegistry, inner);

// No hub bump on retention: pruned rows are terminal, so any waiter on
// them resolved long ago (terminal state is durable and observed via the
// await seam).
delegate_process_retention!(WatchedProcessRegistry, inner);

impl super::registry::ProcessClockRebind for WatchedProcessRegistry {
    fn with_runtime_clock(&self, clock: Arc<dyn crate::Clock>) -> Option<Arc<dyn ProcessRegistry>> {
        self.inner.with_runtime_clock(clock).map(|inner| {
            Arc::new(Self {
                inner,
                hub: self.hub.clone(),
                sink: self.sink.clone(),
                event_paths: Mutex::new(HashMap::new()),
            }) as Arc<dyn ProcessRegistry>
        })
    }
}
