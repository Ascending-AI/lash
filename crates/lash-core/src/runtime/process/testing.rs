#![cfg(any(test, feature = "testing"))]

use lash_sansio::sync::MutexExt;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::plugin::PluginError;

use super::events::{
    ProcessAwaitOutput, ProcessCompletionAuthority, ProcessEvent, ProcessEventAppendRequest,
    ProcessEventAppendResult, terminal_append_request,
};
use super::model::{
    AbandonRequest, ProcessChange, ProcessChangeCursor, ProcessCompletionOutcome,
    ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessLease, ProcessLeaseClaimOutcome,
    ProcessLeaseCompletion, ProcessListFilter, ProcessObserverBy, ProcessRecord,
    ProcessRegistration, ProcessSessionDeleteReport, ProcessStartOutcome, ProcessStarted,
    ProcessTombstone, SessionId, WaitState,
};
use super::references::ProcessLiveReferenceSummary;
use super::registry::{ProcessPruneReport, ProcessRegistry, ProjectionWatermark};
use super::registry_transitions;
use super::validation::{
    ProcessStartPlan, prepare_process_event_append, prepare_process_registration,
    prepare_process_start,
};

mod continuation;
#[cfg(test)]
mod identity;
mod raw_state;
mod support;
mod types;
mod worklist;
pub use support::TestProcessRegistryWriteExt;
use support::{
    ExecutionWritePause, process_external_ref_conflict, process_lease_expired,
    validate_in_memory_execution_authority,
};
use types::{ManagedLeaseMap, ManagedProcessRecord};
pub use types::{RawProcessRegistryStateForTesting, TestLocalProcessRegistry};

impl TestLocalProcessRegistry {
    #[doc(hidden)]
    pub async fn worklist_page_reads_for_testing(
        &self,
    ) -> Vec<(usize, Option<super::ProcessWorklistCursor>)> {
        self.worklist_page_reads.lock().await.clone()
    }

    async fn next_change_seq(&self) -> u64 {
        let mut next = self.next_change_seq.lock().await;
        *next = next.saturating_add(1);
        *next
    }

    async fn process_miss(&self, process_id: &str) -> PluginError {
        self.tombstones.lock().await.get(process_id).map_or_else(
            || PluginError::Session(format!("unknown process `{process_id}`")),
            |tombstone| PluginError::ProcessNoLongerRetained {
                terminal_label: tombstone.terminal_label.clone(),
                pruned_at_ms: tombstone.pruned_at_ms,
            },
        )
    }

