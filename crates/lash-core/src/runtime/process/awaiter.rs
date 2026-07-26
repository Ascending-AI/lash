use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::watch;

use super::events::{
    ProcessAwaitOutput, ProcessCompletionAuthority, ProcessEvent, ProcessEventAppendRequest,
    ProcessEventAppendResult,
};
use super::model::{
    AbandonRequest, ProcessChangeCursor, ProcessExternalRef, ProcessHandleDescriptor,
    ProcessHandleGrant, ProcessHandleGrantEntry, ProcessLease, ProcessLeaseClaimOutcome,
    ProcessLeaseCompletion, ProcessListFilter, ProcessRecord, ProcessRegistration,
    ProcessSessionDeleteReport, ProcessStarted, SessionScope, WaitState,
};
use super::registry::{ProcessPruneReport, ProcessRegistry};
use crate::PluginError;

const AWAIT_BACKOFF_MIN: Duration = Duration::from_millis(25);
const AWAIT_BACKOFF_MAX: Duration = Duration::from_secs(1);

#[derive(Clone, Default)]
pub struct ProcessChangeHub {
    inner: Arc<Mutex<HashMap<String, watch::Sender<u64>>>>,
}

impl ProcessChangeHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe before reading a process row. The receiver carries only a
    /// version counter; waiters always re-read the registry after a bump.
    pub fn subscribe(&self, process_id: &str) -> watch::Receiver<u64> {
        let mut guard = self.inner.lock().expect("process change hub lock poisoned");
        guard
            .entry(process_id.to_string())
            .or_insert_with(|| {
                let (tx, _rx) = watch::channel(0);
                tx
            })
            .subscribe()
    }

    pub fn notify(&self, process_id: &str) {
        let mut guard = self.inner.lock().expect("process change hub lock poisoned");
        let mut remove = false;
        if let Some(tx) = guard.get(process_id) {
            if tx.receiver_count() == 0 {
                remove = true;
            } else {
                let next = (*tx.borrow()).wrapping_add(1);
                if tx.send(next).is_err() {
                    remove = true;
                }
            }
        }
        if remove {
            guard.remove(process_id);
        }
    }

    #[cfg(test)]
    fn tracked_processes(&self) -> usize {
        self.inner
            .lock()
            .expect("process change hub lock poisoned")
            .len()
    }
}

/// Host-facing, best-effort push of each appended process event.
///
/// A sink is an optional freshness feed, **never a source of truth.** The
/// durable event log ([`ProcessRegistry::events_after`]) is the only complete
/// record; a sink lets a host observe appends promptly without polling, but it
/// makes no delivery promise.
///
/// # Contract
///
/// - **Best-effort freshness, never truth.** [`WatchedProcessRegistry`] calls
///   [`emit`](Self::emit) after a successful `append_event`, in that pod's
///   per-process append order. There is no buffering, no retry, and no
///   delivery guarantee across pod crashes or restarts: an event that was
///   appended durably may never reach the sink (e.g. the pod died between the
///   durable write and the emit). Consumers that need completeness reconcile
///   from `events_after` — the durable log is authoritative — typically at
///   terminal time.
/// - **Emission cannot fail the write.** `emit` returns `()`, so a sink can
///   never fail or roll back an append; the durable write has already
///   committed by the time `emit` runs. But the decorator *awaits* `emit`
///   inline on the append path, so a slow sink slows every append. Implementors
///   must return fast: hand any real I/O off to a channel or background task
///   internally rather than blocking inside `emit`.
///
/// # Example: offload to a channel
///
/// A sink must return fast, so a real implementation hands each event to a
/// channel and does its projection/logging on a consumer task. Dropping on a
/// full channel is the correct best-effort behavior — the durable log, read via
/// `events_after`, remains the reconcile source.
///
/// ```
/// use lash_core::{ProcessEvent, ProcessEventSink};
/// use tokio::sync::mpsc;
///
/// struct ChannelSink {
///     tx: mpsc::Sender<ProcessEvent>,
/// }
///
/// #[async_trait::async_trait]
/// impl ProcessEventSink for ChannelSink {
///     async fn emit(&self, event: &ProcessEvent) {
///         // Non-blocking: drop on a full channel rather than slow the append.
///         let _ = self.tx.try_send(event.clone());
///     }
/// }
/// ```
#[async_trait::async_trait]
pub trait ProcessEventSink: Send + Sync {
    /// Observe one appended process event. Best-effort; see the trait contract.
    ///
    /// Must be fast and non-blocking — offload I/O to a channel/task internally.
    async fn emit(&self, event: &ProcessEvent);
}

/// [`ProcessRegistry`] decorator: publishes in-process change ticks on every
/// mutation (so [`ProcessAwaiter`] wakes without polling) and, when a
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

/// Wrap `inner` in a [`WatchedProcessRegistry`] with no event sink.
///
/// The decorated handle publishes change ticks to the returned
/// [`ProcessChangeHub`]. Use [`watch_process_registry_with_sink`] to also feed a
/// host-facing [`ProcessEventSink`].
pub fn watch_process_registry(
    inner: Arc<dyn ProcessRegistry>,
) -> (Arc<dyn ProcessRegistry>, ProcessChangeHub) {
    watch_process_registry_with_sink(inner, None)
}

