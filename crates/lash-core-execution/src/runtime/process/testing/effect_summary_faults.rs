//! A registry decorator that simulates a worker crash before a run's
//! effect-summary batch commits.

// The delegation macros take each forwarding hook as a block, and these hooks
// only forward.
#![expect(
    unused_braces,
    reason = "the registry delegation macros require a block hook; these forward unchanged"
)]

use lash_sansio::sync::MutexExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::runtime::process::registry::ProcessRegistry;
use crate::runtime::process::registry_concerns::{ProcessEventLog, ProcessLifecycle};
use crate::runtime::process::registry_delegate::{
    delegate_process_leases, delegate_process_observer_registry, delegate_process_query,
    delegate_process_registrar, delegate_process_retention, delegate_process_tool_intents,
    delegate_process_wake_outbox,
};
use crate::{
    AbandonRequest, CancelOrigin, ParentEndPlan, ParentScope, PluginError, ProcessAwaitOutput,
    ProcessCompletionAuthority, ProcessCompletionOutcome, ProcessEventAppendRequest,
    ProcessExecutionWriteAuthority, ProcessId, ProcessLease, ProcessRecord, ProcessStartOutcome,
    ProcessStarted, RuntimeReplayAttribution, SessionId, StoreRealization, WaitState,
};

/// Refuses the next `failures` registry writes that carry a runtime append of
/// `event_type` (one of the `process.effect_*` kinds), after letting the first
/// [`Self::after`] of them through, forwarding everything else to the wrapped
/// registry unchanged, and counts the writes that carried one. A refusal is a
/// retryable store fault, as a crash before the commit is to the substrate.
///
/// A run's summary rides its boundary writes (FIG-3571): an event batch, a
/// wait's enter or clear, or the terminal completion, each one transaction.
/// Its effects have already committed their journal entries when the
/// boundary is written, so a refused write leaves exactly the durable state a
/// crash before that boundary leaves: recorded effects with no summary events
/// and no boundary.
pub struct EffectSummaryAppendFaults {
    inner: Arc<dyn ProcessRegistry>,
    event_type: &'static str,
    skip: Arc<AtomicUsize>,
    remaining: Arc<AtomicUsize>,
    refused: Arc<std::sync::Mutex<Vec<ProcessEventAppendRequest>>>,
    refused_writes: Arc<AtomicUsize>,
    carried: Arc<AtomicUsize>,
}

impl EffectSummaryAppendFaults {
    pub fn new(inner: Arc<dyn ProcessRegistry>, event_type: &'static str, failures: usize) -> Self {
        Self {
            inner,
            event_type,
            skip: Arc::default(),
            remaining: Arc::new(AtomicUsize::new(failures)),
            refused: Arc::default(),
            refused_writes: Arc::default(),
            carried: Arc::default(),
        }
    }

    /// Let the first `writes` carrying writes through before refusing.
    #[must_use]
    pub fn after(self, writes: usize) -> Self {
        self.skip.store(writes, Ordering::SeqCst);
        self
    }

    /// How many writes this decorator has refused so far.
    pub fn injected(&self) -> usize {
        self.refused_writes.load(Ordering::SeqCst)
    }

    /// The `event_type` appends of the writes this decorator refused, in
    /// order.
    pub fn refused(&self) -> Vec<ProcessEventAppendRequest> {
        self.refused.lock_recover().clone()
    }

    /// How many writes carrying an `event_type` append reached the wrapped
    /// registry: one per run boundary that had a summary to commit.
    pub fn summary_writes(&self) -> usize {
        self.carried.load(Ordering::SeqCst)
    }

    fn fault_write(
        &self,
        process_id: &ProcessId,
        requests: &[ProcessEventAppendRequest],
    ) -> Result<(), PluginError> {
        let carried = requests
            .iter()
            .filter(|request| request.event_type == self.event_type)
            .cloned()
            .collect::<Vec<_>>();
        if carried.is_empty() {
            return Ok(());
        }
        let skipped = self
            .skip
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |skip| {
                skip.checked_sub(1)
            })
            .is_ok();
        let refused = !skipped
            && self
                .remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok();
        if !refused {
            self.carried.fetch_add(1, Ordering::SeqCst);
            return Ok(());
        }
        self.refused_writes.fetch_add(1, Ordering::SeqCst);
        self.refused.lock_recover().extend(carried);
        Err(PluginError::RuntimeEffectController(
            crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::StoreCommitContended,
                format!(
                    "injected crash before the `{}` batch for process `{process_id}`",
                    self.event_type
                ),
            ),
        ))
    }
}

impl crate::FleetFormatStore for EffectSummaryAppendFaults {
    fn fleet_format(&self) -> crate::FleetFormat {
        self.inner.fleet_format()
    }
}

delegate_process_query!(EffectSummaryAppendFaults, inner);

delegate_process_registrar!(
    EffectSummaryAppendFaults,
    inner,
    registration | _faults,
    forwarded | { forwarded.await },
    event | _faults,
    _process_id,
    forwarded | { forwarded.await }
);

delegate_process_observer_registry!(EffectSummaryAppendFaults, inner);

#[async_trait::async_trait]
impl ProcessEventLog for EffectSummaryAppendFaults {
    async fn append_event(
        &self,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
    ) -> Result<crate::ProcessEventAppendReceipt, PluginError> {
        self.inner.append_event(process_id, request).await
    }

