//! A registry decorator that injects read, terminal-write and page-read
//! faults, holds a non-terminal page at a known point,
//! and counts point reads, over any backend.

use lash_sansio::sync::MutexExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::model::{ProcessId, SessionId};
use super::super::registry::ProcessRegistry;
use super::super::registry_delegate::{
    delegate_process_observer_registry, delegate_process_registrar, delegate_process_tool_intents,
};

/// Wraps a registry so a test can make its point reads of one process fail,
/// miss, or answer a stale record, can fail terminal,
/// external-reference and cancellation writes, and can count how its callers
/// read processes.
///
/// Reads are faulted at the point reads —
/// [`get_process`](super::super::registry_concerns::ProcessQuery::get_process)
/// and the exact-incarnation reads built on it — and every fault applies only
/// to callers that go through this decorator: the wrapped backend's own
/// writes never see the faults. Every other operation forwards unchanged.
#[derive(Clone)]
pub struct ProcessRegistryFaults {
    inner: Arc<dyn ProcessRegistry>,
    faults: Arc<std::sync::Mutex<ReadFaultPlan>>,
    process_point_reads: Arc<AtomicUsize>,
}

/// What a held external-reference write runs before it stops for good.
type ExternalRefWriteHold =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>;

#[derive(Default)]
struct ReadFaultPlan {
    delete_error: Option<crate::PluginError>,
    error: Option<crate::PluginError>,
    error_after: Option<(usize, crate::PluginError)>,
    absent: bool,
    record_override: Option<crate::ProcessRecord>,
    pinned: Option<crate::ProcessRecord>,
    events_read_error: Option<crate::PluginError>,
    terminal_write_error: Option<crate::PluginError>,
    terminal_write_outcome: Option<crate::ProcessCompletionOutcome>,
    external_ref_write_error: Option<crate::PluginError>,
    external_ref_write_hold: Option<ExternalRefWriteHold>,
    cancel_request_write_error: Option<crate::PluginError>,
    event_append_error: Option<crate::PluginError>,
    non_terminal_page_reads: Vec<NonTerminalPageRead>,
    non_terminal_page_errors: Option<(usize, std::collections::VecDeque<crate::PluginError>)>,
    non_terminal_page_pause: Option<NonTerminalPagePause>,
    event_page_pause: Option<NonTerminalPagePause>,
    registration_hold: Option<RegistrationHold>,
    registration_pause: Option<NonTerminalPagePause>,
    consumer_release_pause: Option<NonTerminalPagePause>,
    consumer_released_pause: Option<NonTerminalPagePause>,
    start_key_read_pause: Option<NonTerminalPagePause>,
}

/// Where a held registration stops: before it reaches the wrapped registry,
/// or once the wrapped registry committed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistrationHoldPoint {
    BeforeRegistering,
    AfterRegistering,
}

/// One armed registration hold: where it stops the next registration, and
/// what it tells the test when it gets there.
#[derive(Clone)]
struct RegistrationHold {
    point: RegistrationHoldPoint,
    reached: Arc<dyn Fn() + Send + Sync>,
}

/// One non-terminal-page read the decorator saw: the page limit and the
/// continuation it was asked from.
pub type NonTerminalPageRead = (usize, Option<crate::ProcessRegistryCursor>);

/// Holds the next non-terminal-page read until the test resumes it.
#[derive(Clone)]
pub struct NonTerminalPagePause {
    gate: Arc<crate::testing::Gate>,
}

impl NonTerminalPagePause {
    fn new() -> Self {
        Self {
            gate: Arc::new(crate::testing::Gate::new("process registry pause")),
        }
    }

    /// Wait until the paused read has reached the decorator.
    pub async fn wait_until_validated(&self) {
        self.gate.reached(1).await;
    }

    /// Let the paused read through.
    pub fn resume(&self) {
        self.gate.open_all();
    }

    async fn hold(&self) {
        self.gate.pass().await;
    }
}

impl ProcessRegistryFaults {
    pub fn new(inner: Arc<dyn ProcessRegistry>) -> Self {
        Self {
            inner,
            faults: Arc::default(),
            process_point_reads: Arc::default(),
        }
    }

