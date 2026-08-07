use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::plugin::PluginError;

use super::super::{
    ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessLease, ProcessLeaseClaimOutcome,
    ProcessLeaseCompletion, ProcessRecord, ProcessRegistry, ProcessStartOutcome, ProcessStarted,
    WaitState,
};
use super::{ManagedLeaseMap, TestLocalProcessRegistry};

impl Default for TestLocalProcessRegistry {
    fn default() -> Self {
        Self {
            transaction: Arc::new(Mutex::new(())),
            managed: Arc::new(Mutex::new(HashMap::new())),
            process_read_error: Arc::new(Mutex::new(None)),
            process_read_error_after: Arc::new(Mutex::new(None)),
            process_read_absent: Arc::new(Mutex::new(false)),
            process_read_override: Arc::new(Mutex::new(None)),
            process_lease_claim_error: Arc::new(Mutex::new(None)),
            process_lease_renew_error: Arc::new(Mutex::new(None)),
            process_terminal_write_error: Arc::new(Mutex::new(None)),
            process_terminal_write_outcome: Arc::new(Mutex::new(None)),
            process_lease_release_error: Arc::new(Mutex::new(None)),
            next_change_seq: Arc::new(Mutex::new(0)),
            observers: Arc::new(Mutex::new(HashMap::<_, HashSet<_>>::new())),
            wake_targets: Arc::new(Mutex::new(HashMap::new())),
            tombstones: Arc::new(Mutex::new(HashMap::new())),
            leases: Arc::new(Mutex::new(HashMap::new())),
            handovers: Arc::new(Mutex::new(HashMap::new())),
            execution_write_pause: Arc::new(std::sync::Mutex::new(None)),
            wake_mark_pause: Arc::new(std::sync::Mutex::new(None)),
            append_target_snapshot_pause: Arc::new(std::sync::Mutex::new(None)),
            append_outbox_pause: Arc::new(std::sync::Mutex::new(None)),
            prune_managed_removal_pause: Arc::new(std::sync::Mutex::new(None)),
            wake_delivery_config: super::super::WakeDeliveryConfig::default(),
            wake_deliveries: Arc::new(Mutex::new(HashMap::new())),
            wake_allocation_floors: Arc::new(Mutex::new(HashMap::new())),
            clock: Arc::new(crate::SystemClock),
        }
    }
}

impl TestLocalProcessRegistry {
    /// Updates process read error state for store and process-engine implementors while persisting
    /// and coordinating durable process execution.
    pub async fn set_process_read_error(&self, error: Option<PluginError>) {
        *self.process_read_error.lock().await = error;
    }

    /// Injects one process-read error after `successful_reads` successful reads.
    pub async fn set_process_read_error_after(&self, successful_reads: usize, error: PluginError) {
        *self.process_read_error_after.lock().await = Some((successful_reads, error));
    }

    /// Controls deterministic read-as-absent injection for recovery tests.
    pub async fn set_process_read_absent(&self, absent: bool) {
        *self.process_read_absent.lock().await = absent;
    }

    /// Overrides the next process read with a supplied production read-model row.
    pub async fn set_process_read_override(&self, record: ProcessRecord) {
        *self.process_read_override.lock().await = Some(record);
    }

    /// Updates process-lease claim error injection for recovery tests.
    pub async fn set_process_lease_claim_error(&self, error: Option<PluginError>) {
        *self.process_lease_claim_error.lock().await = error;
    }

    /// Updates process-lease renewal error injection for recovery tests.
    pub async fn set_process_lease_renew_error(&self, error: Option<PluginError>) {
        *self.process_lease_renew_error.lock().await = error;
    }

    /// Updates process terminal-write error injection for recovery tests.
    pub async fn set_process_terminal_write_error(&self, error: Option<PluginError>) {
        *self.process_terminal_write_error.lock().await = error;
    }

    /// Overrides the next fenced terminal-write outcome.
    pub async fn set_process_terminal_write_outcome(
        &self,
        outcome: super::super::ProcessCompletionOutcome,
    ) {
        *self.process_terminal_write_outcome.lock().await = Some(outcome);
    }

    /// Updates process-lease release error injection for recovery tests.
    pub async fn set_process_lease_release_error(&self, error: Option<PluginError>) {
        *self.process_lease_release_error.lock().await = error;
    }

    /// Sets the wake delivery config carried by a `TestLocalProcessRegistry` for store and
    /// process-engine implementors while persisting and coordinating durable process execution.
    pub fn with_wake_delivery_config(mut self, config: super::super::WakeDeliveryConfig) -> Self {
        self.wake_delivery_config = config;
        self
    }

