//! A registry decorator that injects read, lease, terminal-write and worklist
//! faults and stale wake deliveries, holds a worklist page at a known point,
//! and counts point and lease reads, over any backend.

use lash_sansio::sync::MutexExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::model::{ProcessId, SessionId};
use super::super::registry::ProcessRegistry;
use super::super::registry_delegate::{
    delegate_process_observer_registry, delegate_process_registrar, delegate_process_retention,
    delegate_process_tool_intents,
};

/// Wraps a registry so a test can make its point reads of one process fail,
/// miss, or answer a stale record, can fail its lease, terminal,
/// external-reference and cancellation writes, can hand its wake-delivery
/// driver a claimed delivery the registry no longer holds, and can count how
/// its callers read processes and leases.
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
    injected_wakes: Arc<std::sync::Mutex<Vec<crate::WakeDelivery>>>,
    process_point_reads: Arc<AtomicUsize>,
    lease_point_reads: Arc<AtomicUsize>,
    lease_batch_reads: Arc<AtomicUsize>,
}

/// What a held external-reference write runs before it stops for good.
type ExternalRefWriteHold =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>;

#[derive(Default)]
struct ReadFaultPlan {
    error: Option<crate::PluginError>,
    error_after: Option<(usize, crate::PluginError)>,
    absent: bool,
    record_override: Option<crate::ProcessRecord>,
    pinned: Option<crate::ProcessRecord>,
    events_read_error: Option<crate::PluginError>,
    lease_claim_error: Option<crate::PluginError>,
    lease_renew_error: Option<crate::PluginError>,
    lease_release_error: Option<crate::PluginError>,
    terminal_write_error: Option<crate::PluginError>,
    terminal_write_outcome: Option<crate::ProcessCompletionOutcome>,
    external_ref_write_error: Option<crate::PluginError>,
    external_ref_write_hold: Option<ExternalRefWriteHold>,
    cancel_request_write_error: Option<crate::PluginError>,
    event_append_error: Option<crate::PluginError>,
    worklist_page_reads: Vec<WorklistPageRead>,
    worklist_page_errors: Option<(usize, std::collections::VecDeque<crate::PluginError>)>,
    worklist_page_pause: Option<WorklistPagePause>,
    registration_hold: Option<RegistrationHold>,
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

/// One worklist-page read the decorator saw: the page limit and the
/// continuation it was asked from.
pub type WorklistPageRead = (usize, Option<crate::ProcessWorklistCursor>);

/// Holds the next worklist-page read until the test resumes it.
#[derive(Clone)]
pub struct WorklistPagePause {
    reached: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}

impl WorklistPagePause {
    fn new() -> Self {
        Self {
            reached: Arc::new(tokio::sync::Notify::new()),
            resume: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Wait until the paused read has reached the decorator.
    pub async fn wait_until_validated(&self) {
        self.reached.notified().await;
    }

    /// Let the paused read through.
    pub fn resume(&self) {
        self.resume.notify_one();
    }

    async fn hold(&self) {
        self.reached.notify_one();
        self.resume.notified().await;
    }
}

impl ProcessRegistryFaults {
    pub fn new(inner: Arc<dyn ProcessRegistry>) -> Self {
        Self {
            inner,
            faults: Arc::default(),
            injected_wakes: Arc::default(),
            process_point_reads: Arc::default(),
            lease_point_reads: Arc::default(),
            lease_batch_reads: Arc::default(),
        }
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

    /// Every process-lease claim fails with `error` until cleared with `None`.
    pub fn set_process_lease_claim_error(&self, error: Option<crate::PluginError>) {
        self.faults.lock_recover().lease_claim_error = error;
    }

    /// Every process-lease renewal fails with `error` until cleared with
    /// `None`.
    pub fn set_process_lease_renew_error(&self, error: Option<crate::PluginError>) {
        self.faults.lock_recover().lease_renew_error = error;
    }

    /// Every process-lease release fails with `error` until cleared with
    /// `None`.
    pub fn set_process_lease_release_error(&self, error: Option<crate::PluginError>) {
        self.faults.lock_recover().lease_release_error = error;
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

    /// The next plain event append fails with `error`, once, without reaching
    /// the wrapped registry.
    pub fn fail_next_event_append(&self, error: crate::PluginError) {
        self.faults.lock_recover().event_append_error = Some(error);
    }

    /// After `successful_reads` more worklist-page reads pass, the following
    /// ones fail with `errors`, in order.
    pub fn set_worklist_page_errors(
        &self,
        successful_reads: usize,
        errors: Vec<crate::PluginError>,
    ) {
        self.faults.lock_recover().worklist_page_errors = Some((successful_reads, errors.into()));
    }

    /// Every worklist-page read that reached the decorator, in order.
    pub fn worklist_page_reads(&self) -> Vec<WorklistPageRead> {
        self.faults.lock_recover().worklist_page_reads.clone()
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

    /// Hold the next worklist-page read until the returned handle resumes it.
    pub fn pause_next_worklist_page(&self) -> WorklistPagePause {
        let pause = WorklistPagePause::new();
        self.faults.lock_recover().worklist_page_pause = Some(pause.clone());
        pause
    }

    /// The next claim of pending wake deliveries hands out `wake` first,
    /// already claimed, whether or not the wrapped registry holds it: the
    /// stale row a driver meets when a delivery outlives the incarnation that
    /// minted it.
    pub fn inject_claimed_wake_delivery(
        &self,
        wake: crate::ProcessWakeDelivery,
    ) -> Result<(), crate::PluginError> {
        let mut delivery = crate::WakeDelivery::pending(wake, self.inner.wake_delivery_config())?;
        delivery.disposition = crate::WakeDeliveryDisposition::Enqueuing {
            claim_token: format!("injected:{}", delivery.delivery_id),
        };
        delivery.attempts = 1;
        self.injected_wakes.lock_recover().push(delivery);
        Ok(())
    }

    /// How many single-process lease reads reached the registry.
    pub fn lease_point_reads(&self) -> usize {
        self.lease_point_reads.load(Ordering::SeqCst)
    }

    /// How many batched lease reads reached the registry.
    pub fn lease_batch_reads(&self) -> usize {
        self.lease_batch_reads.load(Ordering::SeqCst)
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

    fn lease_fault(
        &self,
        fault: impl FnOnce(&ReadFaultPlan) -> &Option<crate::PluginError>,
    ) -> Result<(), crate::PluginError> {
        fault(&self.faults.lock_recover())
            .clone()
            .map_or(Ok(()), Err)
    }

    fn take_events_read_fault(&self) -> Result<(), crate::PluginError> {
        self.faults
            .lock_recover()
            .events_read_error
            .take()
            .map_or(Ok(()), Err)
    }

    fn worklist_page_fault(&self) -> Result<(), crate::PluginError> {
        let mut plan = self.faults.lock_recover();
        let Some((successful_reads, errors)) = plan.worklist_page_errors.as_mut() else {
            return Ok(());
        };
        if *successful_reads > 0 {
            *successful_reads -= 1;
            return Ok(());
        }
        let error = errors.pop_front();
        if errors.is_empty() {
            plan.worklist_page_errors = None;
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
        self.inner.get_process_by_start_key(start_key).await
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

    async fn processes_changed_since(
        &self,
        cursor: crate::ProcessChangeCursor,
        limit: usize,
    ) -> Result<(Vec<crate::ProcessChange>, crate::ProcessChangeCursor), crate::PluginError> {
        self.inner.processes_changed_since(cursor, limit).await
    }

    async fn list_non_terminal_page(
        &self,
        limit: std::num::NonZeroUsize,
        continuation: Option<crate::ProcessWorklistCursor>,
    ) -> Result<crate::ProcessWorklistPage, crate::PluginError> {
        let pause = {
            let mut plan = self.faults.lock_recover();
            plan.worklist_page_reads
                .push((limit.get(), continuation.clone()));
            plan.worklist_page_pause.take()
        };
        if let Some(pause) = pause {
            pause.hold().await;
        }
        self.worklist_page_fault()?;
        self.inner.list_non_terminal_page(limit, continuation).await
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

    async fn list_parked_processes(
        &self,
        query: &crate::store::ProcessParkQuery,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        self.inner.list_parked_processes(query).await
    }

    async fn process_park_feed(
        &self,
        after: crate::store::ParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<crate::store::ParkFeedPage<crate::store::ProcessParkKey>, crate::PluginError> {
        self.inner.process_park_feed(after, limit).await
    }

    async fn summarize_parked_processes(
        &self,
    ) -> Result<crate::store::ParkSummary, crate::PluginError> {
        self.inner.summarize_parked_processes().await
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

delegate_process_observer_registry!(ProcessRegistryFaults, inner);

#[async_trait::async_trait]
impl super::super::registry_concerns::ProcessEventLog for ProcessRegistryFaults {
    async fn append_event(
        &self,
        process_id: &ProcessId,
        request: crate::ProcessEventAppendRequest,
    ) -> Result<crate::ProcessEventAppendReceipt, crate::PluginError> {
        let injected = self.faults.lock_recover().event_append_error.take();
        if let Some(error) = injected {
            return Err(error);
        }
        self.inner.append_event(process_id, request).await
    }

    async fn append_event_with_authority(
        &self,
        process_id: &ProcessId,
        request: crate::ProcessEventAppendRequest,
        authority: &crate::ProcessExecutionWriteAuthority,
    ) -> Result<crate::ProcessEventAppendReceipt, crate::PluginError> {
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
        self.inner
            .event_page_after(process_id, after_sequence, limit, mode)
            .await
    }

    async fn count_events_through(
        &self,
        process_id: &ProcessId,
        event_type: &str,
        up_to_sequence: u64,
    ) -> Result<u64, crate::PluginError> {
        self.take_events_read_fault()?;
        self.inner
            .count_events_through(process_id, event_type, up_to_sequence)
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

    async fn complete_process_with_lease(
        &self,
        lease: &crate::ProcessLease,
        await_output: crate::ProcessAwaitOutput,
    ) -> Result<crate::ProcessCompletionOutcome, crate::PluginError> {
        {
            let mut faults = self.faults.lock_recover();
            if let Some(error) = faults.terminal_write_error.clone() {
                return Err(error);
            }
            if let Some(outcome) = faults.terminal_write_outcome.take() {
                return Ok(outcome);
            }
        }
        self.inner
            .complete_process_with_lease(lease, await_output)
            .await
    }

    async fn record_parent_end(&self, parent: &crate::ScopeId) -> Result<(), crate::PluginError> {
        self.inner.record_parent_end(parent).await
    }

    async fn list_pending_parent_end_plans(
        &self,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<crate::ParentEndPlan>, crate::PluginError> {
        self.inner.list_pending_parent_end_plans(limit).await
    }

    async fn get_parent_end_plan(
        &self,
        parent: &crate::ScopeId,
    ) -> Result<Option<crate::ParentEndPlan>, crate::PluginError> {
        self.inner.get_parent_end_plan(parent).await
    }

    async fn list_parent_end_children(
        &self,
        parent: &crate::ScopeId,
        after: Option<&ProcessId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        self.inner
            .list_parent_end_children(parent, after, limit)
            .await
    }

    async fn settle_parent_end_plan(
        &self,
        parent: &crate::ScopeId,
    ) -> Result<(), crate::PluginError> {
        self.inner.settle_parent_end_plan(parent).await
    }

    async fn list_unrecorded_opener_parents(
        &self,
        after: Option<&str>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<crate::ScopeId>, crate::PluginError> {
        self.inner
            .list_unrecorded_opener_parents(after, limit)
            .await
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

    async fn request_process_abandon(
        &self,
        process_id: &ProcessId,
        request: crate::AbandonRequest,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.inner
            .request_process_abandon(process_id, request)
            .await
    }

    async fn record_caller_departure(
        &self,
        process_id: &ProcessId,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.inner.record_caller_departure(process_id).await
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

    async fn park_process_with_authority(
        &self,
        process_id: &ProcessId,
        park: crate::store::ProcessParkWrite,
        authority: &crate::ProcessExecutionWriteAuthority,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.inner
            .park_process_with_authority(process_id, park, authority)
            .await
    }

    async fn begin_parked_rerun_with_authority(
        &self,
        process_id: &ProcessId,
        authority: &crate::ProcessExecutionWriteAuthority,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.inner
            .begin_parked_rerun_with_authority(process_id, authority)
            .await
    }
}

delegate_process_tool_intents!(ProcessRegistryFaults, inner);

#[async_trait::async_trait]
impl super::super::registry_concerns::ProcessWakeOutbox for ProcessRegistryFaults {
    fn wake_delivery_config(&self) -> crate::WakeDeliveryConfig {
        self.inner.wake_delivery_config()
    }

    async fn claim_pending_wake_deliveries(
        &self,
        limit: usize,
    ) -> Result<Vec<crate::WakeDelivery>, crate::PluginError> {
        let mut claimed = {
            let mut injected = self.injected_wakes.lock_recover();
            let take = injected.len().min(limit);
            injected.drain(..take).collect::<Vec<_>>()
        };
        let remaining = limit - claimed.len();
        if remaining > 0 {
            claimed.extend(self.inner.claim_pending_wake_deliveries(remaining).await?);
        }
        Ok(claimed)
    }

    async fn list_wake_deliveries(
        &self,
        state: Option<crate::WakeDeliveryState>,
    ) -> Result<Vec<crate::WakeDelivery>, crate::PluginError> {
        self.inner.list_wake_deliveries(state).await
    }

    async fn wake_delivery_report(&self) -> Result<crate::WakeDeliveryReport, crate::PluginError> {
        self.inner.wake_delivery_report().await
    }

    async fn mark_wake_enqueued(
        &self,
        delivery_id: &str,
        claim_token: &str,
    ) -> Result<crate::WakeDeliveryClaimOutcome, crate::PluginError> {
        self.inner
            .mark_wake_enqueued(delivery_id, claim_token)
            .await
    }

    async fn discard_wake_delivery(
        &self,
        delivery_id: &str,
        claim_token: &str,
        reason: crate::WakeDiscardReason,
    ) -> Result<crate::WakeDeliveryClaimOutcome, crate::PluginError> {
        self.inner
            .discard_wake_delivery(delivery_id, claim_token, reason)
            .await
    }

    async fn redrive_wake_delivery(&self, delivery_id: &str) -> Result<(), crate::PluginError> {
        self.inner.redrive_wake_delivery(delivery_id).await
    }

    async fn defer_wake_delivery(
        &self,
        delivery_id: &str,
        claim_token: &str,
        next_attempt_at_ms: u64,
    ) -> Result<crate::WakeDeliveryClaimOutcome, crate::PluginError> {
        self.inner
            .defer_wake_delivery(delivery_id, claim_token, next_attempt_at_ms)
            .await
    }
}

#[async_trait::async_trait]
impl super::super::registry_concerns::ProcessLeases for ProcessRegistryFaults {
    async fn claim_process_lease(
        &self,
        process_id: &ProcessId,
        owner: &crate::LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<crate::ProcessLeaseClaimOutcome, crate::PluginError> {
        self.lease_fault(|plan| &plan.lease_claim_error)?;
        self.inner
            .claim_process_lease(process_id, owner, lease_ttl_ms)
            .await
    }

    async fn reclaim_process_lease(
        &self,
        process_id: &ProcessId,
        owner: &crate::LeaseOwnerIdentity,
        observed_holder: &crate::ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<crate::ProcessLeaseClaimOutcome, crate::PluginError> {
        self.lease_fault(|plan| &plan.lease_claim_error)?;
        self.inner
            .reclaim_process_lease(process_id, owner, observed_holder, lease_ttl_ms)
            .await
    }

    async fn renew_process_lease(
        &self,
        lease: &crate::ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<crate::ProcessLease, crate::PluginError> {
        self.lease_fault(|plan| &plan.lease_renew_error)?;
        self.inner.renew_process_lease(lease, lease_ttl_ms).await
    }

    async fn get_process_lease(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<crate::ProcessLease>, crate::PluginError> {
        self.lease_point_reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get_process_lease(process_id).await
    }

    async fn get_process_leases(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<Option<crate::ProcessLease>>, crate::PluginError> {
        self.lease_batch_reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get_process_leases(process_ids).await
    }

    async fn complete_process_lease(
        &self,
        completion: &crate::ProcessLeaseCompletion,
    ) -> Result<(), crate::PluginError> {
        self.lease_fault(|plan| &plan.lease_release_error)?;
        self.inner.complete_process_lease(completion).await
    }
}

delegate_process_retention!(ProcessRegistryFaults, inner);

impl super::super::registry_concerns::ProcessClockRebind for ProcessRegistryFaults {
    fn with_runtime_clock(&self, clock: Arc<dyn crate::Clock>) -> Option<Arc<dyn ProcessRegistry>> {
        self.inner.with_runtime_clock(clock).map(|inner| {
            Arc::new(Self {
                inner,
                faults: Arc::clone(&self.faults),
                injected_wakes: Arc::clone(&self.injected_wakes),
                process_point_reads: Arc::clone(&self.process_point_reads),
                lease_point_reads: Arc::clone(&self.lease_point_reads),
                lease_batch_reads: Arc::clone(&self.lease_batch_reads),
            }) as Arc<dyn ProcessRegistry>
        })
    }
}