    /// The next session cleanup fails before changing process state.
    pub fn fail_next_session_delete(&self, error: crate::PluginError) {
        self.faults.lock_recover().delete_error = Some(error);
    }

    async fn delete_session_faulted(
        &self,
        session_id: &SessionId,
    ) -> Result<crate::ProcessSessionDeleteReport, crate::PluginError> {
        let error = self.faults.lock_recover().delete_error.take();
        if let Some(error) = error {
            return Err(error);
        }
        self.inner.delete_session_process_state(session_id).await
    }

    /// Every point read fails with `error` until cleared with `None`.
    pub fn set_process_read_error(&self, error: Option<crate::PluginError>) {
        self.faults.lock_recover().error = error;
    }

    /// The point read after `successful_reads` more successful ones fails
    /// with `error`, once.
    pub fn set_process_read_error_after(&self, successful_reads: usize, error: crate::PluginError) {
        self.faults.lock_recover().error_after = Some((successful_reads, error));
    }

    /// Every point read answers "absent" until cleared.
    pub fn set_process_read_absent(&self, absent: bool) {
        self.faults.lock_recover().absent = absent;
    }

    /// The next point read answers `record`, once.
    pub fn set_process_read_override(&self, record: crate::ProcessRecord) {
        self.faults.lock_recover().record_override = Some(record);
    }

    /// Every point read answers `record` until cleared with `None`, without
    /// reaching the wrapped registry. A reader polling a pinned record does no
    /// backend I/O, so a paused-clock test can step its cadence without that
    /// I/O idling the runtime into auto-advancing the clock.
    pub fn set_process_read_pinned(&self, record: Option<crate::ProcessRecord>) {
        self.faults.lock_recover().pinned = record;
    }

    /// How many point reads reached this decorator, faulted or forwarded.
    pub fn process_point_reads(&self) -> usize {
        self.process_point_reads.load(Ordering::SeqCst)
    }

    /// The next event-history read fails with `error`, once.
    pub fn set_process_events_read_error(&self, error: crate::PluginError) {
        self.faults.lock_recover().events_read_error = Some(error);
    }

    /// Every fenced terminal write fails with `error` until cleared with
    /// `None`.
    pub fn set_process_terminal_write_error(&self, error: Option<crate::PluginError>) {
        self.faults.lock_recover().terminal_write_error = error;
    }

    /// The next fenced terminal write answers `outcome` without writing, once.
    pub fn set_process_terminal_write_outcome(&self, outcome: crate::ProcessCompletionOutcome) {
        self.faults.lock_recover().terminal_write_outcome = Some(outcome);
    }

    /// The next external-reference write fails with `error`, once, without
    /// reaching the wrapped registry.
    pub fn fail_next_external_ref_write(&self, error: crate::PluginError) {
        self.faults.lock_recover().external_ref_write_error = Some(error);
    }

    /// The next external-reference write awaits `reached` and never returns,
    /// once, without reaching the wrapped registry: the execution that issued
    /// it dies there, after the start registered and scheduled its process
    /// and before the start answered.
    pub fn hold_next_external_ref_write<F>(&self, reached: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.faults.lock_recover().external_ref_write_hold =
            Some(Box::new(move || Box::pin(reached)));
    }

    /// The next cancellation request fails with `error`, once, without
    /// reaching the wrapped registry.
    pub fn fail_next_cancel_request(&self, error: crate::PluginError) {
        self.faults.lock_recover().cancel_request_write_error = Some(error);
    }

    /// The next event append fails with `error`, once, without reaching the
    /// wrapped registry.
    pub fn fail_next_event_append(&self, error: crate::PluginError) {
        self.faults.lock_recover().event_append_error = Some(error);
    }

    /// After `successful_reads` more non-terminal-page reads pass, the following
    /// ones fail with `errors`, in order.
    pub fn set_non_terminal_page_errors(
        &self,
        successful_reads: usize,
        errors: Vec<crate::PluginError>,
    ) {
        self.faults.lock_recover().non_terminal_page_errors =
            Some((successful_reads, errors.into()));
    }

