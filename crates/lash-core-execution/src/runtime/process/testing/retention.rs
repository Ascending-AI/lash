use super::*;

#[async_trait::async_trait]
impl super::super::registry::ProcessRetention for TestLocalProcessRegistry {
    async fn pending_process_artifact_cleanup(
        &self,
    ) -> Result<Vec<crate::ProcessArtifactCleanup>, PluginError> {
        let state = self.state.lock().await;
        let mut pending = state.artifact_cleanup.values().cloned().collect::<Vec<_>>();
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
    ) -> Result<crate::ProcessArtifactCleanupAck, PluginError> {
        self.write(async |state| {
            let process_ref = crate::ProcessRef::new(process_id.clone(), incarnation);
            let found = state
                .managed
                .get(process_id)
                .map(|record| record.record.incarnation);
            let removed = state
                .artifact_cleanup
                .remove(&(process_id.clone(), incarnation))
                .is_some();
            Ok(match found {
                Some(found) if found != incarnation => {
                    crate::ProcessArtifactCleanupAck::StaleIncarnation {
                        expected: process_ref,
                        found: crate::ProcessRef::new(process_id.clone(), found),
                    }
                }
                _ if removed => crate::ProcessArtifactCleanupAck::Acknowledged { process_ref },
                _ => crate::ProcessArtifactCleanupAck::Unknown { process_ref },
            })
        })
        .await
    }

    async fn compact_process_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: ProjectionWatermark,
        trigger_store: Option<&dyn crate::TriggerStore>,
    ) -> Result<usize, PluginError> {
        self.write(async |state| {
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
            let pending_artifact_cleanup = state
                .artifact_cleanup
                .keys()
                .cloned()
                .collect::<HashSet<_>>();
            let tombstones = &mut state.tombstones;
            let before = tombstones.len();
            let mut compacted_through = None;
            tombstones.retain(|_, tombstone| {
                let retained = tombstone.pruned_at_ms >= cutoff_epoch_ms
                    || max_change_seq
                        .is_some_and(|max_change_seq| tombstone.pruned_change_seq > max_change_seq)
                    || outstanding_trigger_delivery_process_ids
                        .contains(tombstone.process_id.as_str())
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
                state.tombstone_compaction_horizon =
                    state.tombstone_compaction_horizon.max(compacted_through);
            }
            Ok(before - tombstones.len())
        })
        .await
    }

    async fn prunable_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: ProjectionWatermark,
    ) -> Result<Vec<ProcessId>, PluginError> {
        let state = self.state.lock().await;
        let pending = Self::processes_with_pending_deliveries(&state);
        Ok(Self::prunable_process_ids(
            &state.managed,
            cutoff_epoch_ms,
            filter.as_ref(),
            watermark,
            &pending,
        ))
    }

    async fn prune_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: ProjectionWatermark,
    ) -> Result<ProcessPruneReport, PluginError> {
        self.write(async |state| {
            let processes_with_pending_deliveries = Self::processes_with_pending_deliveries(state);
            let mut pruned_events = 0;
            let prunable = Self::prunable_process_ids(
                &state.managed,
                cutoff_epoch_ms,
                filter.as_ref(),
                watermark,
                &processes_with_pending_deliveries,
            );
            let pruned_at_ms = self.clock.timestamp_ms();
            for id in &prunable {
                if let Some(record) = state.managed.remove(id) {
                    pruned_events += record.events.len();
                    let pruned_change_seq = next_change_seq(state);
                    let cleanup = crate::ProcessArtifactCleanup::from_record(&record.record);
                    state
                        .artifact_cleanup
                        .insert((id.clone(), record.record.incarnation), cleanup);
                    state.tombstones.insert(
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
            let prunable: HashSet<ProcessId> = prunable.into_iter().collect();
            parent_end::reclaim_settled_plans(state, cutoff_epoch_ms);
            self.pause_prune_after_managed_removal().await;
            for process_ids in state.observers.values_mut() {
                process_ids.retain(|process_id| !prunable.contains(process_id));
            }
            state
                .observers
                .retain(|_, process_ids| !process_ids.is_empty());
            state
                .wake_targets
                .retain(|process_id, _| !prunable.contains(process_id));
            state
                .leases
                .retain(|process_id, _| !prunable.contains(process_id));
            state
                .handovers
                .retain(|(process_id, _), _| !prunable.contains(process_id));
            // Match the SQL process FK's ON DELETE CASCADE exactly. Allocation
            // floors intentionally survive because they are not process-owned.
            state
                .wake_deliveries
                .retain(|_, delivery| !prunable.contains(&delivery.wake.process_id));
            Ok(ProcessPruneReport {
                pruned_processes: prunable.len(),
                pruned_events,
                pruned_trigger_deliveries: 0,
                artifact_cleanup_acknowledgements: Vec::new(),
            })
        })
        .await
    }
}
