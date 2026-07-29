use std::sync::Arc;

use crate::plugin::PluginError;

use super::super::{
    ProcessExecutionWriteAuthority, ProcessLease, ProcessLeaseClaimOutcome, ProcessLeaseCompletion,
    ProcessRecord, ProcessRegistry, ProcessStartOutcome, ProcessStarted, WaitState,
};
use super::TestLocalProcessRegistry;

impl TestLocalProcessRegistry {
    pub fn with_wake_delivery_config(mut self, config: super::super::WakeDeliveryConfig) -> Self {
        self.wake_delivery_config = config;
        self
    }

    pub fn with_clock(mut self, clock: Arc<dyn crate::Clock>) -> Self {
        self.clock = clock;
        self
    }
}

#[derive(Clone)]
pub(super) struct ExecutionWritePause {
    pub(super) validated: Arc<tokio::sync::Notify>,
    pub(super) resume: Arc<tokio::sync::Notify>,
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
