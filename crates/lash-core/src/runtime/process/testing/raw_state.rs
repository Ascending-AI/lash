use super::*;

impl TestLocalProcessRegistry {
    #[doc(hidden)]
    pub async fn raw_state_for_testing(&self) -> RawProcessRegistryStateForTesting {
        let managed = self.managed.lock().await;
        let mut records = managed
            .values()
            .map(|entry| (entry.record.clone(), entry.change_seq))
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.0.id.cmp(&right.0.id));
        let mut events = managed
            .iter()
            .flat_map(|(process_id, entry)| {
                entry
                    .events
                    .iter()
                    .cloned()
                    .map(|event| (process_id.clone(), event))
            })
            .collect::<Vec<_>>();
        events.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.sequence.cmp(&right.1.sequence))
        });
        let process_incarnations = managed
            .iter()
            .map(|(process_id, entry)| {
                (
                    process_id.clone(),
                    entry.record.incarnation.registration_sequence(),
                )
            })
            .collect::<HashMap<_, _>>();
        drop(managed);

        let observers = self.observers.lock().await;
        let mut observer_rows = observers
            .iter()
            .flat_map(|(session_id, process_ids)| {
                process_ids.iter().cloned().map(|process_id| {
                    let incarnation = process_incarnations
                        .get(&process_id)
                        .expect("observer edge references a managed process")
                        .to_owned();
                    (session_id.clone(), process_id, incarnation)
                })
            })
            .collect::<Vec<_>>();
        observer_rows.sort();
        drop(observers);

        let mut leases = self
            .leases
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        leases.sort_by(|left, right| left.process_id.cmp(&right.process_id));
        let mut wake_deliveries = self
            .wake_deliveries
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        wake_deliveries.sort_by(|left, right| left.delivery_id.cmp(&right.delivery_id));
        let mut tombstones = self
            .tombstones
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        // One process id owns one tombstone per incarnation, so the process id
        // alone is not a total order. The SQL registries observe
        // `ORDER BY process_id, incarnation`; the map behind this projection is
        // a `HashMap`, so tie-breaking on the incarnation is what keeps the
        // in-memory observation equal to theirs instead of randomly ordered.
        tombstones.sort_by(|left, right| {
            left.process_id
                .cmp(&right.process_id)
                .then_with(|| left.incarnation.cmp(&right.incarnation))
        });
        let mut wake_allocation_floors = self
            .wake_allocation_floors
            .lock()
            .await
            .iter()
            .map(|((session_id, process_id), sequence)| {
                (session_id.clone(), process_id.clone(), *sequence)
            })
            .collect::<Vec<_>>();
        wake_allocation_floors.sort();

        RawProcessRegistryStateForTesting {
            records,
            events,
            observers: observer_rows,
            leases,
            wake_deliveries,
            wake_allocation_floors,
            tombstones,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SQL registries observe tombstones as `ORDER BY process_id,
    /// incarnation`. One process id owns one tombstone per incarnation, so the
    /// in-memory projection must break the process-id tie on the incarnation:
    /// without that the rows come out in `HashMap` order and the cross-backend
    /// differential diverges at random (FIG-3013).
    #[tokio::test]
    async fn tombstone_observation_breaks_the_process_id_tie_on_the_incarnation() {
        let registry = TestLocalProcessRegistry::default();
        let process_id = ProcessId::from("shared-name");
        {
            let mut tombstones = registry.tombstones.lock().await;
            // Sixteen incarnations: a `HashMap` ordering that happens to come
            // out ascending by chance is a 1-in-16! event.
            for sequence in 1..=16_u64 {
                let incarnation = ProcessIncarnation::from_registration_sequence(sequence);
                tombstones.insert(
                    (process_id.to_string(), incarnation),
                    ProcessTombstone {
                        process_id: process_id.clone(),
                        incarnation,
                        terminal_label: "completed".to_string(),
                        pruned_at_ms: 1_000,
                        pruned_change_seq: sequence,
                    },
                );
            }
        }

        let observed = registry
            .raw_state_for_testing()
            .await
            .tombstones
            .into_iter()
            .map(|tombstone| tombstone.incarnation.registration_sequence())
            .collect::<Vec<_>>();

        assert_eq!(observed, (1..=16).collect::<Vec<_>>());
    }
}