/// Wrap `inner` in a [`WatchedProcessRegistry`], optionally installing a
/// [`ProcessEventSink`] that receives every appended event.
///
/// The sink is best-effort freshness, not truth — see [`ProcessEventSink`].
pub fn watch_process_registry_with_sink(
    inner: Arc<dyn ProcessRegistry>,
    sink: Option<Arc<dyn ProcessEventSink>>,
) -> (Arc<dyn ProcessRegistry>, ProcessChangeHub) {
    let hub = ProcessChangeHub::new();
    (
        Arc::new(WatchedProcessRegistry {
            inner,
            hub: hub.clone(),
            sink,
            event_paths: Mutex::new(HashMap::new()),
        }),
        hub,
    )
}

impl WatchedProcessRegistry {
    fn event_path(&self, process_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut paths = self.event_paths.lock().expect("event path lock poisoned");
        paths.retain(|_, path| path.strong_count() > 0);
        if let Some(path) = paths.get(process_id).and_then(Weak::upgrade) {
            return path;
        }
        let path = Arc::new(tokio::sync::Mutex::new(()));
        paths.insert(process_id.to_string(), Arc::downgrade(&path));
        path
    }

    async fn sink_cursor(&self, process_id: &str) -> Option<u64> {
        self.sink.as_ref()?;
        self.inner
            .recent_events(process_id, 1)
            .await
            .ok()
            .map(|events| {
                events
                    .into_iter()
                    .map(|event| event.sequence)
                    .max()
                    .unwrap_or(0)
            })
    }

    async fn emit_events_after(&self, process_id: &str, cursor: Option<u64>) {
        let (Some(sink), Some(cursor)) = (self.sink.as_ref(), cursor) else {
            return;
        };
        let Ok(events) = self.inner.events_after(process_id, cursor).await else {
            return;
        };
        for event in events {
            sink.emit(&event).await;
        }
    }
}

/// Core waiter for process terminal state and events (ADR 0016).
///
/// The awaiter is the store-only fallback that
/// [`ProcessWorkDriver`](crate::ProcessWorkDriver) uses when no engine-native
/// [`ProcessAttach`] owns the wait. It performs narrow point reads
/// (`get_process`, `events_after`) and, when constructed with a
/// [`ProcessChangeHub`], wakes promptly on local mutations instead of polling.
/// Callers still bound every wait with [`tokio::time::timeout`].
#[derive(Clone)]
pub struct ProcessAwaiter {
    registry: Arc<dyn ProcessRegistry>,
    hub: Option<ProcessChangeHub>,
}

impl ProcessAwaiter {
    /// Hub-backed awaiter: local mutations published to `hub` wake waiters
    /// without database polling. This is what a [`WatchedProcessRegistry`]
    /// wrapping provides via [`watch_process_registry`].
    pub fn new(registry: Arc<dyn ProcessRegistry>, hub: ProcessChangeHub) -> Self {
        Self {
            registry,
            hub: Some(hub),
        }
    }

    /// Hubless awaiter: correct without any change signal, using only the
    /// bounded backoff point-read loop (25ms floor, doubling, 1s cap). Use when
    /// the registry is not wrapped in-process — e.g. a store-only test.
    pub fn polling(registry: Arc<dyn ProcessRegistry>) -> Self {
        Self {
            registry,
            hub: None,
        }
    }

    /// Resolve once `process_id` is terminal, returning its outcome. See
    /// [`ProcessWorkDriver::await_terminal`](crate::ProcessWorkDriver::await_terminal)
    /// for the timeout-bounding contract.
    pub async fn await_terminal(
        &self,
        process_id: &str,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        if let Some(output) = self.read_terminal(process_id).await? {
            return Ok(output);
        }
        crate::runtime::process_worker::release_process_execution_permit_while(
            self.await_terminal_inner(process_id),
        )
        .await
    }

    async fn await_terminal_inner(
        &self,
        process_id: &str,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        let mut backoff = AWAIT_BACKOFF_MIN;
        if let Some(hub) = self.hub.as_ref() {
            let mut rx = hub.subscribe(process_id);
            loop {
                if let Some(output) = self.read_terminal(process_id).await? {
                    return Ok(output);
                }
                tokio::select! {
                    changed = rx.changed() => {
                        match changed {
                            Ok(()) => backoff = AWAIT_BACKOFF_MIN,
                            // Sender dropped (unreachable today given the hub
                            // GC invariant, but latent): a dead receiver would
                            // otherwise fire immediately on every loop turn.
                            // Stop selecting on it and degrade to the
                            // sleep-only backoff loop below.
                            Err(_) => break,
                        }
                    }
                    _ = tokio::time::sleep(backoff) => {
                        backoff = next_backoff(backoff);
                    }
                }
            }
        }
        loop {
            if let Some(output) = self.read_terminal(process_id).await? {
                return Ok(output);
            }
            tokio::time::sleep(backoff).await;
            backoff = next_backoff(backoff);
        }
    }

    /// Resolve with the first `event_type` event on `process_id` past
    /// `after_sequence`. Historical matches resolve immediately.
    pub async fn await_event(
        &self,
        process_id: &str,
        event_type: &str,
        after_sequence: u64,
    ) -> Result<ProcessEvent, PluginError> {
        if let Some(event) = self
            .read_event(process_id, event_type, after_sequence)
            .await?
        {
            return Ok(event);
        }
        crate::runtime::process_worker::release_process_execution_permit_while(
            self.await_event_inner(process_id, event_type, after_sequence),
        )
        .await
    }

