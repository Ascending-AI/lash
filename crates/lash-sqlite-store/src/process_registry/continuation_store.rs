use super::*;

#[async_trait::async_trait]
impl ProcessContinuationStore for SqliteProcessRegistry {
    async fn put_segment_handover(
        &self,
        process_id: &str,
        handover: PersistedSegmentHandover,
    ) -> Result<(), lash_core::PluginError> {
        self.put_segment_handover_impl(process_id, handover).await
    }

    async fn get_segment_handover(
        &self,
        process_id: &str,
        segment_ordinal: u64,
    ) -> Result<Option<PersistedSegmentHandover>, lash_core::PluginError> {
        self.get_segment_handover_impl(process_id, segment_ordinal)
            .await
    }

    async fn latest_segment_handover(
        &self,
        process_id: &str,
    ) -> Result<Option<PersistedSegmentHandover>, lash_core::PluginError> {
        self.latest_segment_handover_impl(process_id).await
    }

    async fn delete_segment_handovers(
        &self,
        process_id: &str,
    ) -> Result<(), lash_core::PluginError> {
        self.delete_segment_handovers_impl(process_id).await
    }
}
