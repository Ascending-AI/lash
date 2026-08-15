use super::*;

impl TestLocalProcessRegistry {
    #[doc(hidden)]
    pub async fn worklist_page_reads_for_testing(
        &self,
    ) -> Vec<(usize, Option<super::super::ProcessWorklistCursor>)> {
        self.worklist_page_reads.lock().await.clone()
    }

    pub(super) async fn next_change_seq(&self) -> u64 {
        let mut next = self.next_change_seq.lock().await;
        *next = next.saturating_add(1);
        *next
    }

    pub(super) async fn process_miss(&self, process_id: &str) -> PluginError {
        self.tombstones.lock().await.get(process_id).map_or_else(
            || PluginError::Session(format!("unknown process `{process_id}`")),
            |tombstone| PluginError::ProcessNoLongerRetained {
                terminal_label: tombstone.terminal_label.clone(),
                pruned_at_ms: tombstone.pruned_at_ms,
            },
        )
    }

    pub(super) async fn insert_process(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<ProcessRecord, PluginError> {
        let registration = prepare_process_registration(registration)?;
        let registration_fingerprint =
            crate::runtime::process_registration_fingerprint(&registration, observers);
        let mut observer_set = observers.to_vec();
        observer_set.sort();
        observer_set.dedup();
        let mut managed = self.managed.lock().await;
        if let Some(existing) = managed.get(&registration.id) {
            if existing.record.registration_fingerprint == registration_fingerprint {
                return Ok(existing.record.clone());
            }
            return Err(PluginError::Session(format!(
                "process `{}` registration fingerprint conflict: existing {}, new {}",
                registration.id, existing.record.registration_fingerprint, registration_fingerprint
            )));
        }
        let id = registration.id.clone();
        let wake_session_id = registration.wake_session_id.clone();
        let record = ProcessRecord::from_prepared_registration(
            registration,
            registration_fingerprint,
            self.clock.timestamp_ms(),
        );
        let change_seq = self.next_change_seq().await;
        managed.insert(
            id.clone(),
            ManagedProcessRecord {
                record: record.clone(),
                change_seq,
                events: Vec::new(),
                keyed_events: HashMap::new(),
                parent_end_actions: None,
            },
        );
        if let Some(target) = wake_session_id {
            self.wake_targets.lock().await.insert(id.clone(), target);
        }
        for session_id in observer_set {
            self.observers
                .lock()
                .await
                .entry(session_id)
                .or_default()
                .insert(id.clone());
        }
        Ok(record)
    }
}