    /// Every non-terminal-page read that reached the decorator, in order.
    pub fn non_terminal_page_reads(&self) -> Vec<NonTerminalPageRead> {
        self.faults.lock_recover().non_terminal_page_reads.clone()
    }

    /// The next registration through this decorator stops at `point` and
    /// never returns, after calling `reached`, once: the point where a test
    /// kills the registering attempt, as a crash between the registry write
    /// and what follows it would. Every later registration forwards
    /// unchanged.
    pub fn hold_next_registration(
        &self,
        point: RegistrationHoldPoint,
        reached: Arc<dyn Fn() + Send + Sync>,
    ) {
        self.faults.lock_recover().registration_hold = Some(RegistrationHold { point, reached });
    }

    /// Hold the next registration before it reaches the wrapped registry
    /// until the returned handle resumes it; it then registers as usual.
    pub fn pause_next_registration(&self) -> NonTerminalPagePause {
        let pause = NonTerminalPagePause::new();
        self.faults.lock_recover().registration_pause = Some(pause.clone());
        pause
    }

    /// Hold the answer of the next start-key read until the returned handle
    /// resumes it: the read reaches the wrapped registry first, so its caller
    /// acts on what the key held then, however the key changed meanwhile.
    pub fn pause_next_start_key_read(&self) -> NonTerminalPagePause {
        let pause = NonTerminalPagePause::new();
        self.faults.lock_recover().start_key_read_pause = Some(pause.clone());
        pause
    }

    /// Hold the next consumer-hold release until the returned handle resumes
    /// it: the instant a parked call has consumed its child's terminal and
    /// not yet released the child's row for pruning (ADR 0116 §3.6).
    pub fn pause_next_consumer_release(&self) -> NonTerminalPagePause {
        let pause = NonTerminalPagePause::new();
        self.faults.lock_recover().consumer_release_pause = Some(pause.clone());
        pause
    }

    /// Hold the answer of the next consumer-hold release, once the wrapped
    /// registry committed it, until the returned handle resumes it: the
    /// instant a parked call's child is prunable and the call has not yet
    /// settled (ADR 0116 §3.6).
    pub fn pause_after_next_consumer_release(&self) -> NonTerminalPagePause {
        let pause = NonTerminalPagePause::new();
        self.faults.lock_recover().consumer_released_pause = Some(pause.clone());
        pause
    }

    /// Hold the next non-terminal-page read until the returned handle resumes it.
    pub fn pause_next_non_terminal_page(&self) -> NonTerminalPagePause {
        let pause = NonTerminalPagePause::new();
        self.faults.lock_recover().non_terminal_page_pause = Some(pause.clone());
        pause
    }

    /// Hold the next event-page read (`event_page_after`, and the reads
    /// built on it) until the returned handle resumes it.
    pub fn pause_next_event_page(&self) -> NonTerminalPagePause {
        let pause = NonTerminalPagePause::new();
        self.faults.lock_recover().event_page_pause = Some(pause.clone());
        pause
    }

    fn faulted_read(&self) -> Option<Result<Option<crate::ProcessRecord>, crate::PluginError>> {
        let mut plan = self.faults.lock_recover();
        if let Some(error) = plan.error.clone() {
            return Some(Err(error));
        }
        if let Some((remaining, error)) = plan.error_after.as_mut() {
            if *remaining == 0 {
                let error = error.clone();
                plan.error_after = None;
                return Some(Err(error));
            }
            *remaining -= 1;
        }
        if plan.absent {
            return Some(Ok(None));
        }
        if let Some(record) = plan.record_override.take() {
            return Some(Ok(Some(record)));
        }
        plan.pinned.clone().map(|record| Ok(Some(record)))
    }

    fn take_events_read_fault(&self) -> Result<(), crate::PluginError> {
        self.faults
            .lock_recover()
            .events_read_error
            .take()
            .map_or(Ok(()), Err)
    }

    fn non_terminal_page_fault(&self) -> Result<(), crate::PluginError> {
        let mut plan = self.faults.lock_recover();
        let Some((successful_reads, errors)) = plan.non_terminal_page_errors.as_mut() else {
            return Ok(());
        };
        if *successful_reads > 0 {
            *successful_reads -= 1;
            return Ok(());
        }
        let error = errors.pop_front();
        if errors.is_empty() {
            plan.non_terminal_page_errors = None;
        }
        error.map_or(Ok(()), Err)
    }
}

