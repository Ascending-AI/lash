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
        drop(managed);

        let observers = self.observers.lock().await;
        let mut observer_rows = observers
            .iter()
            .flat_map(|(session_id, process_ids)| {
                process_ids
                    .iter()
                    .cloned()
                    .map(|process_id| (session_id.clone(), process_id))
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
        tombstones.sort_by(|left, right| left.process_id.cmp(&right.process_id));
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