    /// Sets the clock carried by a `TestLocalProcessRegistry` for store and process-engine
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_clock(mut self, clock: Arc<dyn crate::Clock>) -> Self {
        self.clock = clock;
        self
    }

    #[doc(hidden)]
    pub fn pause_next_execution_write_after_validation(&self) -> ExecutionWritePauseHandle {
        let pause = ExecutionWritePause::new();
        *self
            .execution_write_pause
            .lock()
            .expect("execution write pause lock") = Some(pause.clone());
        pause.handle()
    }

    #[doc(hidden)]
    pub fn pause_next_wake_mark(&self) -> ExecutionWritePauseHandle {
        let pause = ExecutionWritePause::new();
        *self.wake_mark_pause.lock().expect("wake mark pause lock") = Some(pause.clone());
        pause.handle()
    }

    #[doc(hidden)]
    pub fn pause_next_append_after_outbox(&self) -> ExecutionWritePauseHandle {
        let pause = ExecutionWritePause::new();
        *self
            .append_outbox_pause
            .lock()
            .expect("append outbox pause lock") = Some(pause.clone());
        pause.handle()
    }

    #[doc(hidden)]
    pub fn pause_next_append_after_target_snapshot(&self) -> ExecutionWritePauseHandle {
        let pause = ExecutionWritePause::new();
        *self
            .append_target_snapshot_pause
            .lock()
            .expect("append target snapshot pause lock") = Some(pause.clone());
        pause.handle()
    }

    #[doc(hidden)]
    pub fn pause_next_prune_after_managed_removal(&self) -> ExecutionWritePauseHandle {
        let pause = ExecutionWritePause::new();
        *self
            .prune_managed_removal_pause
            .lock()
            .expect("prune managed-removal pause lock") = Some(pause.clone());
        pause.handle()
    }

    #[doc(hidden)]
    pub fn transaction_is_locked_for_testing(&self) -> bool {
        self.transaction.try_lock().is_err()
    }

    #[doc(hidden)]
    pub async fn replace_process_projection_for_testing(&self, record: ProcessRecord) {
        let _transaction = self.transaction.lock().await;
        let process_id = record.id.clone();
        self.managed
            .lock()
            .await
            .get_mut(&process_id)
            .expect("replace projection for registered process")
            .record = record;
    }

    pub(super) async fn pause_append_after_target_snapshot(&self) {
        Self::wait_for_pause(
            &self.append_target_snapshot_pause,
            "append target snapshot pause lock",
        )
        .await;
    }

    pub(super) async fn pause_append_after_outbox(&self) {
        Self::wait_for_pause(&self.append_outbox_pause, "append outbox pause lock").await;
    }

    pub(super) async fn pause_execution_write_after_validation(&self) {
        Self::wait_for_pause(&self.execution_write_pause, "execution write pause lock").await;
    }

    pub(super) async fn pause_prune_after_managed_removal(&self) {
        Self::wait_for_pause(
            &self.prune_managed_removal_pause,
            "prune managed-removal pause lock",
        )
        .await;
    }

    async fn wait_for_pause(
        slot: &std::sync::Mutex<Option<ExecutionWritePause>>,
        lock_label: &str,
    ) {
        let pause = slot.lock().expect(lock_label).take();
        if let Some(pause) = pause {
            pause.validated.notify_one();
            pause.resume.notified().await;
        }
    }
}

#[derive(Clone)]
pub(super) struct ExecutionWritePause {
    pub(super) validated: Arc<tokio::sync::Notify>,
    pub(super) resume: Arc<tokio::sync::Notify>,
}