/// `resolve_process_ref` and `get_process_ref` keep the trait's provided
/// bodies, which read through [`get_process`](Self::get_process): an exact
/// incarnation read sees the same faults a point read does.
#[async_trait::async_trait]
impl super::super::registry_concerns::ProcessQuery for ProcessRegistryFaults {
    async fn get_process_by_start_key(
        &self,
        start_key: &crate::StartKey,
    ) -> Result<Option<crate::ProcessRecord>, crate::PluginError> {
        let read = self.inner.get_process_by_start_key(start_key).await;
        let pause = self.faults.lock_recover().start_key_read_pause.take();
        if let Some(pause) = pause {
            pause.hold().await;
        }
        read
    }

    async fn get_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<crate::ProcessRecord>, crate::PluginError> {
        self.process_point_reads.fetch_add(1, Ordering::SeqCst);
        if let Some(faulted) = self.faulted_read() {
            return faulted;
        }
        self.inner.get_process(process_id).await
    }

    async fn list_processes(
        &self,
        filter: &crate::ProcessListFilter,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        self.inner.list_processes(filter).await
    }

    async fn list_processes_page(
        &self,
        filter: &crate::ProcessListFilter,
        limit: std::num::NonZeroUsize,
        continuation: Option<crate::ProcessRosterCursor>,
    ) -> Result<crate::ProcessRosterRecords, crate::PluginError> {
        self.inner
            .list_processes_page(filter, limit, continuation)
            .await
    }

    async fn process_change_bounds(
        &self,
    ) -> Result<crate::ProcessChangeBounds, crate::PluginError> {
        self.inner.process_change_bounds().await
    }

    async fn processes_changed_since(
        &self,
        cursor: crate::ProcessChangeCursor,
        limit: usize,
    ) -> Result<(Vec<crate::ProcessChange>, crate::ProcessChangeCursor), crate::PluginError> {
        self.inner.processes_changed_since(cursor, limit).await
    }

    async fn list_non_terminal_processes_page(
        &self,
        limit: std::num::NonZeroUsize,
        continuation: Option<crate::ProcessRegistryCursor>,
    ) -> Result<crate::NonTerminalProcessPage, crate::PluginError> {
        let pause = {
            let mut plan = self.faults.lock_recover();
            plan.non_terminal_page_reads
                .push((limit.get(), continuation.clone()));
            plan.non_terminal_page_pause.take()
        };
        if let Some(pause) = pause {
            pause.hold().await;
        }
        self.non_terminal_page_fault()?;
        self.inner
            .list_non_terminal_processes_page(limit, continuation)
            .await
    }

