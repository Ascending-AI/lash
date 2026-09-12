use super::*;

#[async_trait::async_trait]
impl super::super::registry::ProcessRetention for TestLocalProcessRegistry {
    async fn pending_process_artifact_cleanup(
        &self,
    ) -> Result<Vec<crate::ProcessArtifactCleanup>, PluginError> {
        let cleanup = self.artifact_cleanup.lock().await;
        let mut pending = cleanup.values().cloned().collect::<Vec<_>>();
        pending.sort_by(|left, right| {
            left.process_id
                .cmp(&right.process_id)
                .then(left.incarnation.cmp(&right.incarnation))
        });
        Ok(pending)
    }

    async fn complete_process_artifact_cleanup(
        &self,
        process_id: &ProcessId,
        incarnation: ProcessIncarnation,
    ) -> Result<(), PluginError> {
        self.artifact_cleanup
            .lock()
            .await
            .remove(&(process_id.clone(), incarnation));
        Ok(())
    }

    async fn compact_process_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: ProjectionWatermark,
        trigger_store: Option<&dyn crate::TriggerStore>,
    ) -> Result<usize, PluginError> {
        let _transaction = self.transaction.lock().await;
        let max_change_seq = match watermark {
            ProjectionWatermark::UpTo(cursor) => Some(cursor.store_sequence()),
            ProjectionWatermark::NoProjector => None,
        };
        let outstanding_trigger_delivery_process_ids = match trigger_store {
            Some(trigger_store) => trigger_store.list_delivery_process_ids().await?,
            None => Vec::new(),
        };
        let outstanding_trigger_delivery_process_ids = outstanding_trigger_delivery_process_ids
            .iter()
            .map(ProcessId::as_str)
            .collect::<std::collections::HashSet<_>>();
        let pending_artifact_cleanup = self
            .artifact_cleanup
            .lock()
            .await
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let mut tombstones = self.tombstones.lock().await;
        let before = tombstones.len();
        let mut compacted_through = None;
        tombstones.retain(|_, tombstone| {
            let retained = tombstone.pruned_at_ms >= cutoff_epoch_ms
                || max_change_seq
                    .is_some_and(|max_change_seq| tombstone.pruned_change_seq > max_change_seq)
                || outstanding_trigger_delivery_process_ids.contains(tombstone.process_id.as_str())
                || pending_artifact_cleanup
                    .contains(&(tombstone.process_id.clone(), tombstone.incarnation));
            if !retained {
                compacted_through = Some(
                    compacted_through
                        .unwrap_or(0)
                        .max(tombstone.pruned_change_seq),
                );
            }
            retained
        });
        if let Some(compacted_through) = compacted_through {
            let mut horizon = self.tombstone_compaction_horizon.lock().await;
            *horizon = (*horizon).max(compacted_through);
        }
        Ok(before - tombstones.len())
    }

    async fn prunable_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: ProjectionWatermark,
    ) -> Result<Vec<ProcessId>, PluginError> {
        let _transaction = self.transaction.lock().await;
        let processes_with_pending_deliveries = self.processes_with_pending_deliveries().await;
        let managed = self.managed.lock().await;
        Ok(Self::prunable_process_ids(
            &managed,
            cutoff_epoch_ms,
            filter.as_ref(),
            watermark,
            &processes_with_pending_deliveries,
        ))
    }

    async fn prune_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: ProjectionWatermark,
    ) -> Result<ProcessPruneReport, PluginError> {
        let _transaction = self.transaction.lock().await;
        let processes_with_pending_deliveries = self.processes_with_pending_deliveries().await;
        let mut pruned_events = 0;
        let prunable: HashSet<ProcessId> = {
            let mut managed = self.managed.lock().await;
            let prunable = Self::prunable_process_ids(
                &managed,
                cutoff_epoch_ms,
                filter.as_ref(),
                watermark,
                &processes_with_pending_deliveries,
            );
            let pruned_at_ms = self.clock.timestamp_ms();
            for id in &prunable {
                if let Some(record) = managed.remove(id) {
                    pruned_events += record.events.len();
                    let pruned_change_seq = self.next_change_seq().await;
                    let cleanup = crate::ProcessArtifactCleanup::from_record(&record.record);
                    self.artifact_cleanup
                        .lock()
                        .await
                        .insert((id.clone(), record.record.incarnation), cleanup);
                    self.tombstones.lock().await.insert(
                        (id.clone().to_string(), record.record.incarnation),
                        ProcessTombstone {
                            process_id: id.clone(),
                            incarnation: record.record.incarnation,
                            terminal_label: record.record.status.label().to_string(),
                            pruned_at_ms,
                            pruned_change_seq,
                        },
                    );
                }
            }
            prunable.into_iter().collect()
        };
        self.pause_prune_after_managed_removal().await;
        {
            let mut observers = self.observers.lock().await;
            for process_ids in observers.values_mut() {
                process_ids.retain(|process_id| !prunable.contains(process_id));
            }
            observers.retain(|_, process_ids| !process_ids.is_empty());
        }
        self.wake_targets
            .lock()
            .await
            .retain(|process_id, _| !prunable.contains(process_id));
        self.leases
            .lock()
            .await
            .retain(|process_id, _| !prunable.contains(process_id));
        self.handovers
            .lock()
            .await
            .retain(|(process_id, _), _| !prunable.contains(process_id));
        // Match the SQL process FK's ON DELETE CASCADE exactly. Allocation
        // floors intentionally survive because they are not process-owned.
        self.wake_deliveries
            .lock()
            .await
            .retain(|_, delivery| !prunable.contains(&delivery.wake.process_id));
        Ok(ProcessPruneReport {
            pruned_processes: prunable.len(),
            pruned_events,
            pruned_trigger_deliveries: 0,
        })
    }
}
