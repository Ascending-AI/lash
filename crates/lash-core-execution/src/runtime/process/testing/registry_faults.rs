//! A registry decorator that injects read faults and stale wake deliveries and
//! counts point and lease reads, over any backend.

// The delegation macros take each forwarding hook as a block, and these hooks
// only forward.
#![expect(
    unused_braces,
    reason = "the registry delegation macros require a block hook; these forward unchanged"
)]

use lash_sansio::sync::MutexExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::model::{ProcessId, SessionId};
use super::super::registry::ProcessRegistry;
use super::super::registry_delegate::{
    delegate_process_event_log, delegate_process_lifecycle, delegate_process_observer_registry,
    delegate_process_registrar, delegate_process_retention, delegate_process_tool_intents,
};

/// Wraps a registry so a test can make its point reads of one process fail,
/// miss, or answer a stale record, can hand its wake-delivery driver a claimed
/// delivery the registry no longer holds, and can count how its callers read
/// processes and leases.
///
/// Only point reads are faulted —
/// [`get_process`](super::super::registry_concerns::ProcessQuery::get_process)
/// and the exact-incarnation reads built on it — and only for callers that
/// read through this decorator: the wrapped backend's own writes never see the
/// faults. Every other operation forwards unchanged.
#[derive(Clone)]
pub struct ProcessRegistryFaults {
    inner: Arc<dyn ProcessRegistry>,
    faults: Arc<std::sync::Mutex<ReadFaultPlan>>,
    injected_wakes: Arc<std::sync::Mutex<Vec<crate::WakeDelivery>>>,
    process_point_reads: Arc<AtomicUsize>,
    lease_point_reads: Arc<AtomicUsize>,
    lease_batch_reads: Arc<AtomicUsize>,
}

#[derive(Default)]
struct ReadFaultPlan {
    error: Option<crate::PluginError>,
    error_after: Option<(usize, crate::PluginError)>,
    absent: bool,
    record_override: Option<crate::ProcessRecord>,
    pinned: Option<crate::ProcessRecord>,
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
}

/// `resolve_process_ref` and `get_process_ref` keep the trait's provided
/// bodies, which read through [`get_process`](Self::get_process): an exact
/// incarnation read sees the same faults a point read does.
#[async_trait::async_trait]
impl super::super::registry_concerns::ProcessQuery for ProcessRegistryFaults {
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
}

delegate_process_registrar!(
    ProcessRegistryFaults,
    inner,
    registration | _faults,
    _process_id,
    forwarded | { forwarded.await },
    event | _faults,
    _process_id,
    forwarded | { forwarded.await }
);

delegate_process_observer_registry!(ProcessRegistryFaults, inner);

delegate_process_event_log!(
    ProcessRegistryFaults,
    inner,
    event | _faults,
    _process_id,
    forwarded | { forwarded.await }
);

delegate_process_lifecycle!(
    ProcessRegistryFaults,
    inner,
    event | _faults,
    _process_id,
    forwarded | { forwarded.await }
);

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
        self.inner
            .reclaim_process_lease(process_id, owner, observed_holder, lease_ttl_ms)
            .await
    }

    async fn renew_process_lease(
        &self,
        lease: &crate::ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<crate::ProcessLease, crate::PluginError> {
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