impl ExecutionWritePause {
    fn new() -> Self {
        Self {
            validated: Arc::new(tokio::sync::Notify::new()),
            resume: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn handle(&self) -> ExecutionWritePauseHandle {
        ExecutionWritePauseHandle {
            validated: Arc::clone(&self.validated),
            resume: Arc::clone(&self.resume),
        }
    }
}

#[derive(Clone)]
pub struct ExecutionWritePauseHandle {
    pub(super) validated: Arc<tokio::sync::Notify>,
    pub(super) resume: Arc<tokio::sync::Notify>,
}

impl ExecutionWritePauseHandle {
    pub async fn wait_until_validated(&self) {
        self.validated.notified().await;
    }

    pub fn resume(&self) {
        self.resume.notify_one();
    }
}

/// Loud, stable error for a superseded or expired process lease.
pub(super) fn process_lease_expired(process_id: &str) -> PluginError {
    PluginError::ProcessLeaseSuperseded {
        process_id: process_id.to_string(),
    }
}

pub(super) fn validate_in_memory_execution_authority(
    leases: &ManagedLeaseMap,
    process_id: &str,
    record: &ProcessRecord,
    authority: &ProcessExecutionWriteAuthority,
    start: Option<&ProcessStarted>,
    now: u64,
) -> Result<(), PluginError> {
    match authority {
        ProcessExecutionWriteAuthority::Invocation { .. } => {
            if let Some(started) = start {
                authority.validate_invocation_for_start(
                    process_id,
                    started,
                    record.first_started.as_deref(),
                )
            } else {
                authority.validate_invocation_for_write(process_id, record)
            }
        }
        ProcessExecutionWriteAuthority::Lease(lease) => {
            if lease.process_id != process_id {
                return Err(process_lease_expired(process_id));
            }
            if leases.get(process_id).is_some_and(|current| {
                !current.lease_token.is_empty()
                    && current.owner.same_incarnation(&lease.owner)
                    && current.lease_token == lease.lease_token
                    && current.fencing_token == lease.fencing_token
                    && current.expires_at_epoch_ms > now
            }) {
                Ok(())
            } else {
                Err(process_lease_expired(process_id))
            }
        }
    }
}

pub(super) fn process_external_ref_conflict(
    process_id: &str,
    existing: &ProcessExternalRef,
    new: &ProcessExternalRef,
) -> PluginError {
    PluginError::Session(format!(
        "process `{process_id}` external ref conflict: existing {existing:?}, new {new:?}"
    ))
}

/// Explicit fixture-only conveniences for lifecycle writes whose production
/// API requires an execution authority. Each write claims and releases a real
/// process lease through the registry under test.
#[async_trait::async_trait]
#[doc(hidden)]
pub trait TestProcessRegistryWriteExt: ProcessRegistry {
    async fn record_first_started(
        &self,
        process_id: &str,
        started: ProcessStarted,
    ) -> Result<ProcessRecord, PluginError> {
        let lease = claim_fixture_write_lease(self, process_id).await?;
        let result = self
            .record_first_started_with_authority(
                process_id,
                started,
                &ProcessExecutionWriteAuthority::lease(lease.clone()),
            )
            .await
            .and_then(ProcessStartOutcome::into_record);
        finish_fixture_write(self, &lease, result).await
    }

    async fn set_process_wait(
        &self,
        process_id: &str,
        wait: WaitState,
    ) -> Result<ProcessRecord, PluginError> {
        let lease = claim_fixture_write_lease(self, process_id).await?;
        let result = self
            .set_process_wait_with_authority(
                process_id,
                wait,
                &ProcessExecutionWriteAuthority::lease(lease.clone()),
            )
            .await;
        finish_fixture_write(self, &lease, result).await
    }

    async fn clear_process_wait(&self, process_id: &str) -> Result<ProcessRecord, PluginError> {
        let lease = claim_fixture_write_lease(self, process_id).await?;
        let result = self
            .clear_process_wait_with_authority(
                process_id,
                &ProcessExecutionWriteAuthority::lease(lease.clone()),
            )
            .await;
        finish_fixture_write(self, &lease, result).await
    }
}

impl<T> TestProcessRegistryWriteExt for T where T: ProcessRegistry + ?Sized {}

async fn claim_fixture_write_lease(
    registry: &(impl ProcessRegistry + ?Sized),
    process_id: &str,
) -> Result<ProcessLease, PluginError> {
    let owner =
        crate::LeaseOwnerIdentity::opaque(format!("test-fixture:{process_id}"), "lifecycle-write");
    match registry
        .claim_process_lease(process_id, &owner, 60_000)
        .await?
    {
        ProcessLeaseClaimOutcome::Acquired(lease) => Ok(lease),
        ProcessLeaseClaimOutcome::Busy { holder } => Err(PluginError::Session(format!(
            "test fixture cannot claim process `{process_id}` held by `{}`",
            holder.owner.owner_id
        ))),
    }
}

async fn finish_fixture_write<T>(
    registry: &(impl ProcessRegistry + ?Sized),
    lease: &ProcessLease,
    result: Result<T, PluginError>,
) -> Result<T, PluginError> {
    let release = registry
        .complete_process_lease(&ProcessLeaseCompletion::from_lease(lease))
        .await;
    match result {
        Err(error) => Err(error),
        Ok(value) => {
            release?;
            Ok(value)
        }
    }
}
