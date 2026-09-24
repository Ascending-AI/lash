use super::*;
use lash_sansio::ProcessId;

#[async_trait::async_trait]
impl ProcessContinuationStore for SqliteProcessRegistry {
    async fn put_segment_handover(
        &self,
        process_id: &ProcessId,
        handover: PersistedSegmentHandover,
    ) -> Result<(), lash_core_execution::PluginError> {
        self.put_segment_handover_impl(process_id, handover).await
    }

    async fn get_segment_handover(
        &self,
        process_id: &ProcessId,
        segment_ordinal: u64,
    ) -> Result<Option<PersistedSegmentHandover>, lash_core_execution::PluginError> {
        self.get_segment_handover_impl(process_id, segment_ordinal)
            .await
    }

    async fn latest_segment_handover(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<PersistedSegmentHandover>, lash_core_execution::PluginError> {
        self.latest_segment_handover_impl(process_id).await
    }

    async fn segment_start(
        &self,
        segment: &lash_core_execution::ProcessSegmentKey,
    ) -> Result<Option<lash_core_execution::SegmentStartMarker>, lash_core_execution::PluginError>
    {
        self.segment_start_impl(segment).await
    }

    async fn mark_segment_started(
        &self,
        segment: &lash_core_execution::ProcessSegmentKey,
        marker: lash_core_execution::SegmentStartMarker,
    ) -> Result<lash_core_execution::SegmentStartMarker, lash_core_execution::PluginError> {
        self.mark_segment_started_impl(segment, marker).await
    }

    async fn delete_segment_handovers(
        &self,
        process_id: &ProcessId,
    ) -> Result<(), lash_core_execution::PluginError> {
        self.delete_segment_handovers_impl(process_id).await
    }
}