    async fn filter_unregistered_process_ids(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<ProcessId>, crate::PluginError> {
        self.inner
            .filter_unregistered_process_ids(process_ids)
            .await
    }

    async fn filter_tombstoned_process_ids(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<ProcessId>, crate::PluginError> {
        self.inner.filter_tombstoned_process_ids(process_ids).await
    }

    async fn live_reference_summary(
        &self,
    ) -> Result<Vec<crate::ProcessLiveReferenceView>, crate::PluginError> {
        self.inner.live_reference_summary().await
    }

    async fn count_non_terminal_processes(&self) -> Result<usize, crate::PluginError> {
        self.inner.count_non_terminal_processes().await
    }
}

impl crate::FleetFormatStore for ProcessRegistryFaults {
    fn fleet_format(&self) -> crate::FleetFormat {
        self.inner.fleet_format()
    }
}

delegate_process_registrar!(
    ProcessRegistryFaults,
    inner,
    registration | faults,
    forwarded | {
        let pause = faults.faults.lock_recover().registration_pause.take();
        if let Some(pause) = pause {
            pause.hold().await;
        }
        let hold = faults.faults.lock_recover().registration_hold.take();
        match hold {
            None => forwarded.await,
            Some(hold) => {
                if hold.point == RegistrationHoldPoint::AfterRegistering {
                    forwarded.await?;
                } else {
                    drop(forwarded);
                }
                (hold.reached)();
                std::future::pending().await
            }
        }
    },
    event | faults,
    _process_id,
    forwarded | {
        let hold = faults.faults.lock_recover().external_ref_write_hold.take();
        if let Some(reached) = hold {
            drop(forwarded);
            reached().await;
            return std::future::pending().await;
        }
        let injected = faults.faults.lock_recover().external_ref_write_error.take();
        match injected {
            Some(error) => Err(error),
            None => forwarded.await,
        }
    }
);

delegate_process_observer_registry!(ProcessRegistryFaults, inner, delete_session_faulted);

#[async_trait::async_trait]
impl super::super::registry_concerns::ProcessEventLog for ProcessRegistryFaults {
    async fn append_event_with_authority(
        &self,
        process_id: &ProcessId,
        request: crate::ProcessEventAppendRequest,
        authority: &crate::ProcessExecutionWriteAuthority,
    ) -> Result<crate::ProcessEventAppendReceipt, crate::PluginError> {
        let injected = self.faults.lock_recover().event_append_error.take();
        if let Some(error) = injected {
            return Err(error);
        }
        self.inner
            .append_event_with_authority(process_id, request, authority)
            .await
    }

    async fn append_events(
        &self,
        process_id: &ProcessId,
        requests: Vec<crate::ProcessEventAppendRequest>,
        authority: &crate::ProcessExecutionWriteAuthority,
    ) -> Result<Vec<crate::ProcessEventAppendReceipt>, crate::PluginError> {
        let injected = self.faults.lock_recover().event_append_error.take();
        if let Some(error) = injected {
            return Err(error);
        }
        self.inner
            .append_events(process_id, requests, authority)
            .await
    }

    // `event_page` keeps the trait's provided body, which reads through
    // `event_page_after`: a by-id read sees the same fault.
    async fn event_page_after(
        &self,
        process_id: &crate::ProcessId,
        after_sequence: u64,
        limit: std::num::NonZeroUsize,
        mode: crate::ProcessEventQueryMode,
    ) -> Result<crate::ProcessEventReadOutcome<crate::ProcessEventPage>, crate::PluginError> {
        self.take_events_read_fault()?;
        let pause = self.faults.lock_recover().event_page_pause.take();
        if let Some(pause) = pause {
            pause.hold().await;
        }
        self.inner
            .event_page_after(process_id, after_sequence, limit, mode)
            .await
    }

    async fn recent_events(
        &self,
        process_id: &ProcessId,
        limit: usize,
    ) -> Result<Vec<crate::ProcessEvent>, crate::PluginError> {
        self.take_events_read_fault()?;
        self.inner.recent_events(process_id, limit).await
    }
}

#[async_trait::async_trait]
impl super::super::registry_concerns::ProcessLifecycle for ProcessRegistryFaults {
    async fn complete_process(
        &self,
        process_id: &ProcessId,
        await_output: crate::ProcessAwaitOutput,
        authority: crate::ProcessCompletionAuthority,
    ) -> Result<crate::ProcessCompletionOutcome, crate::PluginError> {
        self.inner
            .complete_process(process_id, await_output, authority)
            .await
    }

    async fn complete_process_with_prelude(
        &self,
        process_id: &ProcessId,
        await_output: crate::ProcessAwaitOutput,
        prelude: Vec<crate::ProcessEventAppendRequest>,
        authority: crate::ProcessCompletionAuthority,
    ) -> Result<crate::ProcessCompletionOutcome, crate::PluginError> {
        self.inner
            .complete_process_with_prelude(process_id, await_output, prelude, authority)
            .await
    }

    async fn get_parent_end_plan(
        &self,
        parent: &crate::ScopeId,
    ) -> Result<Option<crate::ParentEndPlan>, crate::PluginError> {
        self.inner.get_parent_end_plan(parent).await
    }

    async fn record_first_started_with_authority(
        &self,
        process_id: &ProcessId,
        started: crate::ProcessStarted,
        authority: &crate::ProcessExecutionWriteAuthority,
    ) -> Result<crate::ProcessStartOutcome, crate::PluginError> {
        self.inner
            .record_first_started_with_authority(process_id, started, authority)
            .await
    }

    async fn request_process_cancel(
        &self,
        process_id: &crate::ProcessId,
        origin: crate::CancelOrigin,
        requester: String,
        attribution: Option<crate::RuntimeReplayAttribution>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.request_process_cancel_reporting_realization(
            process_id,
            origin,
            requester,
            attribution,
        )
        .await
        .map(|(record, _)| record)
    }

    async fn request_process_cancel_reporting_realization(
        &self,
        process_id: &crate::ProcessId,
        origin: crate::CancelOrigin,
        requester: String,
        attribution: Option<crate::RuntimeReplayAttribution>,
    ) -> Result<(crate::ProcessRecord, crate::StoreRealization), crate::PluginError> {
        let injected = self.faults.lock_recover().cancel_request_write_error.take();
        if let Some(error) = injected {
            return Err(error);
        }
        self.inner
            .request_process_cancel_reporting_realization(
                process_id,
                origin,
                requester,
                attribution,
            )
            .await
    }

    async fn set_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        wait: crate::WaitState,
        prelude: Vec<crate::ProcessEventAppendRequest>,
        authority: &crate::ProcessExecutionWriteAuthority,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.inner
            .set_process_wait_with_authority(process_id, wait, prelude, authority)
            .await
    }

    async fn clear_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        prelude: Vec<crate::ProcessEventAppendRequest>,
        authority: &crate::ProcessExecutionWriteAuthority,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.inner
            .clear_process_wait_with_authority(process_id, prelude, authority)
            .await
    }
}

