//! A registry decorator that simulates a worker crash between an effect's
//! replay-row commit and its durable effect-summary append.

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
    delegate_process_leases, delegate_process_lifecycle, delegate_process_observer_registry,
    delegate_process_query, delegate_process_registrar, delegate_process_retention,
    delegate_process_tool_intents, delegate_process_wake_outbox,
};

/// Fails the next `failures` runtime appends of `event_type` (one of the
/// `process.effect_*` kinds), forwarding everything else to the wrapped
/// registry unchanged.
///
/// The effect itself has already committed its replay row when the runtime
/// appends, so a refused append leaves exactly the durable state a crash in
/// that window leaves: a recorded effect with no summary event.
pub struct EffectSummaryAppendFaults {
    inner: Arc<dyn ProcessRegistry>,
    event_type: &'static str,
    remaining: Arc<AtomicUsize>,
    refused: Arc<std::sync::Mutex<Vec<crate::ProcessEventAppendRequest>>>,
}

impl EffectSummaryAppendFaults {
    pub fn new(inner: Arc<dyn ProcessRegistry>, event_type: &'static str, failures: usize) -> Self {
        Self {
            inner,
            event_type,
            remaining: Arc::new(AtomicUsize::new(failures)),
            refused: Arc::default(),
        }
    }

    /// How many appends this decorator has refused so far.
    pub fn injected(&self) -> usize {
        self.refused.lock_recover().len()
    }

    /// The appends this decorator refused, in order.
    pub fn refused(&self) -> Vec<crate::ProcessEventAppendRequest> {
        self.refused.lock_recover().clone()
    }

    fn take_fault(&self, request: &crate::ProcessEventAppendRequest) -> bool {
        if request.event_type != self.event_type {
            return false;
        }
        let refused = self
            .remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();
        if refused {
            self.refused.lock_recover().push(request.clone());
        }
        refused
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
impl super::super::registry_concerns::ProcessEventLog for EffectSummaryAppendFaults {
    async fn append_event(
        &self,
        process_id: &ProcessId,
        request: crate::ProcessEventAppendRequest,
    ) -> Result<crate::ProcessEventAppendReceipt, crate::PluginError> {
        self.inner.append_event(process_id, request).await
    }

    async fn append_event_with_authority(
        &self,
        process_id: &ProcessId,
        request: crate::ProcessEventAppendRequest,
        authority: &crate::ProcessExecutionWriteAuthority,
    ) -> Result<crate::ProcessEventAppendReceipt, crate::PluginError> {
        if self.take_fault(&request) {
            return Err(crate::PluginError::Session(format!(
                "injected crash before the `{}` append for process `{process_id}`",
                request.event_type
            )));
        }
        self.inner
            .append_event_with_authority(process_id, request, authority)
            .await
    }

    async fn event_page_after(
        &self,
        process_id: &crate::ProcessId,
        after_sequence: u64,
        limit: std::num::NonZeroUsize,
        mode: crate::ProcessEventQueryMode,
    ) -> Result<crate::ProcessEventReadOutcome<crate::ProcessEventPage>, crate::PluginError> {
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
        self.inner
            .count_events_through(process_id, event_type, up_to_sequence)
            .await
    }

    async fn recent_events(
        &self,
        process_id: &ProcessId,
        limit: usize,
    ) -> Result<Vec<crate::ProcessEvent>, crate::PluginError> {
        self.inner.recent_events(process_id, limit).await
    }
}

delegate_process_lifecycle!(
    EffectSummaryAppendFaults,
    inner,
    event | _faults,
    _process_id,
    forwarded | { forwarded.await }
);

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
                remaining: Arc::clone(&self.remaining),
                refused: Arc::clone(&self.refused),
            }) as Arc<dyn ProcessRegistry>
        })
    }
}
