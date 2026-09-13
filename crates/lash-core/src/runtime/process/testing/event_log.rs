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
        let _transaction = self.transaction.lock().await;
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(self.process_miss(process_id).await);
        };
        self.append_managed_event(record, request).await
    }

    async fn append_event_ref(
        &self,
        process_ref: &crate::ProcessRef,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        super::super::validate_generic_process_event_append(&request)?;
        let _transaction = self.transaction.lock().await;
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(&process_ref.process_id) else {
            return Err(self.process_miss(&process_ref.process_id).await);
        };
        if record.record.incarnation != process_ref.incarnation {
            return Err(super::registry_transitions::process_incarnation_superseded(
                process_ref,
                record.record.incarnation,
            ));
        }
        self.append_managed_event(record, request).await
    }

    async fn append_event_with_authority(
        &self,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        let _transaction = self.transaction.lock().await;
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(self.process_miss(process_id).await);
        };
        let leases = self.leases.lock().await;
        validate_in_memory_execution_authority(
            &leases,
            process_id,
            &record.record,
            authority,
            None,
            self.clock.timestamp_ms(),
        )?;
        self.pause_execution_write_after_validation().await;
        let result = self.append_managed_event(record, request).await;
        drop(leases);
        result
    }

    async fn events_after(
        &self,
        process_id: &ProcessId,
        after_sequence: u64,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        self.process_events_read_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _transaction = self.transaction.lock().await;
        if let Some(error) = self.process_events_read_error.lock().await.take() {
            return Err(error);
        }
        let managed = self.managed.lock().await;
        let Some(record) = managed.get(process_id) else {
            return Err(self.process_miss(process_id).await);
        };
        Ok(record
            .events
            .iter()
            .filter(|event| event.sequence > after_sequence)
            .cloned()
            .collect())
    }

    async fn events_after_ref(
        &self,
        process_ref: &crate::ProcessRef,
        after_sequence: u64,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        self.process_events_read_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _transaction = self.transaction.lock().await;
        if let Some(error) = self.process_events_read_error.lock().await.take() {
            return Err(error);
        }
        let managed = self.managed.lock().await;
        let Some(record) = managed.get(&process_ref.process_id) else {
            return Err(self.process_miss(&process_ref.process_id).await);
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
            .filter(|event| event.sequence > after_sequence)
            .cloned()
            .collect())
    }

    async fn count_events_through_ref(
        &self,
        process_ref: &crate::ProcessRef,
        event_type: &str,
        up_to_sequence: u64,
    ) -> Result<u64, PluginError> {
        self.process_events_read_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _transaction = self.transaction.lock().await;
        if let Some(error) = self.process_events_read_error.lock().await.take() {
            return Err(error);
        }
        let managed = self.managed.lock().await;
        let Some(record) = managed.get(&process_ref.process_id) else {
            return Err(self.process_miss(&process_ref.process_id).await);
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
}
