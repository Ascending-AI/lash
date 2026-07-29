use crate::plugin::PluginError;

use super::super::registry::ProcessContinuationStore;
use super::TestLocalProcessRegistry;

#[async_trait::async_trait]
impl ProcessContinuationStore for TestLocalProcessRegistry {
    async fn put_segment_handover(
        &self,
        process_id: &str,
        handover: crate::PersistedSegmentHandover,
    ) -> Result<(), PluginError> {
        if !self.managed.lock().await.contains_key(process_id) {
            return Err(self.process_miss(process_id).await);
        }
        let key = (process_id.to_string(), handover.segment_ordinal);
        let mut handovers = self.handovers.lock().await;
        if let Some(existing) = handovers.get(&key) {
            if existing == &handover {
                return Ok(());
            }
            return Err(PluginError::Session(format!(
                "process `{process_id}` segment {} handover conflict",
                handover.segment_ordinal
            )));
        }
        handovers.retain(|(stored_process_id, stored_ordinal), _| {
            stored_process_id != process_id
                || *stored_ordinal >= handover.segment_ordinal.saturating_sub(1)
        });
        handovers.insert(key, handover);
        Ok(())
    }

    async fn get_segment_handover(
        &self,
        process_id: &str,
        segment_ordinal: u64,
    ) -> Result<Option<crate::PersistedSegmentHandover>, PluginError> {
        Ok(self
            .handovers
            .lock()
            .await
            .get(&(process_id.to_string(), segment_ordinal))
            .cloned())
    }

    async fn latest_segment_handover(
        &self,
        process_id: &str,
    ) -> Result<Option<crate::PersistedSegmentHandover>, PluginError> {
        Ok(self
            .handovers
            .lock()
            .await
            .iter()
            .filter(|((stored_process_id, _), _)| stored_process_id == process_id)
            .max_by_key(|((_, ordinal), _)| *ordinal)
            .map(|(_, handover)| handover.clone()))
    }

    async fn delete_segment_handovers(&self, process_id: &str) -> Result<(), PluginError> {
        self.handovers
            .lock()
            .await
            .retain(|(stored_process_id, _), _| stored_process_id != process_id);
        Ok(())
    }
}
