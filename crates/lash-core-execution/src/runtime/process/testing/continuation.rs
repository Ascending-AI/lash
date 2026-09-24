use crate::ProcessId;
use crate::plugin::PluginError;

use super::super::registry::ProcessContinuationStore;
use super::TestLocalProcessRegistry;
use super::types::RetainedHandover;

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
                if existing.handover == handover {
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
            state.handovers.insert(
                key,
                RetainedHandover {
                    handover,
                    started: None,
                },
            );
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
            .map(|retained| retained.handover.clone()))
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
            .map(|(_, retained)| retained.handover.clone()))
    }

    async fn segment_start(
        &self,
        segment: &crate::ProcessSegmentKey,
    ) -> Result<Option<crate::SegmentStartMarker>, PluginError> {
        Ok(self
            .state
            .lock()
            .await
            .handovers
            .get(&(segment.process_id.clone(), segment.segment_ordinal))
            .and_then(|retained| retained.started.clone()))
    }

    async fn mark_segment_started(
        &self,
        segment: &crate::ProcessSegmentKey,
        marker: crate::SegmentStartMarker,
    ) -> Result<crate::SegmentStartMarker, PluginError> {
        let key = (segment.process_id.clone(), segment.segment_ordinal);
        self.write(async |state| {
            let retained = state.handovers.get_mut(&key).ok_or_else(|| {
                PluginError::Session(format!(
                    "process `{}` segment {} has no retained handover to mark started",
                    segment.process_id, segment.segment_ordinal
                ))
            })?;
            Ok(retained.started.get_or_insert(marker).clone())
        })
        .await
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
