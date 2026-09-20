use crate::ProcessId;
use crate::plugin::PluginError;

use super::super::registry::ProcessContinuationStore;
use super::TestLocalProcessRegistry;

#[async_trait::async_trait]
impl ProcessContinuationStore for TestLocalProcessRegistry {
    async fn put_segment_handover(
        &self,
        process_id: &ProcessId,
        handover: crate::PersistedSegmentHandover,
    ) -> Result<(), PluginError> {
        self.write(async |state| {
            if !state.managed.contains_key(process_id) {
                return Err(super::process_miss(state, process_id));
            }
            let key = (process_id.clone(), handover.segment_ordinal);
            if let Some(existing) = state.handovers.get(&key) {
                if existing == &handover {
                    return Ok(());
                }
                return Err(PluginError::Session(format!(
                    "process `{process_id}` segment {} handover conflict",
                    handover.segment_ordinal
                )));
            }
            state
                .handovers
                .retain(|(stored_process_id, stored_ordinal), _| {
                    stored_process_id != process_id
                        || *stored_ordinal >= handover.segment_ordinal.saturating_sub(1)
                });
            state.handovers.insert(key, handover);
            Ok(())
        })
        .await
    }

    async fn get_segment_handover(
        &self,
        process_id: &ProcessId,
        segment_ordinal: u64,
    ) -> Result<Option<crate::PersistedSegmentHandover>, PluginError> {
        Ok(self
            .state
            .lock()
            .await
            .handovers
            .get(&(ProcessId::from(process_id.to_string()), segment_ordinal))
            .cloned())
    }

    async fn latest_segment_handover(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<crate::PersistedSegmentHandover>, PluginError> {
        Ok(self
            .state
            .lock()
            .await
            .handovers
            .iter()
            .filter(|((stored_process_id, _), _)| stored_process_id == process_id)
            .max_by_key(|((_, ordinal), _)| *ordinal)
            .map(|(_, handover)| handover.clone()))
    }

    async fn delete_segment_handovers(&self, process_id: &ProcessId) -> Result<(), PluginError> {
        self.write(async |state| {
            state
                .handovers
                .retain(|(stored_process_id, _), _| stored_process_id != process_id);
            Ok(())
        })
        .await
    }
}