    async fn await_event_inner(
        &self,
        process_id: &str,
        event_type: &str,
        after_sequence: u64,
    ) -> Result<ProcessEvent, PluginError> {
        let mut backoff = AWAIT_BACKOFF_MIN;
        if let Some(hub) = self.hub.as_ref() {
            let mut rx = hub.subscribe(process_id);
            loop {
                if let Some(event) = self
                    .read_event(process_id, event_type, after_sequence)
                    .await?
                {
                    return Ok(event);
                }
                tokio::select! {
                    changed = rx.changed() => {
                        match changed {
                            Ok(()) => backoff = AWAIT_BACKOFF_MIN,
                            // Sender dropped (unreachable today given the hub
                            // GC invariant, but latent): a dead receiver would
                            // otherwise fire immediately on every loop turn.
                            // Stop selecting on it and degrade to the
                            // sleep-only backoff loop below.
                            Err(_) => break,
                        }
                    }
                    _ = tokio::time::sleep(backoff) => {
                        backoff = next_backoff(backoff);
                    }
                }
            }
        }
        loop {
            if let Some(event) = self
                .read_event(process_id, event_type, after_sequence)
                .await?
            {
                return Ok(event);
            }
            tokio::time::sleep(backoff).await;
            backoff = next_backoff(backoff);
        }
    }

    async fn read_terminal(
        &self,
        process_id: &str,
    ) -> Result<Option<ProcessAwaitOutput>, PluginError> {
        let record = self
            .registry
            .try_get_process(process_id)
            .await?
            .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))?;
        Ok(record.status.await_output().cloned())
    }

    async fn read_event(
        &self,
        process_id: &str,
        event_type: &str,
        after_sequence: u64,
    ) -> Result<Option<ProcessEvent>, PluginError> {
        Ok(self
            .registry
            .events_after(process_id, after_sequence)
            .await?
            .into_iter()
            .find(|event| event.event_type == event_type))
    }
}

fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(AWAIT_BACKOFF_MAX)
}

#[async_trait::async_trait]
pub trait ProcessAttach: Send + Sync {
    async fn await_terminal(&self, process_id: &str) -> Result<ProcessAwaitOutput, PluginError>;
}

#[async_trait::async_trait]
impl ProcessRegistry for WatchedProcessRegistry {
    fn durability_tier(&self) -> crate::DurabilityTier {
        self.inner.durability_tier()
    }

    async fn register_process(
        &self,
        registration: ProcessRegistration,
    ) -> Result<ProcessRecord, PluginError> {
        let process_id = registration.id.clone();
        let record = self.inner.register_process(registration).await?;
        self.hub.notify(&process_id);
        Ok(record)
    }

    async fn put_segment_handover(
        &self,
        process_id: &str,
        handover: crate::PersistedSegmentHandover,
    ) -> Result<(), PluginError> {
        self.inner.put_segment_handover(process_id, handover).await
    }

    async fn get_segment_handover(
        &self,
        process_id: &str,
        segment_ordinal: u64,
    ) -> Result<Option<crate::PersistedSegmentHandover>, PluginError> {
        self.inner
            .get_segment_handover(process_id, segment_ordinal)
            .await
    }

    async fn latest_segment_handover(
        &self,
        process_id: &str,
    ) -> Result<Option<crate::PersistedSegmentHandover>, PluginError> {
        self.inner.latest_segment_handover(process_id).await
    }