    async fn append_event_with_authority(
        &self,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<crate::ProcessEventAppendReceipt, PluginError> {
        self.fault_write(process_id, std::slice::from_ref(&request))?;
        self.inner
            .append_event_with_authority(process_id, request, authority)
            .await
    }

    async fn append_events(
        &self,
        process_id: &ProcessId,
        requests: Vec<ProcessEventAppendRequest>,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<Vec<crate::ProcessEventAppendReceipt>, PluginError> {
        self.fault_write(process_id, &requests)?;
        self.inner
            .append_events(process_id, requests, authority)
            .await
    }

    async fn event_page_after(
        &self,
        process_id: &ProcessId,
        after_sequence: u64,
        limit: std::num::NonZeroUsize,
        mode: crate::ProcessEventQueryMode,
    ) -> Result<crate::ProcessEventReadOutcome<crate::ProcessEventPage>, PluginError> {
        self.inner
            .event_page_after(process_id, after_sequence, limit, mode)
            .await
    }

    async fn count_events_through(
        &self,
        process_id: &ProcessId,
        event_type: &str,
        up_to_sequence: u64,
    ) -> Result<u64, PluginError> {
        self.inner
            .count_events_through(process_id, event_type, up_to_sequence)
            .await
    }

    async fn recent_events(
        &self,
        process_id: &ProcessId,
        limit: usize,
    ) -> Result<Vec<crate::ProcessEvent>, PluginError> {
        self.inner.recent_events(process_id, limit).await
    }
}

#[async_trait::async_trait]
impl ProcessLifecycle for EffectSummaryAppendFaults {
    async fn complete_process(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        self.complete_process_with_prelude(process_id, await_output, Vec::new(), authority)
            .await
    }

    async fn complete_process_with_prelude(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        prelude: Vec<crate::ProcessEventAppendRequest>,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        self.fault_write(process_id, &prelude)?;
        self.inner
            .complete_process_with_prelude(process_id, await_output, prelude, authority)
            .await
    }

    async fn complete_process_with_lease(
        &self,
        lease: &ProcessLease,
        await_output: ProcessAwaitOutput,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        self.inner
            .complete_process_with_lease(lease, await_output)
            .await
    }

    async fn record_parent_end(&self, parent: &ParentScope) -> Result<(), PluginError> {
        self.inner.record_parent_end(parent).await
    }

    async fn list_pending_parent_end_plans(
        &self,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<ParentEndPlan>, PluginError> {
        self.inner.list_pending_parent_end_plans(limit).await
    }

    async fn get_parent_end_plan(
        &self,
        parent: &ParentScope,
    ) -> Result<Option<ParentEndPlan>, PluginError> {
        self.inner.get_parent_end_plan(parent).await
    }

    async fn list_parent_end_children(
        &self,
        parent: &ParentScope,
        after: Option<&ProcessId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        self.inner
            .list_parent_end_children(parent, after, limit)
            .await
    }

    async fn settle_parent_end_plan(&self, parent: &ParentScope) -> Result<(), PluginError> {
        self.inner.settle_parent_end_plan(parent).await
    }

    async fn list_unrecorded_opener_parents(
        &self,
        after: Option<&str>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<ParentScope>, PluginError> {
        self.inner
            .list_unrecorded_opener_parents(after, limit)
            .await
    }

    async fn record_first_started_with_authority(
        &self,
        process_id: &ProcessId,
        started: ProcessStarted,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessStartOutcome, PluginError> {
        self.inner
            .record_first_started_with_authority(process_id, started, authority)
            .await
    }

    async fn request_process_cancel(
        &self,
        process_id: &ProcessId,
        origin: CancelOrigin,
        requester: String,
        attribution: Option<RuntimeReplayAttribution>,
    ) -> Result<ProcessRecord, PluginError> {
        self.inner
            .request_process_cancel(process_id, origin, requester, attribution)
            .await
    }

    async fn request_process_cancel_reporting_realization(
        &self,
        process_id: &ProcessId,
        origin: CancelOrigin,
        requester: String,
        attribution: Option<RuntimeReplayAttribution>,
    ) -> Result<(ProcessRecord, StoreRealization), PluginError> {
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
        request: AbandonRequest,
    ) -> Result<ProcessRecord, PluginError> {
        self.inner
            .request_process_abandon(process_id, request)
            .await
    }

    async fn record_caller_departure(
        &self,
        process_id: &ProcessId,
    ) -> Result<ProcessRecord, PluginError> {
        self.inner.record_caller_departure(process_id).await
    }

    async fn set_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        wait: WaitState,
        prelude: Vec<crate::ProcessEventAppendRequest>,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        self.fault_write(process_id, &prelude)?;
        self.inner
            .set_process_wait_with_authority(process_id, wait, prelude, authority)
            .await
    }

    async fn clear_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        prelude: Vec<crate::ProcessEventAppendRequest>,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        self.fault_write(process_id, &prelude)?;
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

delegate_process_tool_intents!(EffectSummaryAppendFaults, inner);

delegate_process_wake_outbox!(EffectSummaryAppendFaults, inner);

delegate_process_leases!(EffectSummaryAppendFaults, inner);

delegate_process_retention!(EffectSummaryAppendFaults, inner);

impl super::super::registry_concerns::ProcessClockRebind for EffectSummaryAppendFaults {
    fn with_runtime_clock(&self, clock: Arc<dyn crate::Clock>) -> Option<Arc<dyn ProcessRegistry>> {
        self.inner.with_runtime_clock(clock).map(|inner| {
            Arc::new(Self {
                inner,
                event_type: self.event_type,
                skip: Arc::clone(&self.skip),
                remaining: Arc::clone(&self.remaining),
                refused: Arc::clone(&self.refused),
                refused_writes: Arc::clone(&self.refused_writes),
                carried: Arc::clone(&self.carried),
            }) as Arc<dyn ProcessRegistry>
        })
    }
}