    async fn insert_process(
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

    async fn append_managed_event(
        &self,
        record: &mut ManagedProcessRecord,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendResult, PluginError> {
        let replay_lookup = request
            .replay
            .as_ref()
            .and_then(|replay| record.keyed_events.get(replay.key.as_str()))
            .cloned();
        let last_sequence = record.events.last().map(|event| event.sequence);
        let wake_session_id = self
            .wake_targets
            .lock()
            .await
            .get(&record.record.id)
            .cloned();
        let sender_floor = match wake_session_id.as_ref() {
            Some(target_session_id) => self
                .wake_allocation_floors
                .lock()
                .await
                .get(&(target_session_id.clone(), record.record.id.clone()))
                .copied(),
            None => None,
        };
        self.pause_append_after_target_snapshot().await;
        let sequence = super::allocate_process_event_sequence(last_sequence, sender_floor)?;
        let now = self.clock.timestamp_ms();
        let prepared = prepare_process_event_append(
            &record.record,
            request,
            sequence,
            last_sequence,
            replay_lookup,
            now,
            wake_session_id.as_deref(),
        )?;
        match prepared {
            super::ProcessEventAppendPlan::Replay {
                event,
                repair_record,
                wake_delivery,
                ..
            } => {
                self.insert_wake_delivery(wake_delivery.as_ref()).await?;
                if let Some(repaired) = repair_record {
                    record.record = repaired;
                    record.change_seq = self.next_change_seq().await;
                }
                Ok(ProcessEventAppendResult {
                    event,
                    wake_delivery,
                })
            }
            super::ProcessEventAppendPlan::Insert {
                event,
                projected_record,
                wake_delivery,
                ..
            } => {
                self.insert_wake_delivery(wake_delivery.as_ref()).await?;
                self.advance_wake_allocation_floor(
                    wake_session_id.as_deref(),
                    &record.record.id,
                    sequence,
                )
                .await;
                self.pause_append_after_outbox().await;
                record.record = projected_record;
                record.change_seq = self.next_change_seq().await;
                record.events.push(event.clone());
                if let Some(replay) = event.invocation.replay.clone() {
                    record.keyed_events.insert(replay.key, event.clone());
                }
                Ok(ProcessEventAppendResult {
                    event,
                    wake_delivery,
                })
            }
        }
    }

    async fn insert_wake_delivery(
        &self,
        wake: Option<&super::ProcessWakeDelivery>,
    ) -> Result<(), PluginError> {
        let Some(wake) = wake else {
            return Ok(());
        };
        let delivery = super::WakeDelivery::pending(wake.clone(), self.wake_delivery_config)?;
        self.wake_deliveries
            .lock()
            .await
            .entry(delivery.delivery_id.clone())
            .or_insert(delivery);
        Ok(())
    }

    async fn advance_wake_allocation_floor(
        &self,
        target_session_id: Option<&str>,
        process_id: &str,
        sequence: u64,
    ) {
        let Some(target_session_id) = target_session_id else {
            return;
        };
        self.wake_allocation_floors.lock().await.insert(
            (target_session_id.to_string(), process_id.to_string()),
            sequence,
        );
    }
}

#[async_trait::async_trait]
impl ProcessRegistry for TestLocalProcessRegistry {
    fn wake_delivery_config(&self) -> super::WakeDeliveryConfig {
        self.wake_delivery_config
    }

    async fn register_process_with_observers(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<ProcessRecord, PluginError> {
        let _transaction = self.transaction.lock().await;
        let process_id = registration.id.clone();
        let managed_before = self.managed.lock().await.clone();
        let observers_before = self.observers.lock().await.clone();
        let wake_targets_before = self.wake_targets.lock().await.clone();
        let result = async {
            self.insert_process(registration, observers).await?;
            let mut managed = self.managed.lock().await;
            let record = managed
                .get_mut(&process_id)
                .expect("registration inserted process");
            for session_id in observers {
                self.append_managed_event(
                    record,
                    ProcessEventAppendRequest::observer_added(
                        &process_id,
                        session_id,
                        &ProcessObserverBy::host("registration"),
                    ),
                )
                .await?;
            }
            Ok(record.record.clone())
        }
        .await;
        if result.is_err() {
            *self.managed.lock().await = managed_before;
            *self.observers.lock().await = observers_before;
            *self.wake_targets.lock().await = wake_targets_before;
        }
        result
    }

    async fn set_external_ref(
        &self,
        process_id: &str,
        external_ref: ProcessExternalRef,
    ) -> Result<ProcessRecord, PluginError> {
        let _transaction = self.transaction.lock().await;
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(self.process_miss(process_id).await);
        };
        if let Some(existing) = &record.record.external_ref {
            if existing == &external_ref {
                return Ok(record.record.clone());
            }
            return Err(process_external_ref_conflict(
                process_id,
                existing,
                &external_ref,
            ));
        }
        let request = ProcessEventAppendRequest::external_ref_set(process_id, &external_ref);
        self.append_managed_event(record, request).await?;
        Ok(record.record.clone())
    }

    async fn add_observer(
        &self,
        session_id: &str,
        process_id: &str,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        let _transaction = self.transaction.lock().await;
        let managed_before = self.managed.lock().await.clone();
        let observers_before = self.observers.lock().await.clone();
        let result = async {
            let mut managed = self.managed.lock().await;
            let Some(record) = managed.get_mut(process_id) else {
                return Err(self.process_miss(process_id).await);
            };
            let inserted = self
                .observers
                .lock()
                .await
                .entry(session_id.to_string())
                .or_default()
                .insert(process_id.to_string());
            if inserted {
                self.append_managed_event(
                    record,
                    ProcessEventAppendRequest::observer_added(process_id, session_id, &by),
                )
                .await?;
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            *self.managed.lock().await = managed_before;
            *self.observers.lock().await = observers_before;
        }
        result
    }

    async fn remove_observer(
        &self,
        session_id: &str,
        process_id: &str,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        let _transaction = self.transaction.lock().await;
        let managed_before = self.managed.lock().await.clone();
        let observers_before = self.observers.lock().await.clone();
        let result = async {
            let mut managed = self.managed.lock().await;
            let Some(record) = managed.get_mut(process_id) else {
                return Err(self.process_miss(process_id).await);
            };
            let removed = self
                .observers
                .lock()
                .await
                .get_mut(session_id)
                .is_some_and(|processes| processes.remove(process_id));
            if removed {
                self.append_managed_event(
                    record,
                    ProcessEventAppendRequest::observer_removed(process_id, session_id, &by),
                )
                .await?;
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            *self.managed.lock().await = managed_before;
            *self.observers.lock().await = observers_before;
        }
        result
    }

    async fn transfer_observers(
        &self,
        from_session_id: &str,
        to_session_id: &str,
        process_ids: &[String],
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        let _transaction = self.transaction.lock().await;
        let managed_before = self.managed.lock().await.clone();
        let observers_before = self.observers.lock().await.clone();
        let result = async {
            for process_id in process_ids {
                let mut managed = self.managed.lock().await;
                let Some(record) = managed.get_mut(process_id) else {
                    return Err(self.process_miss(process_id).await);
                };
                let mut observers = self.observers.lock().await;
                let removed = observers
                    .get_mut(from_session_id)
                    .is_some_and(|processes| processes.remove(process_id));
                if !removed {
                    return Err(PluginError::Session(format!(
                        "process `{process_id}` is not observed by session `{from_session_id}`"
                    )));
                }
                observers
                    .entry(to_session_id.to_string())
                    .or_default()
                    .insert(process_id.clone());
                drop(observers);
                self.append_managed_event(
                    record,
                    ProcessEventAppendRequest::observer_removed(process_id, from_session_id, &by),
                )
                .await?;
                self.append_managed_event(
                    record,
                    ProcessEventAppendRequest::observer_added(process_id, to_session_id, &by),
                )
                .await?;
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            *self.managed.lock().await = managed_before;
            *self.observers.lock().await = observers_before;
        }
        result
    }

    async fn list_observed_by(&self, session_id: &str) -> Result<Vec<ProcessRecord>, PluginError> {
        let process_ids = self
            .observers
            .lock()
            .await
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        let managed = self.managed.lock().await;
        let mut records = process_ids
            .into_iter()
            .filter_map(|process_id| managed.get(&process_id).map(|row| row.record.clone()))
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(records)
    }

    async fn observers_for_process(&self, process_id: &str) -> Result<Vec<SessionId>, PluginError> {
        if !self.managed.lock().await.contains_key(process_id) {
            return Err(self.process_miss(process_id).await);
        }
        let mut sessions = self
            .observers
            .lock()
            .await
            .iter()
            .filter(|(_, processes)| processes.contains(process_id))
            .map(|(session_id, _)| session_id.clone())
            .collect::<Vec<_>>();
        sessions.sort();
        Ok(sessions)
    }

    async fn retarget_subscription(
        &self,
        process_id: &str,
        target: Option<&str>,
    ) -> Result<(), PluginError> {
        let _transaction = self.transaction.lock().await;
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(self.process_miss(process_id).await);
        };
        let old_target = self.wake_targets.lock().await.get(process_id).cloned();
        if old_target.as_deref() == target {
            return Ok(());
        }
        self.append_managed_event(
            record,
            ProcessEventAppendRequest::subscription_retargeted(process_id, target),
        )
        .await?;
        let mut wake_targets = self.wake_targets.lock().await;
        match target {
            Some(target) => {
                wake_targets.insert(process_id.to_string(), target.to_string());
            }
            None => {
                wake_targets.remove(process_id);
            }
        }
        drop(wake_targets);
        if let Some(old_target) = old_target {
            for delivery in self.wake_deliveries.lock().await.values_mut() {
                if delivery.state == super::WakeDeliveryState::Pending
                    && delivery.wake.process_id == process_id
                    && delivery.wake.target_session_id == old_target
                {
                    delivery.state = super::WakeDeliveryState::Discarded;
                    delivery.discard_reason = Some(super::WakeDiscardReason::Retargeted);
                }
            }
        }
        Ok(())
    }

    async fn delete_session_process_state(
        &self,
        session_id: &str,
    ) -> Result<ProcessSessionDeleteReport, PluginError> {
        let _transaction = self.transaction.lock().await;
        let removed_observer_count = self
            .observers
            .lock()
            .await
            .remove(session_id)
            .map_or(0, |processes| processes.len());
        let cleared_processes = self
            .wake_targets
            .lock()
            .await
            .iter()
            .filter(|(_, target)| target.as_str() == session_id)
            .map(|(process_id, _)| process_id.clone())
            .collect::<Vec<_>>();
        self.wake_targets
            .lock()
            .await
            .retain(|_, target| target != session_id);
        self.wake_allocation_floors
            .lock()
            .await
            .retain(|(target_session_id, _), _| target_session_id != session_id);
        let mut discarded_wake_delivery_count = 0;
        for delivery in self.wake_deliveries.lock().await.values_mut() {
            if delivery.state == super::WakeDeliveryState::Pending
                && delivery.wake.target_session_id == session_id
            {
                delivery.state = super::WakeDeliveryState::Discarded;
                delivery.discard_reason = Some(super::WakeDiscardReason::TargetGone);
                discarded_wake_delivery_count += 1;
            }
        }
        Ok(ProcessSessionDeleteReport {
            session_id: session_id.to_string(),
            removed_observer_count,
            discarded_wake_delivery_count,
            cleared_subscription_count: cleared_processes.len(),
        })
    }

    async fn wake_allocation_floor_for_testing(
        &self,
        target_session_id: &str,
        process_id: &str,
    ) -> Result<Option<u64>, PluginError> {
        Ok(self
            .wake_allocation_floors
            .lock()
            .await
            .get(&(target_session_id.to_string(), process_id.to_string()))
            .copied())
    }

    async fn append_event(
        &self,
        process_id: &str,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendResult, PluginError> {
        super::validate_generic_process_event_append(&request)?;
        let _transaction = self.transaction.lock().await;
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(self.process_miss(process_id).await);
        };
        self.append_managed_event(record, request).await
    }

    async fn append_event_with_authority(
        &self,
        process_id: &str,
        request: ProcessEventAppendRequest,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessEventAppendResult, PluginError> {
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
        process_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        let _transaction = self.transaction.lock().await;
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

    async fn complete_process(
        &self,
        process_id: &str,
        await_output: ProcessAwaitOutput,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        let _transaction = self.transaction.lock().await;
        // Hold the `managed` lock across load→validate→append so no other
        // completion can complete, prune, and re-register the row with a
        // different disposition between the validation and the terminal append.
        // The row we validate is the row we append to.
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(self.process_miss(process_id).await);
        };
        if record.record.is_terminal() {
            return Ok(ProcessCompletionOutcome::from_stored(
                record.record.clone(),
                &await_output,
            ));
        }
        authority.validate(process_id, record.record.disposition, &await_output)?;
        let request = terminal_append_request(process_id, &await_output, Some(&authority));
        let replay_lookup = request
            .replay
            .as_ref()
            .and_then(|replay| record.keyed_events.get(replay.key.as_str()))
            .cloned();
        let last_sequence = record.events.last().map(|event| event.sequence);
        let wake_session_id = self.wake_targets.lock().await.get(process_id).cloned();
        let sender_floor = match wake_session_id.as_ref() {
            Some(target_session_id) => self
                .wake_allocation_floors
                .lock()
                .await
                .get(&(target_session_id.clone(), process_id.to_string()))
                .copied(),
            None => None,
        };
        let sequence = super::allocate_process_event_sequence(last_sequence, sender_floor)?;
        let now = self.clock.timestamp_ms();
        let prepared = prepare_process_event_append(
            &record.record,
            request,
            sequence,
            last_sequence,
            replay_lookup,
            now,
            wake_session_id.as_deref(),
        )?;
        let outcome = match prepared {
            super::ProcessEventAppendPlan::Replay {
                repair_record,
                wake_delivery,
                ..
            } => {
                self.insert_wake_delivery(wake_delivery.as_ref()).await?;
                if let Some(repaired) = repair_record {
                    record.record = repaired;
                    record.change_seq = self.next_change_seq().await;
                }
                ProcessCompletionOutcome::AlreadyApplied {
                    stored: record.record.clone(),
                }
            }
            super::ProcessEventAppendPlan::Insert {
                event,
                projected_record,
                wake_delivery,
                ..
            } => {
                self.insert_wake_delivery(wake_delivery.as_ref()).await?;
                self.advance_wake_allocation_floor(
                    wake_session_id.as_deref(),
                    process_id,
                    sequence,
                )
                .await;
                record.record = projected_record;
                record.change_seq = self.next_change_seq().await;
                if let Some(replay) = event.invocation.replay.clone() {
                    record.keyed_events.insert(replay.key, event.clone());
                }
                record.events.push(event);
                ProcessCompletionOutcome::Committed(record.record.clone())
            }
        };
        Ok(outcome)
    }

    async fn complete_process_with_lease(
        &self,
        lease: &ProcessLease,
        await_output: ProcessAwaitOutput,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        if let Some(error) = self.process_terminal_write_error.lock().await.clone() {
            return Err(error);
        }
        if let Some(outcome) = self.process_terminal_write_outcome.lock().await.take() {
            return Ok(outcome);
        }
        let _transaction = self.transaction.lock().await;
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(&lease.process_id) else {
            return Err(self.process_miss(&lease.process_id).await);
        };
        if record.record.is_terminal() {
            return Ok(ProcessCompletionOutcome::from_stored(
                record.record.clone(),
                &await_output,
            ));
        }
        let now = self.clock.timestamp_ms();
        let request = terminal_append_request(&lease.process_id, &await_output, None);
        let replay_lookup = request
            .replay
            .as_ref()
            .and_then(|replay| record.keyed_events.get(replay.key.as_str()))
            .cloned();
        let last_sequence = record.events.last().map(|event| event.sequence);
        let wake_session_id = self
            .wake_targets
            .lock()
            .await
            .get(&lease.process_id)
            .cloned();
        let sender_floor = match wake_session_id.as_ref() {
            Some(target_session_id) => self
                .wake_allocation_floors
                .lock()
                .await
                .get(&(target_session_id.clone(), lease.process_id.clone()))
                .copied(),
            None => None,
        };
        let sequence = super::allocate_process_event_sequence(last_sequence, sender_floor)?;
        let prepared = prepare_process_event_append(
            &record.record,
            request,
            sequence,
            last_sequence,
            replay_lookup,
            now,
            wake_session_id.as_deref(),
        )?;
        if let super::ProcessEventAppendPlan::Replay {
            repair_record,
            wake_delivery,
            ..
        } = prepared
        {
            self.insert_wake_delivery(wake_delivery.as_ref()).await?;
            if let Some(repaired) = repair_record {
                record.record = repaired;
                record.change_seq = self.next_change_seq().await;
            }
            return Ok(ProcessCompletionOutcome::AlreadyApplied {
                stored: record.record.clone(),
            });
        }

        let mut leases = self.leases.lock().await;
        let current = leases
            .get_mut(&lease.process_id)
            .filter(|current| {
                !current.lease_token.is_empty()
                    && current.owner.same_incarnation(&lease.owner)
                    && current.lease_token == lease.lease_token
                    && current.fencing_token == lease.fencing_token
                    && current.expires_at_epoch_ms > now
            })
            .ok_or_else(|| process_lease_expired(&lease.process_id))?;
        match prepared {
            super::ProcessEventAppendPlan::Replay { .. } => unreachable!("replay returned above"),
            super::ProcessEventAppendPlan::Insert {
                event,
                projected_record,
                wake_delivery,
                ..
            } => {
                self.insert_wake_delivery(wake_delivery.as_ref()).await?;
                self.advance_wake_allocation_floor(
                    wake_session_id.as_deref(),
                    &lease.process_id,
                    sequence,
                )
                .await;
                record.record = projected_record;
                if let Some(replay) = event.invocation.replay.clone() {
                    record.keyed_events.insert(replay.key, event.clone());
                }
                record.events.push(event);
            }
        }
        record.change_seq = self.next_change_seq().await;
        current.owner = crate::LeaseOwnerIdentity::opaque("", "");
        current.lease_token.clear();
        current.claimed_at_epoch_ms = 0;
        current.expires_at_epoch_ms = 0;
        Ok(ProcessCompletionOutcome::Committed(record.record.clone()))
    }

    async fn record_first_started_with_authority(
        &self,
        process_id: &str,
        started: ProcessStarted,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessStartOutcome, PluginError> {
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
            Some(&started),
            self.clock.timestamp_ms(),
        )?;
        match prepare_process_start(&record.record, &started, authority)? {
            ProcessStartPlan::AlreadyApplied => {
                return Ok(ProcessStartOutcome::AlreadyApplied(record.record.clone()));
            }
            ProcessStartPlan::AlreadyStarted { by } => {
                return Ok(ProcessStartOutcome::AlreadyStarted {
                    current: record.record.clone(),
                    by,
                });
            }
            ProcessStartPlan::AttemptsExhausted {
                attempts,
                max_attempts,
            } => {
                return Ok(ProcessStartOutcome::AttemptsExhausted {
                    current: record.record.clone(),
                    attempts,
                    max_attempts,
                });
            }
            ProcessStartPlan::Append => {}
        }
        let resumed_from_handover = record
            .record
            .first_started
            .as_deref()
            .is_some_and(|retained| authority.permits_owner_bound_resume(retained));
        let request =
            ProcessEventAppendRequest::first_started(process_id, &started, resumed_from_handover);
        self.append_managed_event(record, request).await?;
        drop(leases);
        Ok(ProcessStartOutcome::Started(record.record.clone()))
    }

    async fn request_process_abandon(
        &self,
        process_id: &str,
        request: AbandonRequest,
    ) -> Result<ProcessRecord, PluginError> {
        let _transaction = self.transaction.lock().await;
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(self.process_miss(process_id).await);
        };
        if record.record.is_terminal() {
            return Err(PluginError::Session(format!(
                "terminal process `{process_id}` cannot accept an abandon request"
            )));
        }
        if record.record.abandon_request.is_some() {
            return Ok(record.record.clone());
        }
        let append = ProcessEventAppendRequest::abandon_requested(process_id, &request);
        self.append_managed_event(record, append).await?;
        Ok(record.record.clone())
    }

    async fn set_process_wait_with_authority(
        &self,
        process_id: &str,
        wait: WaitState,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
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
        if record.record.is_terminal() {
            return Err(PluginError::Session(format!(
                "terminal process `{process_id}` cannot enter a wait state"
            )));
        }
        if record.record.wait.as_ref() == Some(&wait) {
            return Ok(record.record.clone());
        }
        let request = ProcessEventAppendRequest::wait_entered(process_id, &wait);
        self.append_managed_event(record, request).await?;
        drop(leases);
        Ok(record.record.clone())
    }

    async fn clear_process_wait_with_authority(
        &self,
        process_id: &str,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
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
        let Some(wait) = record.record.wait.clone() else {
            return Ok(record.record.clone());
        };
        let request = ProcessEventAppendRequest::wait_cleared(process_id, &wait);
        self.append_managed_event(record, request).await?;
        drop(leases);
        Ok(record.record.clone())
    }

    async fn get_process(&self, process_id: &str) -> Result<Option<ProcessRecord>, PluginError> {
        if let Some(error) = self.process_read_error.lock().await.clone() {
            return Err(error);
        }
        {
            let mut scheduled = self.process_read_error_after.lock().await;
            if let Some((remaining, error)) = scheduled.as_mut() {
                if *remaining == 0 {
                    let error = error.clone();
                    *scheduled = None;
                    return Err(error);
                }
                *remaining -= 1;
            }
        }
        if *self.process_read_absent.lock().await {
            return Ok(None);
        }
        if let Some(record) = self.process_read_override.lock().await.take() {
            return Ok(Some(record));
        }
        if let Some(record) = self.managed.lock().await.get(process_id) {
            return Ok(Some(record.record.clone()));
        }
        if self.tombstones.lock().await.contains_key(process_id) {
            return Err(self.process_miss(process_id).await);
        }
        Ok(None)
    }

    async fn list_processes(
        &self,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        let managed = self.managed.lock().await;
        let mut records = managed
            .values()
            .map(|record| record.record.clone())
            .filter(|record| filter.matches_record(record))
            .collect::<Vec<_>>();
        records.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(records)
    }

    async fn processes_changed_since(
        &self,
        cursor: ProcessChangeCursor,
        limit: usize,
    ) -> Result<(Vec<ProcessChange>, ProcessChangeCursor), PluginError> {
        if limit == 0 {
            return Ok((Vec::new(), cursor));
        }
        let managed = self.managed.lock().await;
        let mut rows = managed
            .values()
            .filter(|record| record.change_seq > cursor.store_sequence())
            .map(|record| {
                (
                    record.change_seq,
                    record.record.id.clone(),
                    ProcessChange::Upsert {
                        record: Box::new(record.record.clone()),
                    },
                )
            })
            .collect::<Vec<_>>();
        drop(managed);
        rows.extend(
            self.tombstones
                .lock()
                .await
                .values()
                .filter(|tombstone| tombstone.pruned_change_seq > cursor.store_sequence())
                .map(|tombstone| {
                    (
                        tombstone.pruned_change_seq,
                        tombstone.process_id.clone(),
                        ProcessChange::Deleted {
                            tombstone: tombstone.clone(),
                        },
                    )
                }),
        );
        rows.sort_by(|(left_seq, left_id, _), (right_seq, right_id, _)| {
            left_seq.cmp(right_seq).then_with(|| left_id.cmp(right_id))
        });
        rows.truncate(limit);
        let next_cursor = rows
            .last()
            .map(|(change_seq, _, _)| ProcessChangeCursor::from_store_sequence(*change_seq))
            .unwrap_or(cursor);
        Ok((
            rows.into_iter().map(|(_, _, change)| change).collect(),
            next_cursor,
        ))
    }

    async fn compact_process_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: ProjectionWatermark,
        trigger_store: Option<&dyn crate::TriggerStore>,
    ) -> Result<usize, PluginError> {
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
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>();
        let mut tombstones = self.tombstones.lock().await;
        let before = tombstones.len();
        tombstones.retain(|_, tombstone| {
            tombstone.pruned_at_ms >= cutoff_epoch_ms
                || max_change_seq
                    .is_some_and(|max_change_seq| tombstone.pruned_change_seq > max_change_seq)
                || outstanding_trigger_delivery_process_ids.contains(tombstone.process_id.as_str())
        });
        Ok(before - tombstones.len())
    }

    async fn claim_pending_wake_deliveries(
        &self,
        limit: usize,
    ) -> Result<Vec<super::WakeDelivery>, PluginError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let _transaction = self.transaction.lock().await;
        let now = self.clock.timestamp_ms();
        let mut deliveries = self.wake_deliveries.lock().await;
        for delivery in deliveries.values_mut() {
            if delivery.state == super::WakeDeliveryState::Enqueuing
                && delivery.next_attempt_at_ms <= now
            {
                delivery.state = super::WakeDeliveryState::Pending;
                delivery.claim_token = None;
            }
        }
        let mut ids = deliveries
            .values()
            .filter(|delivery| delivery.state == super::WakeDeliveryState::Pending)
            .filter(|delivery| delivery.next_attempt_at_ms <= now)
            .filter(|candidate| {
                !deliveries.values().any(|earlier| {
                    earlier.state != super::WakeDeliveryState::Enqueued
                        && !(earlier.state == super::WakeDeliveryState::Discarded
                            && earlier.discard_reason
                                == Some(super::WakeDiscardReason::SequenceRewound))
                        && earlier.wake.target_session_id == candidate.wake.target_session_id
                        && earlier.wake.process_id == candidate.wake.process_id
                        && earlier.wake.sequence < candidate.wake.sequence
                })
            })
            .map(|delivery| {
                (
                    delivery.next_attempt_at_ms,
                    delivery.wake.target_session_id.clone(),
                    delivery.wake.process_id.clone(),
                    delivery.wake.sequence,
                    delivery.delivery_id.clone(),
                )
            })
            .collect::<Vec<_>>();
        ids.sort();
        ids.truncate(limit);
        Ok(ids
            .into_iter()
            .filter_map(|(_, _, _, _, id)| {
                let delivery = deliveries.get_mut(&id)?;
                delivery.state = super::WakeDeliveryState::Enqueuing;
                delivery.claim_token = Some(uuid::Uuid::new_v4().to_string());
                delivery.attempts = delivery.attempts.saturating_add(1);
                delivery.first_attempt_ms.get_or_insert(now);
                delivery.next_attempt_at_ms =
                    now.saturating_add(self.wake_delivery_config.enqueuing_stale_after_ms);
                Some(delivery.clone())
            })
            .collect())
    }

    async fn list_wake_deliveries(
        &self,
        state: Option<super::WakeDeliveryState>,
    ) -> Result<Vec<super::WakeDelivery>, PluginError> {
        let _transaction = self.transaction.lock().await;
        let mut deliveries = self
            .wake_deliveries
            .lock()
            .await
            .values()
            .filter(|delivery| state.is_none_or(|state| delivery.state == state))
            .cloned()
            .collect::<Vec<_>>();
        deliveries.sort_by(|left, right| left.delivery_id.cmp(&right.delivery_id));
        Ok(deliveries)
    }

    async fn wake_delivery_report(&self) -> Result<super::WakeDeliveryReport, PluginError> {
        let _transaction = self.transaction.lock().await;
        let deliveries = self.wake_deliveries.lock().await;
        Ok(super::WakeDeliveryReport::from_deliveries(
            deliveries.values(),
        ))
    }

    async fn mark_wake_enqueued(
        &self,
        delivery_id: &str,
        claim_token: &str,
    ) -> Result<super::WakeDeliveryClaimOutcome, PluginError> {
        let pause = self.wake_mark_pause.lock_recover().take();
        if let Some(pause) = pause {
            pause.validated.notify_one();
            pause.resume.notified().await;
        }
        let _transaction = self.transaction.lock().await;
        let mut deliveries = self.wake_deliveries.lock().await;
        let delivery = deliveries.get_mut(delivery_id).ok_or_else(|| {
            PluginError::Session(format!("unknown wake delivery `{delivery_id}`"))
        })?;
        if delivery.state != super::WakeDeliveryState::Enqueuing
            || delivery.claim_token.as_deref() != Some(claim_token)
        {
            return Ok(super::WakeDeliveryClaimOutcome::ClaimLost {
                state: delivery.state,
            });
        }
        delivery.state = super::WakeDeliveryState::Enqueued;
        delivery.claim_token = None;
        delivery.discard_reason = None;
        Ok(super::WakeDeliveryClaimOutcome::Applied)
    }

    async fn discard_wake_delivery(
        &self,
        delivery_id: &str,
        claim_token: &str,
        reason: super::WakeDiscardReason,
    ) -> Result<super::WakeDeliveryClaimOutcome, PluginError> {
        let _transaction = self.transaction.lock().await;
        let mut deliveries = self.wake_deliveries.lock().await;
        let delivery = deliveries.get_mut(delivery_id).ok_or_else(|| {
            PluginError::Session(format!("unknown wake delivery `{delivery_id}`"))
        })?;
        if delivery.state != super::WakeDeliveryState::Enqueuing
            || delivery.claim_token.as_deref() != Some(claim_token)
        {
            return Ok(super::WakeDeliveryClaimOutcome::ClaimLost {
                state: delivery.state,
            });
        }
        delivery.state = super::WakeDeliveryState::Discarded;
        delivery.claim_token = None;
        delivery.discard_reason = Some(reason);
        Ok(super::WakeDeliveryClaimOutcome::Applied)
    }

    async fn redrive_wake_delivery(&self, delivery_id: &str) -> Result<(), PluginError> {
        let _transaction = self.transaction.lock().await;
        let mut deliveries = self.wake_deliveries.lock().await;
        let delivery = deliveries.get_mut(delivery_id).ok_or_else(|| {
            PluginError::Session(format!("unknown wake delivery `{delivery_id}`"))
        })?;
        if delivery.state != super::WakeDeliveryState::Discarded {
            return Err(PluginError::Session(format!(
                "wake delivery `{delivery_id}` is not discarded"
            )));
        }
        delivery.state = super::WakeDeliveryState::Pending;
        delivery.claim_token = None;
        delivery.attempts = 0;
        delivery.first_attempt_ms = None;
        delivery.next_attempt_at_ms = self.clock.timestamp_ms();
        delivery.expires_at_ms = self
            .clock
            .timestamp_ms()
            .saturating_add(self.wake_delivery_config.delivery_expiry_ms);
        delivery.discard_reason = None;
        Ok(())
    }

    async fn defer_wake_delivery(
        &self,
        delivery_id: &str,
        claim_token: &str,
        next_attempt_at_ms: u64,
    ) -> Result<super::WakeDeliveryClaimOutcome, PluginError> {
        let _transaction = self.transaction.lock().await;
        let mut deliveries = self.wake_deliveries.lock().await;
        let delivery = deliveries.get_mut(delivery_id).ok_or_else(|| {
            PluginError::Session(format!("unknown wake delivery `{delivery_id}`"))
        })?;
        if delivery.state != super::WakeDeliveryState::Enqueuing
            || delivery.claim_token.as_deref() != Some(claim_token)
        {
            return Ok(super::WakeDeliveryClaimOutcome::ClaimLost {
                state: delivery.state,
            });
        }
        delivery.state = super::WakeDeliveryState::Pending;
        delivery.claim_token = None;
        delivery.next_attempt_at_ms = next_attempt_at_ms;
        Ok(super::WakeDeliveryClaimOutcome::Applied)
    }

    async fn list_non_terminal_page(
        &self,
        limit: std::num::NonZeroUsize,
        continuation: Option<super::ProcessWorklistCursor>,
    ) -> Result<super::ProcessWorklistPage, PluginError> {
        worklist::list_non_terminal_page(self, limit, continuation).await
    }

    async fn live_reference_summary(
        &self,
    ) -> Result<Vec<ProcessLiveReferenceSummary>, PluginError> {
        let managed = self.managed.lock().await;
        Ok(ProcessLiveReferenceSummary::from_records(
            managed.values().map(|record| &record.record),
        ))
    }

    async fn claim_process_lease(
        &self,
        process_id: &str,
        owner: &crate::LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLeaseClaimOutcome, PluginError> {
        if let Some(error) = self.process_lease_claim_error.lock().await.clone() {
            return Err(error);
        }
        let _transaction = self.transaction.lock().await;
        // A lease is authority over a retained row: the SQL backends gate every
        // lease write on `require_process_conn`; same guard here (FIG-953).
        if !self.managed.lock().await.contains_key(process_id) {
            return Err(self.process_miss(process_id).await);
        }
        let mut leases = self.leases.lock().await;
        let now = self.clock.timestamp_ms();
        // The same pure tables the durable backends consult (this double's
        // hand-rolled copy drifted once — FIG-953). An empty-token row is a
        // retained fence, not an observable lease, per `ProcessLeaseRow::project`.
        let observed = leases
            .get(process_id)
            .filter(|current| !current.lease_token.is_empty())
            .cloned();
        match registry_transitions::decide_process_lease_claim(
            observed.as_ref(),
            owner,
            now,
            lease_ttl_ms,
        ) {
            registry_transitions::ProcessLeaseClaimDecision::ExtendHeldLease { lease } => {
                leases.insert(process_id.to_string(), lease.clone());
                Ok(ProcessLeaseClaimOutcome::Acquired(lease))
            }
            registry_transitions::ProcessLeaseClaimDecision::ReportBusy { holder } => {
                Ok(ProcessLeaseClaimOutcome::Busy { holder })
            }
            registry_transitions::ProcessLeaseClaimDecision::AcquireOnRetainedFence => {
                // A released lease retains its fencing token for its successor.
                let fencing_token = registry_transitions::next_process_lease_fencing_token(
                    leases
                        .get(process_id)
                        .map_or(0, |current| current.fencing_token),
                )?;
                let lease = registry_transitions::acquired_process_lease(
                    process_id,
                    owner,
                    fencing_token,
                    now,
                    lease_ttl_ms,
                );
                leases.insert(process_id.to_string(), lease.clone());
                Ok(ProcessLeaseClaimOutcome::Acquired(lease))
            }
        }
    }

    async fn reclaim_process_lease(
        &self,
        process_id: &str,
        owner: &crate::LeaseOwnerIdentity,
        observed_holder: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLeaseClaimOutcome, PluginError> {
        let _transaction = self.transaction.lock().await;
        let mut leases = self.leases.lock().await;
        let now = self.clock.timestamp_ms();
        let observed = leases
            .get(process_id)
            .filter(|current| !current.lease_token.is_empty())
            .cloned();
        let _ = observed_holder;
        let fencing_token =
            match registry_transitions::decide_process_lease_reclaim(observed.as_ref(), now)? {
                registry_transitions::ProcessLeaseReclaimDecision::ReportBusy { holder } => {
                    return Ok(ProcessLeaseClaimOutcome::Busy { holder });
                }
                registry_transitions::ProcessLeaseReclaimDecision::AcquireOnRetainedFence => {
                    registry_transitions::next_process_lease_fencing_token(
                        leases
                            .get(process_id)
                            .map_or(0, |current| current.fencing_token),
                    )?
                }
                registry_transitions::ProcessLeaseReclaimDecision::AcquireOnObservedFence {
                    fencing_token,
                } => fencing_token,
            };
        let lease = registry_transitions::acquired_process_lease(
            process_id,
            owner,
            fencing_token,
            now,
            lease_ttl_ms,
        );
        leases.insert(process_id.to_string(), lease.clone());
        Ok(ProcessLeaseClaimOutcome::Acquired(lease))
    }

    async fn renew_process_lease(
        &self,
        lease: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLease, PluginError> {
        if let Some(error) = self.process_lease_renew_error.lock().await.clone() {
            return Err(error);
        }
        let mut leases = self.leases.lock().await;
        let now = self.clock.timestamp_ms();
        let live = leases.get(&lease.process_id).filter(|current| {
            !current.lease_token.is_empty()
                && current.owner.same_incarnation(&lease.owner)
                && current.lease_token == lease.lease_token
                && current.fencing_token == lease.fencing_token
                && current.expires_at_epoch_ms > now
        });
        if live.is_none() {
            return Err(process_lease_expired(&lease.process_id));
        }
        let renewed = ProcessLease {
            expires_at_epoch_ms: now.saturating_add(lease_ttl_ms),
            ..lease.clone()
        };
        leases.insert(lease.process_id.clone(), renewed.clone());
        Ok(renewed)
    }

    async fn get_process_lease(
        &self,
        process_id: &str,
    ) -> Result<Option<ProcessLease>, PluginError> {
        Ok(self
            .leases
            .lock()
            .await
            .get(process_id)
            .filter(|lease| !lease.lease_token.is_empty())
            .cloned())
    }

    async fn complete_process_lease(
        &self,
        completion: &ProcessLeaseCompletion,
    ) -> Result<(), PluginError> {
        if let Some(error) = self.process_lease_release_error.lock().await.clone() {
            return Err(error);
        }
        let mut leases = self.leases.lock().await;
        // Release (don't drop) the lease, fenced by the completion token, so a
        // stale completion cannot release a newer owner's lease and the
        // `fencing_token` is preserved for the next claim.
        if let Some(current) = leases.get_mut(&completion.process_id)
            && current.lease_token == completion.lease_token
        {
            current.owner = crate::LeaseOwnerIdentity::opaque("", "");
            current.lease_token = String::new();
            current.claimed_at_epoch_ms = 0;
            current.expires_at_epoch_ms = 0;
        }
        Ok(())
    }

    async fn prune_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: ProjectionWatermark,
    ) -> Result<ProcessPruneReport, PluginError> {
        let _transaction = self.transaction.lock().await;
        let max_change_seq = match watermark {
            ProjectionWatermark::UpTo(cursor) => Some(cursor.store_sequence()),
            ProjectionWatermark::NoProjector => None,
        };
        let processes_with_pending_deliveries = self
            .wake_deliveries
            .lock()
            .await
            .values()
            .filter(|delivery| {
                matches!(
                    delivery.state,
                    super::WakeDeliveryState::Pending | super::WakeDeliveryState::Enqueuing
                )
            })
            .map(|delivery| delivery.wake.process_id.clone())
            .collect::<HashSet<_>>();
        let mut pruned_events = 0;
        let prunable: HashSet<String> = {
            let mut managed = self.managed.lock().await;
            let prunable: Vec<String> = managed
                .iter()
                .filter(|(_, record)| {
                    record.record.is_terminal() && record.record.updated_at_ms < cutoff_epoch_ms
                })
                .filter(|(_, record)| {
                    filter
                        .as_ref()
                        .is_none_or(|filter| filter.matches_record(&record.record))
                })
                .filter(|(_, record)| max_change_seq.is_none_or(|max| record.change_seq <= max))
                .filter(|(id, _)| !processes_with_pending_deliveries.contains(*id))
                .map(|(id, _)| id.clone())
                .collect();
            let pruned_at_ms = self.clock.timestamp_ms();
            for id in &prunable {
                if let Some(record) = managed.remove(id) {
                    pruned_events += record.events.len();
                    let pruned_change_seq = self.next_change_seq().await;
                    self.tombstones.lock().await.insert(
                        id.clone(),
                        ProcessTombstone {
                            process_id: id.clone(),
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

#[cfg(test)]
mod atomic_execution_write_tests;
