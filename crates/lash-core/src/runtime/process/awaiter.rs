use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use super::events::{
    ProcessAwaitOutput, ProcessCompletionAuthority, ProcessEvent, ProcessEventAppendReceipt,
    ProcessEventAppendRequest,
};
use super::model::{
    AbandonRequest, ProcessChange, ProcessChangeCursor, ProcessCompletionOutcome,
    ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessLease, ProcessLeaseClaimOutcome,
    ProcessLeaseCompletion, ProcessListFilter, ProcessObserverBy, ProcessRecord,
    ProcessRegistration, ProcessSessionDeleteReport, ProcessStarted, SessionId, WaitState,
};
use super::registry::{ProcessPruneReport, ProcessRegistry, ProjectionWatermark};
use crate::PluginError;

mod attach;
mod change_hub;
#[path = "awaiter/event_sink.rs"]
mod event_sink;
mod registry_support;
pub use attach::ProcessAttach;
pub use change_hub::ProcessChangeHub;
pub use event_sink::ProcessEventSink;

const AWAIT_BACKOFF_MIN: Duration = Duration::from_millis(25);
const AWAIT_BACKOFF_MAX: Duration = Duration::from_secs(1);

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

/// Wrap `inner` in a change-publishing registry decorator with no event sink.
///
/// The decorated handle publishes change ticks to the returned
/// [`ProcessChangeHub`]. Use [`watch_process_registry_with_sink`] to also feed a
/// host-facing [`ProcessEventSink`].
pub fn watch_process_registry(
    inner: Arc<dyn ProcessRegistry>,
) -> (Arc<dyn ProcessRegistry>, ProcessChangeHub) {
    watch_process_registry_with_sink(inner, None)
}

/// Wrap `inner` in a change-publishing registry decorator, optionally
/// installing a [`ProcessEventSink`] that receives every appended event.
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
    /// without database polling. This is what [`watch_process_registry`]
    /// wraps `registry` to provide.
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
        let record = match self.registry.get_process(process_id).await {
            Ok(Some(record)) => record,
            Ok(None) => {
                return Err(PluginError::Session(format!(
                    "unknown process `{process_id}`"
                )));
            }
            Err(PluginError::ProcessNoLongerRetained {
                terminal_label,
                pruned_at_ms,
            }) => {
                return Ok(Some(ProcessAwaitOutput::NoLongerRetained {
                    terminal_label,
                    pruned_at_ms,
                }));
            }
            Err(error) => return Err(error),
        };
        // A caller-departed row is the one non-terminal state no writer will
        // ever resolve, so parking on it is a guaranteed permanent park. Refuse
        // with the typed error instead (FIG-1383).
        if record.status == crate::ProcessStatus::CallerDeparted {
            return Err(PluginError::ProcessCallerDeparted {
                process_id: process_id.to_string(),
            });
        }
        Ok(record.outcome)
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
impl ProcessRegistry for WatchedProcessRegistry {
    fn wake_delivery_config(&self) -> super::WakeDeliveryConfig {
        self.inner.wake_delivery_config()
    }

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

    async fn add_observer(
        &self,
        session_id: &str,
        process_id: &str,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        self.inner.add_observer(session_id, process_id, by).await
    }

    async fn remove_observer(
        &self,
        session_id: &str,
        process_id: &str,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        self.inner.remove_observer(session_id, process_id, by).await
    }

    async fn transfer_observers(
        &self,
        from_session_id: &str,
        to_session_id: &str,
        process_ids: &[String],
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        self.inner
            .transfer_observers(from_session_id, to_session_id, process_ids, by)
            .await
    }

    async fn list_observed_by(&self, session_id: &str) -> Result<Vec<ProcessRecord>, PluginError> {
        self.inner.list_observed_by(session_id).await
    }

    async fn observers_for_process(&self, process_id: &str) -> Result<Vec<SessionId>, PluginError> {
        self.inner.observers_for_process(process_id).await
    }

