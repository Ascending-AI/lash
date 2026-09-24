//! Process-event log operations for the in-memory registry double.

use super::*;

#[async_trait::async_trait]
impl super::super::registry::ProcessEventLog for TestLocalProcessRegistry {
    async fn append_event(
        &self,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        super::super::validate_generic_process_event_append(&request)?;
        self.write(async |state| {
            if !state.managed.contains_key(process_id) {
                return Err(process_miss(state, process_id));
            }
            self.append_managed_event(state, process_id, request).await
        })
        .await
    }

    async fn append_event_ref(
        &self,
        process_ref: &crate::ProcessRef,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        super::super::validate_generic_process_event_append(&request)?;
        self.write(async |state| {
            let Some(record) = state.managed.get(&process_ref.process_id) else {
                return Err(process_miss(state, &process_ref.process_id));
            };
            if record.record.incarnation != process_ref.incarnation {
                return Err(super::registry_transitions::process_incarnation_superseded(
                    process_ref,
                    record.record.incarnation,
                ));
            }
            self.append_managed_event(state, &process_ref.process_id, request)
                .await
        })
        .await
    }

    async fn append_event_with_authority(
        &self,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        self.write(async |state| {
            let Some(record) = state.managed.get(process_id) else {
                return Err(process_miss(state, process_id));
            };
            validate_in_memory_execution_authority(
                &state.leases,
                process_id,
                &record.record,
                authority,
                None,
                self.clock.timestamp_ms(),
            )?;
            self.pause_execution_write_after_validation().await;
            self.append_managed_event(state, process_id, request).await
        })
        .await
    }

    async fn event_page_ref(
        &self,
        process_ref: &crate::ProcessRef,
        after_sequence: u64,
        limit: std::num::NonZeroUsize,
        mode: crate::ProcessEventQueryMode,
    ) -> Result<crate::ProcessEventReadOutcome<crate::ProcessEventPage>, PluginError> {
        let process_id = &process_ref.process_id;
        if i64::try_from(after_sequence).is_err() {
            return Err(PluginError::Session(
                "process event page sequence exceeds the durable sequence range".to_string(),
            ));
        }
        if let Some(error) = self.process_events_read_error.lock().await.take() {
            return Err(error);
        }
        let state = self.state.lock().await;
        let Some(record) = state.managed.get(process_id) else {
            return match process_miss(&state, process_id) {
                PluginError::ProcessNoLongerRetained {
                    terminal_label,
                    pruned_at_ms,
                } => Ok(crate::ProcessEventReadOutcome::NoLongerRetained(
                    crate::ProcessEventHistoryRetention::Pruned {
                        terminal_label,
                        pruned_at_ms,
                    },
                )),
                error => Err(error),
            };
        };
        if process_ref.incarnation != record.record.incarnation {
            return Ok(crate::ProcessEventReadOutcome::NoLongerRetained(
                crate::ProcessEventHistoryRetention::Retired {
                    requested_incarnation: process_ref.incarnation,
                    current_incarnation: record.record.incarnation,
                },
            ));
        }
        let take = limit.get().saturating_add(1);
        let events = record
            .events
            .iter()
            .filter(|event| event.sequence > after_sequence)
            .take(take)
            .cloned()
            .collect::<Vec<_>>();
        let page = match mode {
            crate::ProcessEventQueryMode::Full => {
                crate::ProcessEventPage::from_full_rows(events, limit)
            }
            crate::ProcessEventQueryMode::Lite => crate::ProcessEventPage::from_lite_rows(
                events
                    .into_iter()
                    .map(|event| crate::ProcessEventLite {
                        sequence: event.sequence,
                        event_type: event.event_type,
                    })
                    .collect(),
                limit,
            ),
        };
        Ok(crate::ProcessEventReadOutcome::Retained(page))
    }

    async fn count_events_through(
        &self,
        process_id: &ProcessId,
        event_type: &str,
        up_to_sequence: u64,
    ) -> Result<u64, PluginError> {
        let state = self.state.lock().await;
        let Some(record) = state.managed.get(process_id) else {
            return Err(process_miss(&state, process_id));
        };
        Ok(record
            .events
            .iter()
            .filter(|event| {
                event.sequence <= up_to_sequence && event.event_type.as_str() == event_type
            })
            .count() as u64)
    }

    async fn count_events_through_ref(
        &self,
        process_ref: &crate::ProcessRef,
        event_type: &str,
        up_to_sequence: u64,
    ) -> Result<u64, PluginError> {
        if let Some(error) = self.process_events_read_error.lock().await.take() {
            return Err(error);
        }
        let state = self.state.lock().await;
        let Some(record) = state.managed.get(&process_ref.process_id) else {
            return Err(process_miss(&state, &process_ref.process_id));
        };
        if record.record.incarnation != process_ref.incarnation {
            return Err(super::registry_transitions::process_incarnation_superseded(
                process_ref,
                record.record.incarnation,
            ));
        }
        Ok(record
            .events
            .iter()
            .filter(|event| {
                event.sequence <= up_to_sequence && event.event_type.as_str() == event_type
            })
            .count() as u64)
    }

    async fn recent_events(
        &self,
        process_id: &ProcessId,
        limit: usize,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        if let Some(error) = self.process_events_read_error.lock().await.take() {
            return Err(error);
        }
        let state = self.state.lock().await;
        let Some(record) = state.managed.get(process_id) else {
            return Err(process_miss(&state, process_id));
        };
        let start = record.events.len().saturating_sub(limit);
        Ok(record.events[start..].to_vec())
    }
}
