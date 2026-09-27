use crate::ProcessId;

use crate::plugin::PluginError;

use super::super::{
    ProcessExecutionWriteAuthority, ProcessRecord, ProcessRegistry, ProcessStartOutcome,
    ProcessStarted, WaitState,
};

/// Explicit fixture-only conveniences for lifecycle writes whose production
/// API requires an execution authority. Writes use the process's persisted
/// engine invocation identity.
#[async_trait::async_trait]
pub trait TestProcessRegistryWriteExt: ProcessRegistry {
    async fn record_first_started(
        &self,
        process_id: &ProcessId,
        started: ProcessStarted,
    ) -> Result<ProcessRecord, PluginError> {
        let authority = authority_for_started(process_id, &started)?;
        self.record_first_started_with_authority(process_id, started, &authority)
            .await
            .map(ProcessStartOutcome::into_record)
    }

    async fn set_process_wait(
        &self,
        process_id: &ProcessId,
        wait: WaitState,
    ) -> Result<ProcessRecord, PluginError> {
        let authority = current_process_authority(self, process_id).await?;
        self.set_process_wait_with_authority(process_id, wait, Vec::new(), &authority)
            .await
    }

    async fn clear_process_wait(
        &self,
        process_id: &ProcessId,
    ) -> Result<ProcessRecord, PluginError> {
        let authority = current_process_authority(self, process_id).await?;
        self.clear_process_wait_with_authority(process_id, Vec::new(), &authority)
            .await
    }
}

impl<T> TestProcessRegistryWriteExt for T where T: ProcessRegistry + ?Sized {}

async fn current_process_authority(
    registry: &(impl ProcessRegistry + ?Sized),
    process_id: &ProcessId,
) -> Result<ProcessExecutionWriteAuthority, PluginError> {
    let record = registry
        .get_process(process_id)
        .await?
        .ok_or_else(|| super::super::registry_transitions::unknown_process(process_id))?;
    let started = record.first_started.as_deref().ok_or_else(|| {
        PluginError::Session(format!(
            "test fixture cannot write process `{process_id}` before its engine invocation starts"
        ))
    })?;
    authority_for_started(process_id, started)
}

fn authority_for_started(
    process_id: &ProcessId,
    started: &ProcessStarted,
) -> Result<ProcessExecutionWriteAuthority, PluginError> {
    let execution_id = started
        .owner
        .engine_process_execution_id(process_id)
        .ok_or_else(|| {
            PluginError::Session(format!(
                "test fixture process `{process_id}` has no engine invocation identity"
            ))
        })?;
    Ok(
        ProcessExecutionWriteAuthority::invocation(process_id.clone(), execution_id)
            .bind_attempt(started.attempt),
    )
}