delegate_process_tool_intents!(ProcessRegistryFaults, inner);

#[async_trait::async_trait]
impl super::super::registry_concerns::ProcessRetention for ProcessRegistryFaults {
    async fn compact_process_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: crate::ProjectionWatermark,
    ) -> Result<usize, crate::PluginError> {
        self.inner
            .compact_process_tombstones(cutoff_epoch_ms, watermark)
            .await
    }

    async fn release_process_events(
        &self,
        process_id: &crate::ProcessId,
        through: u64,
    ) -> Result<crate::ProcessEventRelease, crate::PluginError> {
        self.inner.release_process_events(process_id, through).await
    }

    async fn prune_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<crate::ProcessListFilter>,
        watermark: crate::ProjectionWatermark,
    ) -> Result<crate::ProcessPruneReport, crate::PluginError> {
        self.inner
            .prune_terminal_processes(cutoff_epoch_ms, filter, watermark)
            .await
    }

    async fn prunable_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<crate::ProcessListFilter>,
        watermark: crate::ProjectionWatermark,
    ) -> Result<Vec<ProcessId>, crate::PluginError> {
        self.inner
            .prunable_terminal_processes(cutoff_epoch_ms, filter, watermark)
            .await
    }

    async fn release_consumer_hold(
        &self,
        process_id: &ProcessId,
        key: &str,
    ) -> Result<(), crate::PluginError> {
        let pause = self.faults.lock_recover().consumer_release_pause.take();
        if let Some(pause) = pause {
            pause.hold().await;
        }
        let released = self.inner.release_consumer_hold(process_id, key).await;
        let pause = self.faults.lock_recover().consumer_released_pause.take();
        if let Some(pause) = pause {
            pause.hold().await;
        }
        released
    }

    async fn abandon_consumer_hold(
        &self,
        key: &str,
        owner: &crate::ScopeId,
    ) -> Result<Vec<ProcessId>, crate::PluginError> {
        self.inner.abandon_consumer_hold(key, owner).await
    }
}

impl super::super::registry_concerns::ProcessClockRebind for ProcessRegistryFaults {
    fn with_runtime_clock(&self, clock: Arc<dyn crate::Clock>) -> Option<Arc<dyn ProcessRegistry>> {
        self.inner.with_runtime_clock(clock).map(|inner| {
            Arc::new(Self {
                inner,
                faults: Arc::clone(&self.faults),
                process_point_reads: Arc::clone(&self.process_point_reads),
            }) as Arc<dyn ProcessRegistry>
        })
    }
}