    async fn retarget_subscription(
        &self,
        process_id: &str,
        target: Option<&str>,
    ) -> Result<(), PluginError> {
        self.inner.retarget_subscription(process_id, target).await
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
    async fn admit_tool_intent_submission(
        &self,
        submission: crate::ToolIntentSubmissionRecord,
    ) -> Result<crate::ToolIntentSubmissionAdmission, PluginError> {
        self.inner.admit_tool_intent_submission(submission).await
    }

    async fn complete_tool_intent_submission(
        &self,
        replay_key: &str,
        outcome: crate::ToolIntentExecutionOutcome,
    ) -> Result<crate::ToolIntentSubmissionRecord, PluginError> {
        self.inner
            .complete_tool_intent_submission(replay_key, outcome)
            .await
    }

    async fn pending_tool_intent_parent_end(
        &self,
        session_id: &str,
        execution_scope_id: &str,
    ) -> Result<Vec<crate::ToolIntentSubmissionRecord>, PluginError> {
        self.inner
            .pending_tool_intent_parent_end(session_id, execution_scope_id)
            .await
    }

    async fn complete_tool_intent_parent_end(&self, replay_key: &str) -> Result<(), PluginError> {
        self.inner.complete_tool_intent_parent_end(replay_key).await
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

    async fn get_process(&self, process_id: &str) -> Result<Option<ProcessRecord>, PluginError> {
        self.inner.get_process(process_id).await
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
    ) -> Result<(Vec<ProcessChange>, ProcessChangeCursor), PluginError> {
        self.inner.processes_changed_since(cursor, limit).await
    }

    async fn filter_unregistered_process_ids(
        &self,
        process_ids: &[String],
    ) -> Result<Vec<String>, PluginError> {
        self.inner
            .filter_unregistered_process_ids(process_ids)
            .await
    }

    async fn filter_tombstoned_process_ids(
        &self,
        process_ids: &[String],
    ) -> Result<Vec<String>, PluginError> {
        self.inner.filter_tombstoned_process_ids(process_ids).await
    }

    async fn compact_process_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: ProjectionWatermark,
        trigger_store: Option<&dyn crate::TriggerStore>,
    ) -> Result<usize, PluginError> {
        self.inner
            .compact_process_tombstones(cutoff_epoch_ms, watermark, trigger_store)
            .await
    }

    async fn claim_pending_wake_deliveries(
        &self,
        limit: usize,
    ) -> Result<Vec<super::WakeDelivery>, PluginError> {
        self.inner.claim_pending_wake_deliveries(limit).await
    }

    async fn list_wake_deliveries(
        &self,
        state: Option<super::WakeDeliveryState>,
    ) -> Result<Vec<super::WakeDelivery>, PluginError> {
        self.inner.list_wake_deliveries(state).await
    }

    async fn wake_delivery_report(&self) -> Result<super::WakeDeliveryReport, PluginError> {
        self.inner.wake_delivery_report().await
    }

    async fn mark_wake_enqueued(
        &self,
        delivery_id: &str,
        claim_token: &str,
    ) -> Result<super::WakeDeliveryClaimOutcome, PluginError> {
        self.inner
            .mark_wake_enqueued(delivery_id, claim_token)
            .await
    }

    async fn discard_wake_delivery(
        &self,
        delivery_id: &str,
        claim_token: &str,
        reason: super::WakeDiscardReason,
    ) -> Result<super::WakeDeliveryClaimOutcome, PluginError> {
        self.inner
            .discard_wake_delivery(delivery_id, claim_token, reason)
            .await
    }

    async fn redrive_wake_delivery(&self, delivery_id: &str) -> Result<(), PluginError> {
        self.inner.redrive_wake_delivery(delivery_id).await
    }

    async fn defer_wake_delivery(
        &self,
        delivery_id: &str,
        claim_token: &str,
        next_attempt_at_ms: u64,
    ) -> Result<super::WakeDeliveryClaimOutcome, PluginError> {
        self.inner
            .defer_wake_delivery(delivery_id, claim_token, next_attempt_at_ms)
            .await
    }

    async fn list_non_terminal_page(
        &self,
        limit: std::num::NonZeroUsize,
        continuation: Option<super::ProcessWorklistCursor>,
    ) -> Result<super::ProcessWorklistPage, PluginError> {
        self.inner.list_non_terminal_page(limit, continuation).await
    }

    async fn live_reference_summary(
        &self,
    ) -> Result<Vec<super::references::ProcessLiveReferenceView>, PluginError> {
        self.inner.live_reference_summary().await
    }

    async fn count_non_terminal_processes(&self) -> Result<usize, PluginError> {
        self.inner.count_non_terminal_processes().await
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
        watermark: ProjectionWatermark,
    ) -> Result<ProcessPruneReport, PluginError> {
        // No hub bump: pruned rows are terminal, so any waiter on them resolved
        // long ago (terminal state is durable and observed via the await seam).
        self.inner
            .prune_terminal_processes(cutoff_epoch_ms, filter, watermark)
            .await
    }
}

#[cfg(test)]
mod tests;
