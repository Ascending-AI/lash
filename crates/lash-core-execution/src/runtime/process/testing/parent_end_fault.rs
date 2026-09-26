//! A `ProcessRegistry` decorator that drops the first `record_parent_end`
//! call for one chosen parent scope.
//!
//! The drain-end epilogue writes its end receipt and its parent-end ledger
//! row as two separate stores' writes; the crash window between them is what
//! `redrive_missing_opener_parent_end_rows` closes (ADR 0094, FIG-3419). A
//! law cannot pause the epilogue between the two writes, so it injects the
//! equivalent failure here: the first `record_parent_end` for the target
//! scope answers `Err`, the epilogue traces and returns without the row —
//! the same durable shape a crash after the receipt leaves — and the sweep
//! re-derives it on the next pass.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::runtime::process::registry::ProcessRegistry;
use crate::runtime::process::registry_concerns::ProcessLifecycle;
use crate::runtime::process::registry_delegate::{
    delegate_process_event_log, delegate_process_leases, delegate_process_observer_registry,
    delegate_process_query, delegate_process_registrar, delegate_process_retention,
    delegate_process_tool_intents, delegate_process_wake_outbox,
};
use crate::{
    AbandonRequest, CancelOrigin, ParentEndPlan, ParentScope, PluginError, ProcessAwaitOutput,
    ProcessCompletionAuthority, ProcessCompletionOutcome, ProcessExecutionWriteAuthority,
    ProcessId, ProcessLease, ProcessRecord, ProcessStartOutcome, ProcessStarted,
    RuntimeReplayAttribution, SessionId, StoreRealization, WaitState,
};

/// The decorated registry: one armed fault for `target`, everything else
/// forwarded.
struct ParentEndFault {
    inner: Arc<dyn ProcessRegistry>,
    target: ParentScope,
    armed: AtomicBool,
}

/// Wrap `inner` so the first `record_parent_end(target)` fails once; every
/// other call — including a repeat of the same record — is forwarded.
pub fn fail_parent_end_once(
    inner: Arc<dyn ProcessRegistry>,
    target: ParentScope,
) -> Arc<dyn ProcessRegistry> {
    Arc::new(ParentEndFault {
        inner,
        target,
        armed: AtomicBool::new(true),
    })
}

impl crate::FleetFormatStore for ParentEndFault {
    fn fleet_format(&self) -> crate::FleetFormat {
        self.inner.fleet_format()
    }
}

delegate_process_query!(ParentEndFault, inner);

delegate_process_registrar!(
    ParentEndFault,
    inner,
    registration | _watched,
    forwarded | {
        let record = forwarded.await?;
        Ok(record)
    },
    event | _watched,
    _process_id,
    forwarded | {
        let record = forwarded.await?;
        Ok(record)
    }
);

delegate_process_observer_registry!(ParentEndFault, inner);

delegate_process_event_log!(
    ParentEndFault,
    inner,
    event | _watched,
    _process_id,
    forwarded | {
        let receipt = forwarded.await?;
        Ok(receipt)
    }
);

delegate_process_tool_intents!(ParentEndFault, inner);

delegate_process_wake_outbox!(ParentEndFault, inner);

delegate_process_leases!(ParentEndFault, inner);

delegate_process_retention!(ParentEndFault, inner);

#[async_trait::async_trait]
impl ProcessLifecycle for ParentEndFault {
    async fn complete_process(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        self.inner
            .complete_process(process_id, await_output, authority)
            .await
    }

    async fn complete_process_with_prelude(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        prelude: Vec<crate::ProcessEventAppendRequest>,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
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
        if *parent == self.target && self.armed.swap(false, Ordering::SeqCst) {
            return Err(PluginError::Session(
                "injected crash between the drain-end receipt and the ledger row".to_string(),
            ));
        }
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

impl super::super::registry::ProcessClockRebind for ParentEndFault {
    fn with_runtime_clock(&self, clock: Arc<dyn crate::Clock>) -> Option<Arc<dyn ProcessRegistry>> {
        self.inner.with_runtime_clock(clock).map(|inner| {
            Arc::new(Self {
                inner,
                target: self.target.clone(),
                armed: AtomicBool::new(self.armed.load(Ordering::SeqCst)),
            }) as Arc<dyn ProcessRegistry>
        })
    }
}
