//! Process registry instrumentation preserves every inner override.
use crate::process_counters::Counters;
use lash_execution_contracts::delegate_process_observer_registry;
use lash_execution_contracts::process::ProcessRegistry;
use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::ProcessId;
use std::sync::Arc;

pub struct RecordingRegistry {
    pub inner: Arc<dyn ProcessRegistry>,
    pub counters: Arc<Counters>,
}
#[async_trait::async_trait]
impl lash_execution_contracts::process::registry_concerns::ProcessQuery for RecordingRegistry {
    async fn get_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<
        Option<lash_execution_contracts::ProcessRecord>,
        lash_execution_contracts::PluginError,
    > {
        self.counters.record("registry.get_process");
        self.inner.get_process(process_id).await
    }

    async fn get_process_by_start_key(
        &self,
        start_key: &lash_core_store::process_identity::StartKey,
    ) -> Result<
        Option<lash_execution_contracts::ProcessRecord>,
        lash_execution_contracts::PluginError,
    > {
        self.inner.get_process_by_start_key(start_key).await
    }

    async fn list_processes_page(
        &self,
        filter: &lash_execution_contracts::ProcessListFilter,
        limit: std::num::NonZeroUsize,
        continuation: Option<lash_execution_contracts::ProcessRosterCursor>,
    ) -> Result<lash_execution_contracts::ProcessRosterRecords, lash_execution_contracts::PluginError>
    {
        self.inner
            .list_processes_page(filter, limit, continuation)
            .await
    }

    async fn process_change_bounds(
        &self,
    ) -> Result<lash_execution_contracts::ProcessChangeBounds, lash_execution_contracts::PluginError>
    {
        self.inner.process_change_bounds().await
    }

    async fn processes_changed_since(
        &self,
        cursor: lash_execution_contracts::ProcessChangeCursor,
        limit: usize,
    ) -> Result<
        (
            Vec<lash_execution_contracts::ProcessChange>,
            lash_execution_contracts::ProcessChangeCursor,
        ),
        lash_execution_contracts::PluginError,
    > {
        self.inner.processes_changed_since(cursor, limit).await
    }

    async fn list_non_terminal_processes_page(
        &self,
        limit: std::num::NonZeroUsize,
        continuation: Option<lash_execution_contracts::ProcessRegistryCursor>,
    ) -> Result<
        lash_execution_contracts::NonTerminalProcessPage,
        lash_execution_contracts::PluginError,
    > {
        self.inner
            .list_non_terminal_processes_page(limit, continuation)
            .await
    }

    async fn filter_unregistered_process_ids(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<ProcessId>, lash_execution_contracts::PluginError> {
        self.inner
            .filter_unregistered_process_ids(process_ids)
            .await
    }

    async fn filter_tombstoned_process_ids(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<ProcessId>, lash_execution_contracts::PluginError> {
        self.inner.filter_tombstoned_process_ids(process_ids).await
    }

    async fn live_reference_summary(
        &self,
    ) -> Result<
        Vec<lash_execution_contracts::ProcessLiveReferenceView>,
        lash_execution_contracts::PluginError,
    > {
        self.inner.live_reference_summary().await
    }

    async fn count_non_terminal_processes(
        &self,
    ) -> Result<usize, lash_execution_contracts::PluginError> {
        self.inner.count_non_terminal_processes().await
    }
}
lash_execution_contracts::delegate_process_registrar!(
    RecordingRegistry,
    inner,
    registration | this,
    call | {
        let _ = this;
        call.await
    },
    event | this,
    id,
    call | {
        let _ = (this, id);
        call.await
    }
);
lash_execution_contracts::delegate_process_event_log!(RecordingRegistry, inner);
lash_execution_contracts::delegate_process_lifecycle!(
    RecordingRegistry,
    inner,
    event | this,
    id,
    call | {
        let _ = (this, id);
        call.await
    }
);
lash_execution_contracts::delegate_process_observer_registry!(RecordingRegistry, inner);
lash_execution_contracts::delegate_process_tool_intents!(RecordingRegistry, inner);
lash_execution_contracts::delegate_process_retention!(RecordingRegistry, inner);

impl lash_core_store::store::FleetFormatStore for RecordingRegistry {
    fn fleet_format(&self) -> lash_core_store::store::FleetFormat {
        self.inner.fleet_format()
    }
}
impl lash_execution_contracts::process::ProcessClockRebind for RecordingRegistry {
    fn with_runtime_clock(
        &self,
        clock: Arc<dyn lash_core_ids::clock::Clock>,
    ) -> Option<Arc<dyn ProcessRegistry>> {
        self.inner.with_runtime_clock(clock).map(|inner| {
            Arc::new(Self {
                inner,
                counters: Arc::clone(&self.counters),
            }) as Arc<dyn ProcessRegistry>
        })
    }
}
