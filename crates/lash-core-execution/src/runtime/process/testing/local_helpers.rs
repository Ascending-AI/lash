use super::*;

impl TestLocalProcessRegistry {
    pub async fn worklist_page_reads_for_testing(
        &self,
    ) -> Vec<(usize, Option<super::super::ProcessWorklistCursor>)> {
        self.worklist_page_reads.lock().await.clone()
    }
}

pub(super) fn next_change_seq(state: &mut RegistryState) -> u64 {
    state.next_change_seq = state.next_change_seq.saturating_add(1);
    state.next_change_seq
}

pub(super) fn process_miss(state: &RegistryState, process_id: &ProcessId) -> PluginError {
    state
        .tombstones
        .values()
        .filter(|tombstone| tombstone.process_id == *process_id)
        .max_by_key(|tombstone| tombstone.incarnation)
        .map_or_else(
            || PluginError::ProcessUnknown {
                process_id: ProcessId::from(process_id.to_string()),
            },
            |tombstone| PluginError::ProcessNoLongerRetained {
                terminal_label: tombstone.terminal_label.clone(),
                pruned_at_ms: tombstone.pruned_at_ms,
            },
        )
}

pub(super) fn insert_process(
    registry: &TestLocalProcessRegistry,
    state: &mut RegistryState,
    registration: ProcessRegistration,
    observers: &[SessionId],
) -> Result<crate::ProcessRegistrationOutcome, PluginError> {
    let registration = prepare_process_registration(registration)?;
    let registration_fingerprint =
        crate::runtime::process_registration_fingerprint(&registration, observers);
    let mut observer_set = observers.to_vec();
    observer_set.sort();
    observer_set.dedup();
    if let Some(existing) = state.managed.get(&registration.id) {
        if existing.record.registration_fingerprint == registration_fingerprint {
            return Ok(crate::ProcessRegistrationOutcome::existing(
                existing.record.clone(),
            ));
        }
        return Err(crate::durable_identity_conflict(format!(
            "process `{}` registration fingerprint conflict: existing {}, new {}",
            registration.id, existing.record.registration_fingerprint, registration_fingerprint
        )));
    }
    // Late-registration fence. A `Cancel` child whose parent already has a
    // ledger row is refused, so a child commits either before the row and
    // is swept or after it and is refused. There is no third interleaving
    // in which a child outlives a parent that declared Cancel.
    if registration.lifecycle.on_parent_end == crate::OnParentEnd::Cancel
        && parent_end::parent_end_plan(state, &registration.lifecycle.parent).is_some()
    {
        return Err(crate::PluginError::ParentEnded {
            process_id: registration.id.clone(),
            parent: registration.lifecycle.parent.clone(),
        });
    }
    let id = registration.id.clone();
    let wake_session_id = registration.wake_session_id.clone();
    let change_seq = next_change_seq(state);
    let record = ProcessRecord::from_prepared_registration(
        registration,
        registration_fingerprint,
        ProcessIncarnation::from_registration_sequence(change_seq),
        registry.clock.timestamp_ms(),
    );
    state.managed.insert(
        ProcessId::from(id.clone().to_string()),
        ManagedProcessRecord {
            record: record.clone(),
            change_seq,
            events: Vec::new(),
            keyed_events: HashMap::new(),
        },
    );
    if let Some(target) = wake_session_id {
        state
            .wake_targets
            .insert(ProcessId::from(id.clone().to_string()), target);
    }
    for session_id in observer_set {
        state
            .observers
            .entry(session_id)
            .or_default()
            .insert(ProcessId::from(id.clone().to_string()));
    }
    Ok(crate::ProcessRegistrationOutcome::created(record))
}