    async fn delete_segment_handovers(&self, process_id: &str) -> Result<(), PluginError> {
        self.inner.delete_segment_handovers(process_id).await
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

    async fn grant_handle(
        &self,
        session_scope: &SessionScope,
        process_id: &str,
        descriptor: ProcessHandleDescriptor,
    ) -> Result<ProcessHandleGrant, PluginError> {
        self.inner
            .grant_handle(session_scope, process_id, descriptor)
            .await
    }

    async fn revoke_handle(
        &self,
        session_scope: &SessionScope,
        process_id: &str,
    ) -> Result<(), PluginError> {
        self.inner.revoke_handle(session_scope, process_id).await
    }

    async fn transfer_handle_grants(
        &self,
        from_scope: &SessionScope,
        to_scope: &SessionScope,
        process_ids: &[String],
    ) -> Result<(), PluginError> {
        self.inner
            .transfer_handle_grants(from_scope, to_scope, process_ids)
            .await
    }

    async fn list_handle_grants(
        &self,
        session_scope: &SessionScope,
    ) -> Result<Vec<ProcessHandleGrantEntry>, PluginError> {
        self.inner.list_handle_grants(session_scope).await
    }

    async fn list_live_handle_grants(
        &self,
        session_scope: &SessionScope,
    ) -> Result<Vec<ProcessHandleGrantEntry>, PluginError> {
        self.inner.list_live_handle_grants(session_scope).await
    }

    async fn has_handle_grant(
        &self,
        session_scope: &SessionScope,
        process_id: &str,
    ) -> Result<bool, PluginError> {
        self.inner.has_handle_grant(session_scope, process_id).await
    }

    async fn handle_grants_for_process(
        &self,
        process_id: &str,
    ) -> Result<Vec<ProcessHandleGrant>, PluginError> {
        self.inner.handle_grants_for_process(process_id).await
    }

    async fn delete_session_process_state(
        &self,
        session_id: &str,
    ) -> Result<ProcessSessionDeleteReport, PluginError> {
        self.inner.delete_session_process_state(session_id).await
    }

    async fn append_event(
        &self,
        process_id: &str,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendResult, PluginError> {
        let event_path = self.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(process_id).await;
        let result = self.inner.append_event(process_id, request).await?;
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

    async fn wake_events_after(
        &self,
        process_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        self.inner
            .wake_events_after(process_id, after_sequence)
            .await
    }

    async fn complete_process(
        &self,
        process_id: &str,
        await_output: ProcessAwaitOutput,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        let event_path = self.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(process_id).await;
        let record = self
            .inner
            .complete_process(process_id, await_output, authority)
            .await?;
        self.hub.notify(process_id);
        self.emit_events_after(process_id, sink_cursor).await;
        Ok(record)
    }

    async fn complete_process_with_lease(
        &self,
        lease: &ProcessLease,
        await_output: ProcessAwaitOutput,
    ) -> Result<ProcessRecord, PluginError> {
        let event_path = self.event_path(&lease.process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(&lease.process_id).await;
        let record = self
            .inner
            .complete_process_with_lease(lease, await_output)
            .await?;
        self.hub.notify(&lease.process_id);
        self.emit_events_after(&lease.process_id, sink_cursor).await;
        Ok(record)
    }

    async fn record_first_started(
        &self,
        process_id: &str,
        started: ProcessStarted,
    ) -> Result<ProcessRecord, PluginError> {
        let event_path = self.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(process_id).await;
        let record = self.inner.record_first_started(process_id, started).await?;
        self.hub.notify(process_id);
        self.emit_events_after(process_id, sink_cursor).await;
        Ok(record)
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

    async fn set_process_wait(
        &self,
        process_id: &str,
        wait: WaitState,
    ) -> Result<ProcessRecord, PluginError> {
        let event_path = self.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(process_id).await;
        let record = self.inner.set_process_wait(process_id, wait).await?;
        self.hub.notify(process_id);
        self.emit_events_after(process_id, sink_cursor).await;
        Ok(record)
    }

    async fn clear_process_wait(&self, process_id: &str) -> Result<ProcessRecord, PluginError> {
        let event_path = self.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = self.sink_cursor(process_id).await;
        let record = self.inner.clear_process_wait(process_id).await?;
        self.hub.notify(process_id);
        self.emit_events_after(process_id, sink_cursor).await;
        Ok(record)
    }

    async fn get_process(&self, process_id: &str) -> Option<ProcessRecord> {
        self.inner.get_process(process_id).await
    }

    async fn try_get_process(
        &self,
        process_id: &str,
    ) -> Result<Option<ProcessRecord>, PluginError> {
        self.inner.try_get_process(process_id).await
    }

    async fn list_processes(
        &self,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        self.inner.list_processes(filter).await
    }

    async fn processes_changed_since(
        &self,
        cursor: ProcessChangeCursor,
        limit: usize,
    ) -> Result<(Vec<ProcessRecord>, ProcessChangeCursor), PluginError> {
        self.inner.processes_changed_since(cursor, limit).await
    }

    async fn ack_wake(&self, process_id: &str, sequence: u64) -> Result<(), PluginError> {
        self.inner.ack_wake(process_id, sequence).await?;
        self.hub.notify(process_id);
        Ok(())
    }

    async fn list_non_terminal(&self) -> Result<Vec<ProcessRecord>, PluginError> {
        self.inner.list_non_terminal().await
    }

    async fn live_reference_summary(
        &self,
    ) -> Result<Vec<super::references::ProcessLiveReferenceSummary>, PluginError> {
        self.inner.live_reference_summary().await
    }

    async fn claim_process_lease(
        &self,
        process_id: &str,
        owner: &crate::LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLeaseClaimOutcome, PluginError> {
        self.inner
            .claim_process_lease(process_id, owner, lease_ttl_ms)
            .await
    }

    async fn reclaim_process_lease(
        &self,
        process_id: &str,
        owner: &crate::LeaseOwnerIdentity,
        observed_holder: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLeaseClaimOutcome, PluginError> {
        self.inner
            .reclaim_process_lease(process_id, owner, observed_holder, lease_ttl_ms)
            .await
    }

    async fn renew_process_lease(
        &self,
        lease: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLease, PluginError> {
        self.inner.renew_process_lease(lease, lease_ttl_ms).await
    }

    async fn get_process_lease(
        &self,
        process_id: &str,
    ) -> Result<Option<ProcessLease>, PluginError> {
        self.inner.get_process_lease(process_id).await
    }

    async fn complete_process_lease(
        &self,
        completion: &ProcessLeaseCompletion,
    ) -> Result<(), PluginError> {
        self.inner.complete_process_lease(completion).await
    }

    async fn prune_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        up_to_change_seq: Option<ProcessChangeCursor>,
    ) -> Result<ProcessPruneReport, PluginError> {
        // No hub bump: pruned rows are terminal, so any waiter on them resolved
        // long ago (terminal state is durable and observed via the await seam).
        self.inner
            .prune_terminal_processes(cutoff_epoch_ms, filter, up_to_change_seq)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        ProcessInput, ProcessProvenance, ProcessRegistration, TestLocalProcessRegistry, ToolControl,
    };

    fn registration(process_id: &str) -> ProcessRegistration {
        ProcessRegistration::new(
            process_id,
            ProcessInput::External {
                metadata: serde_json::json!({}),
            },
            crate::RecoveryDisposition::ExternallyOwned,
            ProcessProvenance::host(),
        )
    }

    fn plain_event_type(name: &str) -> crate::ProcessEventType {
        crate::ProcessEventType {
            name: name.to_string(),
            payload_schema: crate::LashSchema::any(),
            semantics: crate::ProcessEventSemanticsSpec::default(),
        }
    }

    fn registration_with_events(process_id: &str, event_types: &[&str]) -> ProcessRegistration {
        registration(process_id)
            .with_extra_event_types(event_types.iter().map(|name| plain_event_type(name)))
    }

    /// Records `(event_type, sequence)` in emit order for sink assertions.
    #[derive(Clone, Default)]
    struct CollectingSink {
        events: Arc<Mutex<Vec<(String, u64)>>>,
    }

    impl CollectingSink {
        fn collected(&self) -> Vec<(String, u64)> {
            self.events.lock().expect("sink lock").clone()
        }
    }

    #[async_trait::async_trait]
    impl ProcessEventSink for CollectingSink {
        async fn emit(&self, event: &ProcessEvent) {
            self.events
                .lock()
                .expect("sink lock")
                .push((event.event_type.clone(), event.sequence));
        }
    }

    fn success(value: serde_json::Value) -> ProcessAwaitOutput {
        ProcessAwaitOutput::Success {
            value,
            control: None::<ToolControl>,
        }
    }

    /// ADR 0016 pins the awaiter's polling cadence: a 25ms floor, doubling
    /// backoff, and a 1s cap. Changing any of the three alters every store-only
    /// deployment's wait economics, so the exact schedule is asserted here.
    #[test]
    fn backoff_schedule_has_25ms_floor_doubling_to_1s_cap() {
        assert_eq!(AWAIT_BACKOFF_MIN, Duration::from_millis(25));
        assert_eq!(AWAIT_BACKOFF_MAX, Duration::from_secs(1));

        let mut backoff = AWAIT_BACKOFF_MIN;
        let mut schedule = vec![backoff];
        while backoff < AWAIT_BACKOFF_MAX {
            backoff = next_backoff(backoff);
            schedule.push(backoff);
        }
        assert_eq!(
            schedule,
            [25, 50, 100, 200, 400, 800, 1000]
                .into_iter()
                .map(Duration::from_millis)
                .collect::<Vec<_>>(),
            "the backoff doubles from the 25ms floor and saturates at the 1s cap"
        );
        assert_eq!(
            next_backoff(AWAIT_BACKOFF_MAX),
            AWAIT_BACKOFF_MAX,
            "the cap is absorbing"
        );
    }

    /// ADR 0017: the decorator delegates `prune_terminal_processes` without a
    /// hub bump — pruned rows are terminal, so their waiters resolved long ago
    /// and a tick would only wake unrelated subscribers spuriously.
    #[tokio::test]
    async fn prune_through_decorator_does_not_bump_the_hub() {
        let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let (registry, hub) = watch_process_registry(raw);
        registry
            .register_process(registration("proc-terminal"))
            .await
            .expect("register terminal");
        registry
            .complete_process(
                "proc-terminal",
                success(serde_json::json!("done")),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete");
        registry
            .register_process(registration("proc-live"))
            .await
            .expect("register live");

        // Subscribe after the mutations above so only post-subscription bumps
        // are observable.
        let mut terminal_rx = hub.subscribe("proc-terminal");
        let mut live_rx = hub.subscribe("proc-live");
        terminal_rx.mark_unchanged();
        live_rx.mark_unchanged();

        let report = registry
            .prune_terminal_processes(u64::MAX, None, None)
            .await
            .expect("prune");
        assert_eq!(report.pruned_processes, 1, "the terminal process pruned");

        assert!(
            !terminal_rx.has_changed().expect("terminal sender open"),
            "prune must not bump the pruned process's hub entry"
        );
        assert!(
            !live_rx.has_changed().expect("live sender open"),
            "prune must not bump surviving processes' hub entries"
        );
    }

    #[tokio::test]
    async fn hub_subscribe_then_notify_wakes_and_gc_drops_empty_entry() {
        let hub = ProcessChangeHub::new();
        let mut rx = hub.subscribe("proc");
        hub.notify("proc");
        tokio::time::timeout(Duration::from_millis(100), rx.changed())
            .await
            .expect("notify should wake")
            .expect("sender remains open");

        drop(rx);
        hub.notify("proc");
        assert_eq!(hub.tracked_processes(), 0);
    }

    #[tokio::test]
    async fn await_event_returns_historical_event_immediately() {
        let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let (registry, hub) = watch_process_registry(raw);
        registry
            .register_process(registration("proc"))
            .await
            .expect("register");
        let appended = registry
            .append_event(
                "proc",
                ProcessEventAppendRequest::cancel_requested("proc", Some("stop".to_string())),
            )
            .await
            .expect("append");

        let event = ProcessAwaiter::new(Arc::clone(&registry), hub)
            .await_event("proc", "process.cancel_requested", 0)
            .await
            .expect("await event");
        assert_eq!(event.sequence, appended.event.sequence);
    }

    #[tokio::test]
    async fn await_terminal_unknown_process_errors() {
        let registry = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let err = ProcessAwaiter::polling(registry)
            .await_terminal("missing")
            .await
            .expect_err("unknown process should error");
        assert!(err.to_string().contains("unknown process `missing`"));
    }

    #[tokio::test]
    async fn await_terminal_propagates_process_store_read_errors() {
        let registry = Arc::new(TestLocalProcessRegistry::default());
        registry
            .set_process_read_error(Some(PluginError::Session(
                "process store read failed".to_string(),
            )))
            .await;
        let err = ProcessAwaiter::polling(registry)
            .await_terminal("unreadable")
            .await
            .expect_err("store read failure should surface");
        assert_eq!(
            err.to_string(),
            "plugin session error: process store read failed"
        );
        assert!(!err.to_string().contains("unknown process"));
    }

    #[tokio::test]
    async fn polling_awaiter_resolves_via_backoff() {
        let registry = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        registry
            .register_process(registration("proc"))
            .await
            .expect("register");
        let writer = Arc::clone(&registry);
        crate::task::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            writer
                .complete_process(
                    "proc",
                    success(serde_json::json!({ "ok": true })),
                    crate::ProcessCompletionAuthority::external_owner(),
                )
                .await
                .expect("complete");
        });

        let output = tokio::time::timeout(
            Duration::from_secs(1),
            ProcessAwaiter::polling(registry).await_terminal("proc"),
        )
        .await
        .expect("polling await timeout")
        .expect("await terminal");
        assert_eq!(output, success(serde_json::json!({ "ok": true })));
    }

    #[tokio::test]
    async fn watched_awaiter_observes_terminal_without_lost_wakeup() {
        let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let (registry, hub) = watch_process_registry(raw);
        registry
            .register_process(registration("proc"))
            .await
            .expect("register");
        let awaiter = ProcessAwaiter::new(Arc::clone(&registry), hub);
        let waiter = crate::task::spawn(async move { awaiter.await_terminal("proc").await });
        registry
            .complete_process(
                "proc",
                success(serde_json::json!("done")),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete");

        let output = tokio::time::timeout(Duration::from_millis(200), waiter)
            .await
            .expect("watched await timeout")
            .expect("join")
            .expect("await terminal");
        assert_eq!(output, success(serde_json::json!("done")));
    }

    #[tokio::test]
    async fn watched_registry_bumps_on_mutations() {
        let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let (registry, hub) = watch_process_registry(raw);
        let mut rx = hub.subscribe("proc");
        registry
            .register_process(registration("proc"))
            .await
            .expect("register");
        tokio::time::timeout(Duration::from_millis(100), rx.changed())
            .await
            .expect("register bump")
            .expect("sender remains open");

        registry
            .append_event(
                "proc",
                ProcessEventAppendRequest::cancel_requested("proc", None),
            )
            .await
            .expect("append");
        tokio::time::timeout(Duration::from_millis(100), rx.changed())
            .await
            .expect("append bump")
            .expect("sender remains open");
    }

    #[tokio::test]
    async fn sink_receives_appended_events_in_order() {
        let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let sink = CollectingSink::default();
        let (registry, _hub) = watch_process_registry_with_sink(raw, Some(Arc::new(sink.clone())));
        registry
            .register_process(registration_with_events(
                "proc",
                &["producer.a", "producer.b"],
            ))
            .await
            .expect("register");
        registry
            .append_event(
                "proc",
                ProcessEventAppendRequest::new("producer.a", serde_json::json!({})),
            )
            .await
            .expect("append a");
        registry
            .append_event(
                "proc",
                ProcessEventAppendRequest::new("producer.b", serde_json::json!({})),
            )
            .await
            .expect("append b");

        assert_eq!(
            sink.collected(),
            vec![("producer.a".to_string(), 1), ("producer.b".to_string(), 2)],
            "the sink must observe appended events after their write, in append order"
        );
    }

    #[tokio::test]
    async fn sink_absent_leaves_appends_unchanged() {
        let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let (registry, _hub) = watch_process_registry_with_sink(raw, None);
        registry
            .register_process(registration_with_events("proc", &["producer.a"]))
            .await
            .expect("register");
        let appended = registry
            .append_event(
                "proc",
                ProcessEventAppendRequest::new("producer.a", serde_json::json!({})),
            )
            .await
            .expect("append succeeds with no sink installed");
        assert_eq!(appended.event.sequence, 1);
    }

    #[tokio::test]
    async fn sink_receives_complete_process_terminal_append() {
        let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let sink = CollectingSink::default();
        let (registry, _hub) = watch_process_registry_with_sink(raw, Some(Arc::new(sink.clone())));
        registry
            .register_process(registration_with_events("proc", &["producer.a"]))
            .await
            .expect("register");
        registry
            .append_event(
                "proc",
                ProcessEventAppendRequest::new("producer.a", serde_json::json!({})),
            )
            .await
            .expect("explicit append");
        registry
            .complete_process(
                "proc",
                success(serde_json::json!("done")),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete");

        assert_eq!(
            sink.collected(),
            vec![
                ("producer.a".to_string(), 1),
                ("process.completed".to_string(), 2),
            ],
            "the sink must observe terminal events appended through completion verbs"
        );
    }

    #[tokio::test]
    async fn sink_receives_runtime_lifecycle_events_in_order() {
        let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let sink = CollectingSink::default();
        let (registry, _hub) = watch_process_registry_with_sink(raw, Some(Arc::new(sink.clone())));
        registry
            .register_process(registration("proc"))
            .await
            .expect("register");
        registry
            .record_first_started(
                "proc",
                ProcessStarted {
                    owner: crate::LeaseOwnerIdentity::opaque("owner", "incarnation"),
                    started_at_ms: 1,
                },
            )
            .await
            .expect("record first start");
        let wait = WaitState {
            kind: crate::WaitKind::Signal {
                name: "ready".to_string(),
                event_type: "signal.ready".to_string(),
                key: "process:proc:signal.ready:1".to_string(),
                ordinal: 1,
            },
            since_ms: 2,
        };
        registry
            .set_process_wait("proc", wait)
            .await
            .expect("enter wait");
        registry
            .clear_process_wait("proc")
            .await
            .expect("clear wait");
        registry
            .set_external_ref(
                "proc",
                ProcessExternalRef {
                    backend: "test".to_string(),
                    id: "external".to_string(),
                    metadata: None,
                },
            )
            .await
            .expect("set external ref");
        registry
            .request_process_abandon(
                "proc",
                AbandonRequest {
                    requested_by: "test".to_string(),
                    requested_at_ms: 3,
                    reason: None,
                },
            )
            .await
            .expect("request abandon");

        assert_eq!(
            sink.collected(),
            vec![
                ("process.first_started".to_string(), 1),
                ("process.waiting".to_string(), 2),
                ("process.resumed".to_string(), 3),
                ("process.external_ref_set".to_string(), 4),
                ("process.abandon_requested".to_string(), 5),
            ],
            "the sink must observe every runtime lifecycle append"
        );
    }

    #[tokio::test]
    async fn sink_present_still_bumps_hub_on_append() {
        let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let sink = CollectingSink::default();
        let (registry, hub) = watch_process_registry_with_sink(raw, Some(Arc::new(sink)));
        let mut rx = hub.subscribe("proc");
        registry
            .register_process(registration_with_events("proc", &["producer.a"]))
            .await
            .expect("register");
        tokio::time::timeout(Duration::from_millis(100), rx.changed())
            .await
            .expect("register bump")
            .expect("sender remains open");
        registry
            .append_event(
                "proc",
                ProcessEventAppendRequest::new("producer.a", serde_json::json!({})),
            )
            .await
            .expect("append");
        tokio::time::timeout(Duration::from_millis(100), rx.changed())
            .await
            .expect("append bump with a sink installed")
            .expect("sender remains open");
    }

    struct NoopRunHandle;

    #[async_trait::async_trait]
    impl crate::ProcessRunHandle for NoopRunHandle {
        async fn claim_and_run_pending(&self) -> Result<(), PluginError> {
            Ok(())
        }
    }

    struct PanicAttach;

    #[async_trait::async_trait]
    impl ProcessAttach for PanicAttach {
        async fn await_terminal(
            &self,
            _process_id: &str,
        ) -> Result<ProcessAwaitOutput, PluginError> {
            panic!("attach should not be called for already-terminal process")
        }
    }

    struct ErrorAttach;

    #[async_trait::async_trait]
    impl ProcessAttach for ErrorAttach {
        async fn await_terminal(
            &self,
            _process_id: &str,
        ) -> Result<ProcessAwaitOutput, PluginError> {
            Err(PluginError::Session("attach failed".to_string()))
        }
    }

    #[tokio::test]
    async fn driver_short_circuits_terminal_before_attach() {
        let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let driver = crate::ProcessWorkDriver::new(raw, Arc::new(NoopRunHandle))
            .with_attach(Arc::new(PanicAttach));
        let registry = driver.process_registry();
        registry
            .register_process(registration("proc"))
            .await
            .expect("register");
        registry
            .complete_process(
                "proc",
                success(serde_json::json!("ready")),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete");

        let output = driver.await_terminal("proc").await.expect("await terminal");
        assert_eq!(output, success(serde_json::json!("ready")));
    }

    #[tokio::test]
    async fn driver_propagates_process_store_read_errors() {
        let raw = Arc::new(TestLocalProcessRegistry::default());
        raw.set_process_read_error(Some(PluginError::Session(
            "process store read failed".to_string(),
        )))
        .await;
        let driver = crate::ProcessWorkDriver::new(raw, Arc::new(NoopRunHandle));

        let err = driver
            .await_terminal("unreadable")
            .await
            .expect_err("store read failure should surface");
        assert_eq!(
            err.to_string(),
            "plugin session error: process store read failed"
        );
        assert!(!err.to_string().contains("unknown process"));
    }

    #[tokio::test]
    async fn driver_attach_errors_propagate_without_poll_fallback() {
        let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let driver = crate::ProcessWorkDriver::new(raw, Arc::new(NoopRunHandle))
            .with_attach(Arc::new(ErrorAttach));
        driver
            .process_registry()
            .register_process(registration("proc"))
            .await
            .expect("register");

        let err = driver
            .await_terminal("proc")
            .await
            .expect_err("attach error should propagate");
        assert!(err.to_string().contains("attach failed"));
    }

    struct CountingAttach {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ProcessAttach for CountingAttach {
        async fn await_terminal(
            &self,
            _process_id: &str,
        ) -> Result<ProcessAwaitOutput, PluginError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(PluginError::Session(
                "attach must not be consulted for a terminal process".to_string(),
            ))
        }
    }

    /// Sim-style race: many waiters attach to one process and completion fires
    /// while they are mid-flight between their subscribe and their first read.
    /// The change hub must resolve every one with identical output — no lost
    /// wakeups, no divergent results (ADR 0016).
    #[tokio::test]
    async fn concurrent_waiters_all_resolve_with_identical_output_on_completion() {
        let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let (registry, hub) = watch_process_registry(raw);
        registry
            .register_process(registration("proc"))
            .await
            .expect("register");

        const WAITERS: usize = 16;
        let barrier = Arc::new(tokio::sync::Barrier::new(WAITERS + 1));
        let mut waiters = Vec::with_capacity(WAITERS);
        for _ in 0..WAITERS {
            let awaiter = ProcessAwaiter::new(Arc::clone(&registry), hub.clone());
            let barrier = Arc::clone(&barrier);
            waiters.push(crate::task::spawn(async move {
                barrier.wait().await;
                awaiter.await_terminal("proc").await
            }));
        }
        // Release every waiter, then complete at once so completion races their
        // first read and subscribe.
        barrier.wait().await;
        let output = success(serde_json::json!({ "raced": true }));
        registry
            .complete_process(
                "proc",
                output.clone(),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete");

        for waiter in waiters {
            let resolved = tokio::time::timeout(Duration::from_secs(2), waiter)
                .await
                .expect("each racing waiter resolves under 2s")
                .expect("join waiter")
                .expect("await terminal");
            assert_eq!(
                resolved, output,
                "every concurrent waiter resolves with identical terminal output"
            );
        }
    }

    /// Sim-style restart/re-attach: a process completes while no waiter is
    /// attached; a later `await_terminal` resolves instantly through the
    /// registry short-circuit and never consults the engine attach (ADR 0016 —
    /// the terminal point-read precedes any attach hand-off).
    #[tokio::test]
    async fn driver_reattach_after_terminal_short_circuits_without_engine_call() {
        use std::sync::atomic::Ordering;

        let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let driver = crate::ProcessWorkDriver::new(raw, Arc::new(NoopRunHandle)).with_attach(
            Arc::new(CountingAttach {
                calls: Arc::clone(&calls),
            }),
        );
        let registry = driver.process_registry();
        registry
            .register_process(registration("proc"))
            .await
            .expect("register");
        // Process reaches terminal with no waiter attached.
        let output = success(serde_json::json!("reattached"));
        registry
            .complete_process(
                "proc",
                output.clone(),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete");

        // A later await resolves via the registry short-circuit, instantly.
        let start = std::time::Instant::now();
        let resolved = driver.await_terminal("proc").await.expect("await terminal");
        assert_eq!(resolved, output);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a terminal short-circuit must never call the engine attach"
        );
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "a short-circuit resolves without any backoff wait"
        );
    }

    /// Records seen vs. dropped emit sequences, dropping even sequences to model
    /// best-effort push loss.
    #[derive(Clone, Default)]
    struct LossySink {
        seen: Arc<Mutex<Vec<u64>>>,
        dropped: Arc<Mutex<Vec<u64>>>,
    }

    #[async_trait::async_trait]
    impl ProcessEventSink for LossySink {
        async fn emit(&self, event: &ProcessEvent) {
            if event.sequence.is_multiple_of(2) {
                self.dropped.lock().expect("sink lock").push(event.sequence);
            } else {
                self.seen.lock().expect("sink lock").push(event.sequence);
            }
        }
    }

    /// Sim-style sink loss: a sink that drops a fraction of emits still leaves
    /// the durable log complete. Reconciling from `events_after` at terminal
    /// recovers every event the push feed missed — ADR 0017's "push loss never
    /// loses truth".
    #[tokio::test]
    async fn lossy_sink_still_reconciles_complete_log_from_events_after() {
        let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let sink = LossySink::default();
        let (registry, _hub) = watch_process_registry_with_sink(raw, Some(Arc::new(sink.clone())));
        registry
            .register_process(registration_with_events("proc", &["producer.step"]))
            .await
            .expect("register");

        const EVENTS: u64 = 6;
        for _ in 0..EVENTS {
            registry
                .append_event(
                    "proc",
                    ProcessEventAppendRequest::new("producer.step", serde_json::json!({})),
                )
                .await
                .expect("append");
        }
        // Terminal events ride the same best-effort sink and remain durable for
        // reconciliation if that push is dropped.
        registry
            .complete_process(
                "proc",
                success(serde_json::json!("done")),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete");

        // The push feed genuinely lost some events...
        assert!(
            !sink.dropped.lock().expect("sink lock").is_empty(),
            "the lossy sink must drop at least one emit for the scenario to be meaningful"
        );
        assert!(
            (sink.seen.lock().expect("sink lock").len() as u64) < EVENTS,
            "the sink observed fewer events than were appended"
        );
        // ...but the durable log is the complete, ordered truth.
        let reconciled = registry
            .events_after("proc", 0)
            .await
            .expect("events")
            .into_iter()
            .filter(|event| event.event_type == "producer.step")
            .map(|event| event.sequence)
            .collect::<Vec<_>>();
        assert_eq!(
            reconciled,
            (1..=EVENTS).collect::<Vec<_>>(),
            "events_after reconciles the complete non-terminal log despite push loss"
        );
    }
}
