#![cfg(any(test, feature = "testing"))]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::plugin::PluginError;

use super::events::{
    ProcessAwaitOutput, ProcessCompletionAuthority, ProcessEvent, ProcessEventAppendRequest,
    ProcessEventAppendResult, terminal_append_request,
};
use super::model::{
    AbandonRequest, PROCESS_LEASE_SCHEMA_VERSION, ProcessChangeCursor, ProcessCompletionOutcome,
    ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessHandleDescriptor,
    ProcessHandleGrant, ProcessHandleGrantEntry, ProcessLease, ProcessLeaseClaimOutcome,
    ProcessLeaseCompletion, ProcessListFilter, ProcessRecord, ProcessRegistration,
    ProcessSessionDeleteReport, ProcessStartOutcome, ProcessStarted, SessionScope, SessionScopeId,
    WaitState,
};
use super::references::ProcessLiveReferenceSummary;
use super::registry::{ProcessPruneReport, ProcessRegistry};
use super::time::current_epoch_ms;
use super::validation::{
    ProcessStartPlan, prepare_process_event_append, prepare_process_registration,
    prepare_process_start,
};

/// In-memory process registry for core tests.
pub struct TestLocalProcessRegistry {
    durability_tier: crate::DurabilityTier,
    managed: Arc<Mutex<ManagedProcessMap>>,
    process_read_error: Arc<Mutex<Option<PluginError>>>,
    next_change_seq: Arc<Mutex<u64>>,
    grants: Arc<Mutex<ManagedGrantMap>>,
    leases: Arc<Mutex<ManagedLeaseMap>>,
    handovers: Arc<Mutex<HashMap<(String, u64), crate::PersistedSegmentHandover>>>,
    trigger_store: Option<Arc<crate::InMemoryTriggerStore>>,
    execution_write_pause: Arc<std::sync::Mutex<Option<ExecutionWritePause>>>,
}

impl Default for TestLocalProcessRegistry {
    fn default() -> Self {
        Self {
            durability_tier: crate::DurabilityTier::Inline,
            managed: Arc::new(Mutex::new(HashMap::new())),
            process_read_error: Arc::new(Mutex::new(None)),
            next_change_seq: Arc::new(Mutex::new(0)),
            grants: Arc::new(Mutex::new(HashMap::new())),
            leases: Arc::new(Mutex::new(HashMap::new())),
            handovers: Arc::new(Mutex::new(HashMap::new())),
            trigger_store: None,
            execution_write_pause: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

type ManagedProcessMap = HashMap<String, ManagedProcessRecord>;
type ManagedGrantMap = HashMap<SessionScopeId, HashMap<String, ProcessHandleGrant>>;
type ManagedLeaseMap = HashMap<String, ProcessLease>;

#[derive(Clone)]
struct ExecutionWritePause {
    validated: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}

#[derive(Clone)]
pub struct ExecutionWritePauseHandle {
    validated: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}

impl ExecutionWritePauseHandle {
    pub async fn wait_until_validated(&self) {
        self.validated.notified().await;
    }

    pub fn resume(&self) {
        self.resume.notify_one();
    }
}

/// Explicit fixture-only conveniences for lifecycle writes whose production
/// API requires an execution authority.
#[async_trait::async_trait]
pub trait TestProcessRegistryWriteExt: ProcessRegistry {
    async fn record_first_started(
        &self,
        process_id: &str,
        started: ProcessStarted,
    ) -> Result<ProcessRecord, PluginError> {
        self.record_first_started_with_authority(
            process_id,
            started,
            &ProcessExecutionWriteAuthority::testing(process_id),
        )
        .await?
        .into_record()
    }

    async fn set_process_wait(
        &self,
        process_id: &str,
        wait: WaitState,
    ) -> Result<ProcessRecord, PluginError> {
        self.set_process_wait_with_authority(
            process_id,
            wait,
            &ProcessExecutionWriteAuthority::testing(process_id),
        )
        .await
    }

    async fn clear_process_wait(&self, process_id: &str) -> Result<ProcessRecord, PluginError> {
        self.clear_process_wait_with_authority(
            process_id,
            &ProcessExecutionWriteAuthority::testing(process_id),
        )
        .await
    }
}

impl<T> TestProcessRegistryWriteExt for T where T: ProcessRegistry + ?Sized {}

struct ManagedProcessRecord {
    record: ProcessRecord,
    change_seq: u64,
    events: Vec<ProcessEvent>,
    keyed_events: HashMap<String, (String, ProcessEvent)>,
    acked_wakes: HashSet<u64>,
}

impl TestLocalProcessRegistry {
    #[doc(hidden)]
    pub fn pause_next_execution_write_after_validation(&self) -> ExecutionWritePauseHandle {
        let pause = ExecutionWritePause {
            validated: Arc::new(tokio::sync::Notify::new()),
            resume: Arc::new(tokio::sync::Notify::new()),
        };
        *self
            .execution_write_pause
            .lock()
            .expect("execution write pause lock") = Some(pause.clone());
        ExecutionWritePauseHandle {
            validated: pause.validated,
            resume: pause.resume,
        }
    }

    async fn pause_execution_write_after_validation(&self) {
        let pause = self
            .execution_write_pause
            .lock()
            .expect("execution write pause lock")
            .take();
        if let Some(pause) = pause {
            pause.validated.notify_one();
            pause.resume.notified().await;
        }
    }

    pub async fn set_process_read_error(&self, error: Option<PluginError>) {
        *self.process_read_error.lock().await = error;
    }

    pub fn with_durability_tier(mut self, durability_tier: crate::DurabilityTier) -> Self {
        self.durability_tier = durability_tier;
        self
    }

    pub fn with_trigger_store(mut self, trigger_store: Arc<crate::InMemoryTriggerStore>) -> Self {
        self.trigger_store = Some(trigger_store);
        self
    }

    pub fn durable() -> Self {
        Self::default().with_durability_tier(crate::DurabilityTier::Durable)
    }

    async fn next_change_seq(&self) -> u64 {
        let mut next = self.next_change_seq.lock().await;
        *next = next.saturating_add(1);
        *next
    }

    async fn insert_process(
        &self,
        registration: ProcessRegistration,
    ) -> Result<ProcessRecord, PluginError> {
        let (registration, registration_hash) = prepare_process_registration(registration)?;
        let mut managed = self.managed.lock().await;
        if let Some(existing) = managed.get(&registration.id) {
            if existing.record.registration_hash == registration_hash {
                return Ok(existing.record.clone());
            }
            return Err(PluginError::Session(format!(
                "process `{}` registration hash conflict: existing {}, new {}",
                registration.id, existing.record.registration_hash, registration_hash
            )));
        }
        let id = registration.id.clone();
        let record = ProcessRecord::from_prepared_registration(
            registration,
            registration_hash,
            current_epoch_ms(),
        );
        let change_seq = self.next_change_seq().await;
        managed.insert(
            id.clone(),
            ManagedProcessRecord {
                record: record.clone(),
                change_seq,
                events: Vec::new(),
                keyed_events: HashMap::new(),
                acked_wakes: HashSet::new(),
            },
        );
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
            .map(|(hash, event)| (hash.clone(), event.clone()));
        let sequence = record.events.len() as u64 + 1;
        let prepared = prepare_process_event_append(
            &record.record,
            request,
            sequence,
            replay_lookup,
            current_epoch_ms(),
        )?;
        match prepared {
            super::ProcessEventAppendPlan::Replay {
                event,
                repair_record,
                wake_delivery,
                ..
            } => {
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
                payload_hash,
                projected_record,
                wake_delivery,
                ..
            } => {
                record.record = projected_record;
                record.change_seq = self.next_change_seq().await;
                record.events.push(event.clone());
                if let Some(replay) = event.invocation.replay.clone() {
                    record
                        .keyed_events
                        .insert(replay.key, (payload_hash, event.clone()));
                }
                Ok(ProcessEventAppendResult {
                    event,
                    wake_delivery,
                })
            }
        }
    }
}

#[async_trait::async_trait]
impl ProcessRegistry for TestLocalProcessRegistry {
    fn durability_tier(&self) -> crate::DurabilityTier {
        self.durability_tier
    }

    async fn register_process(
        &self,
        registration: ProcessRegistration,
    ) -> Result<ProcessRecord, PluginError> {
        self.insert_process(registration).await
    }

    async fn put_segment_handover(
        &self,
        process_id: &str,
        handover: crate::PersistedSegmentHandover,
    ) -> Result<(), PluginError> {
        if !self.managed.lock().await.contains_key(process_id) {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        }
        let key = (process_id.to_string(), handover.segment_ordinal);
        let mut handovers = self.handovers.lock().await;
        if let Some(existing) = handovers.get(&key) {
            if existing == &handover {
                return Ok(());
            }
            return Err(PluginError::Session(format!(
                "process `{process_id}` segment {} handover conflict",
                handover.segment_ordinal
            )));
        }
        handovers.retain(|(stored_process_id, stored_ordinal), _| {
            stored_process_id != process_id
                || *stored_ordinal >= handover.segment_ordinal.saturating_sub(1)
        });
        handovers.insert(key, handover);
        Ok(())
    }

    async fn get_segment_handover(
        &self,
        process_id: &str,
        segment_ordinal: u64,
    ) -> Result<Option<crate::PersistedSegmentHandover>, PluginError> {
        Ok(self
            .handovers
            .lock()
            .await
            .get(&(process_id.to_string(), segment_ordinal))
            .cloned())
    }

    async fn latest_segment_handover(
        &self,
        process_id: &str,
    ) -> Result<Option<crate::PersistedSegmentHandover>, PluginError> {
        Ok(self
            .handovers
            .lock()
            .await
            .iter()
            .filter(|((stored_process_id, _), _)| stored_process_id == process_id)
            .max_by_key(|((_, ordinal), _)| *ordinal)
            .map(|(_, handover)| handover.clone()))
    }

    async fn delete_segment_handovers(&self, process_id: &str) -> Result<(), PluginError> {
        self.handovers
            .lock()
            .await
            .retain(|(stored_process_id, _), _| stored_process_id != process_id);
        Ok(())
    }

    async fn set_external_ref(
        &self,
        process_id: &str,
        external_ref: ProcessExternalRef,
    ) -> Result<ProcessRecord, PluginError> {
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
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

    async fn grant_handle(
        &self,
        session_scope: &SessionScope,
        process_id: &str,
        descriptor: ProcessHandleDescriptor,
    ) -> Result<ProcessHandleGrant, PluginError> {
        if self.get_process(process_id).await.is_none() {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        }
        let grant = ProcessHandleGrant {
            session_id: session_scope.session_id.clone(),
            process_id: process_id.to_string(),
            descriptor,
        };
        self.grants
            .lock()
            .await
            .entry(session_scope.id())
            .or_default()
            .insert(process_id.to_string(), grant.clone());
        Ok(grant)
    }

    async fn revoke_handle(
        &self,
        session_scope: &SessionScope,
        process_id: &str,
    ) -> Result<(), PluginError> {
        if let Some(session_grants) = self.grants.lock().await.get_mut(&session_scope.id()) {
            session_grants.remove(process_id);
        }
        Ok(())
    }

    async fn transfer_handle_grants(
        &self,
        from_scope: &SessionScope,
        to_scope: &SessionScope,
        process_ids: &[String],
    ) -> Result<(), PluginError> {
        let mut grants = self.grants.lock().await;
        let from_scope_id = from_scope.id();
        let to_scope_id = to_scope.id();
        for process_id in process_ids {
            let grant = grants
                .get_mut(&from_scope_id)
                .and_then(|session_grants| session_grants.remove(process_id))
                .ok_or_else(|| {
                    PluginError::Session(format!(
                        "process handle `{process_id}` is not granted to session `{}`",
                        from_scope.session_id
                    ))
                })?;
            grants.entry(to_scope_id.clone()).or_default().insert(
                process_id.clone(),
                ProcessHandleGrant {
                    session_id: to_scope.session_id.clone(),
                    process_id: process_id.clone(),
                    descriptor: grant.descriptor,
                },
            );
        }
        Ok(())
    }

    async fn list_handle_grants(
        &self,
        session_scope: &SessionScope,
    ) -> Result<Vec<ProcessHandleGrantEntry>, PluginError> {
        let grants = self
            .grants
            .lock()
            .await
            .get(&session_scope.id())
            .cloned()
            .unwrap_or_default();
        let managed = self.managed.lock().await;
        let mut entries = grants
            .into_values()
            .filter_map(|grant| {
                managed
                    .get(&grant.process_id)
                    .map(|record| (grant, record.record.clone()))
            })
            .collect::<Vec<_>>();
        entries.sort_by(|(left, _), (right, _)| left.process_id.cmp(&right.process_id));
        Ok(entries)
    }

    async fn list_live_handle_grants(
        &self,
        session_scope: &SessionScope,
    ) -> Result<Vec<ProcessHandleGrantEntry>, PluginError> {
        let grants = self
            .grants
            .lock()
            .await
            .get(&session_scope.id())
            .cloned()
            .unwrap_or_default();
        let managed = self.managed.lock().await;
        let mut entries = grants
            .into_values()
            .filter_map(|grant| {
                managed
                    .get(&grant.process_id)
                    .filter(|record| !record.record.is_terminal())
                    .map(|record| (grant, record.record.clone()))
            })
            .collect::<Vec<_>>();
        entries.sort_by(|(left, _), (right, _)| left.process_id.cmp(&right.process_id));
        Ok(entries)
    }

    async fn has_handle_grant(
        &self,
        session_scope: &SessionScope,
        process_id: &str,
    ) -> Result<bool, PluginError> {
        let session_scope_id = session_scope.id();
        let granted = self
            .grants
            .lock()
            .await
            .get(&session_scope_id)
            .is_some_and(|session_grants| session_grants.contains_key(process_id));
        if !granted {
            return Ok(false);
        }
        Ok(self.managed.lock().await.contains_key(process_id))
    }

    async fn handle_grants_for_process(
        &self,
        process_id: &str,
    ) -> Result<Vec<ProcessHandleGrant>, PluginError> {
        if self.get_process(process_id).await.is_none() {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        }
        let grants = self.grants.lock().await;
        let mut entries = grants
            .values()
            .filter_map(|session_grants| session_grants.get(process_id).cloned())
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        Ok(entries)
    }

    async fn delete_session_process_state(
        &self,
        session_id: &str,
    ) -> Result<ProcessSessionDeleteReport, PluginError> {
        let removed = {
            let mut grants = self.grants.lock().await;
            let mut removed = Vec::new();
            grants.retain(|_, session_grants| {
                if session_grants
                    .values()
                    .next()
                    .is_some_and(|grant| grant.session_id == session_id)
                {
                    removed.extend(session_grants.drain().map(|(_, grant)| grant));
                    false
                } else {
                    true
                }
            });
            removed
        };
        let mut managed = self.managed.lock().await;
        let grants = self.grants.lock().await;
        let mut orphaned_process_ids = Vec::new();
        let mut preserved_process_ids = Vec::new();
        for grant in &removed {
            let Some(record) = managed.get(&grant.process_id) else {
                continue;
            };
            if record.record.is_terminal() {
                continue;
            }
            let still_granted = grants
                .values()
                .any(|session_grants| session_grants.contains_key(&grant.process_id));
            if still_granted {
                preserved_process_ids.push(grant.process_id.clone());
            } else {
                orphaned_process_ids.push(grant.process_id.clone());
            }
        }
        for record in managed.values_mut() {
            if record.record.clear_wake_target_for_session(session_id) {
                record.change_seq = self.next_change_seq().await;
            }
        }
        orphaned_process_ids.sort();
        orphaned_process_ids.dedup();
        preserved_process_ids.sort();
        preserved_process_ids.dedup();
        Ok(ProcessSessionDeleteReport {
            session_id: session_id.to_string(),
            revoked_handle_count: removed.len(),
            deleted_wake_count: 0,
            orphaned_process_ids,
            preserved_process_ids,
        })
    }

    async fn append_event(
        &self,
        process_id: &str,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendResult, PluginError> {
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        };
        self.append_managed_event(record, request).await
    }

    async fn append_event_with_authority(
        &self,
        process_id: &str,
        request: ProcessEventAppendRequest,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessEventAppendResult, PluginError> {
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        };
        let leases = self.leases.lock().await;
        validate_in_memory_execution_authority(
            &leases,
            process_id,
            &record.record,
            authority,
            None,
            current_epoch_ms(),
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
        let managed = self.managed.lock().await;
        let Some(record) = managed.get(process_id) else {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        };
        Ok(record
            .events
            .iter()
            .filter(|event| event.sequence > after_sequence)
            .cloned()
            .collect())
    }

    async fn wake_events_after(
        &self,
        process_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        let managed = self.managed.lock().await;
        let Some(record) = managed.get(process_id) else {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        };
        Ok(record
            .events
            .iter()
            .filter(|event| event.sequence > after_sequence)
            .filter(|event| event.semantics.wake.is_some())
            .filter(|event| !record.acked_wakes.contains(&event.sequence))
            .cloned()
            .collect())
    }

    async fn complete_process(
        &self,
        process_id: &str,
        await_output: ProcessAwaitOutput,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        // Hold the `managed` lock across load→validate→append so no other
        // completion can complete, prune, and re-register the row with a
        // different disposition between the validation and the terminal append.
        // The row we validate is the row we append to.
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
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
            .map(|(hash, event)| (hash.clone(), event.clone()));
        let sequence = record.events.len() as u64 + 1;
        let prepared = prepare_process_event_append(
            &record.record,
            request,
            sequence,
            replay_lookup,
            current_epoch_ms(),
        )?;
        let outcome = match prepared {
            super::ProcessEventAppendPlan::Replay { repair_record, .. } => {
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
                payload_hash,
                projected_record,
                ..
            } => {
                record.record = projected_record;
                record.change_seq = self.next_change_seq().await;
                if let Some(replay) = event.invocation.replay.clone() {
                    record
                        .keyed_events
                        .insert(replay.key, (payload_hash, event.clone()));
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
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(&lease.process_id) else {
            return Err(PluginError::Session(format!(
                "unknown process `{}`",
                lease.process_id
            )));
        };
        if record.record.is_terminal() {
            return Ok(ProcessCompletionOutcome::from_stored(
                record.record.clone(),
                &await_output,
            ));
        }
        let now = current_epoch_ms();
        let request = terminal_append_request(&lease.process_id, &await_output, None);
        let replay_lookup = request
            .replay
            .as_ref()
            .and_then(|replay| record.keyed_events.get(replay.key.as_str()))
            .map(|(hash, event)| (hash.clone(), event.clone()));
        let sequence = record.events.len() as u64 + 1;
        let prepared =
            prepare_process_event_append(&record.record, request, sequence, replay_lookup, now)?;
        if matches!(prepared, super::ProcessEventAppendPlan::Replay { .. }) {
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
                payload_hash,
                projected_record,
                ..
            } => {
                record.record = projected_record;
                if let Some(replay) = event.invocation.replay.clone() {
                    record
                        .keyed_events
                        .insert(replay.key, (payload_hash, event.clone()));
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
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        };
        let leases = self.leases.lock().await;
        validate_in_memory_execution_authority(
            &leases,
            process_id,
            &record.record,
            authority,
            Some(&started),
            current_epoch_ms(),
        )?;
        match prepare_process_start(&record.record, &started)? {
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
        let request = ProcessEventAppendRequest::first_started(process_id, &started);
        self.append_managed_event(record, request).await?;
        drop(leases);
        Ok(ProcessStartOutcome::Started(record.record.clone()))
    }

    async fn request_process_abandon(
        &self,
        process_id: &str,
        request: AbandonRequest,
    ) -> Result<ProcessRecord, PluginError> {
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
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
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        };
        let leases = self.leases.lock().await;
        validate_in_memory_execution_authority(
            &leases,
            process_id,
            &record.record,
            authority,
            None,
            current_epoch_ms(),
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
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        };
        let leases = self.leases.lock().await;
        validate_in_memory_execution_authority(
            &leases,
            process_id,
            &record.record,
            authority,
            None,
            current_epoch_ms(),
        )?;
        let Some(wait) = record.record.wait.clone() else {
            return Ok(record.record.clone());
        };
        let request = ProcessEventAppendRequest::wait_cleared(process_id, &wait);
        self.append_managed_event(record, request).await?;
        drop(leases);
        Ok(record.record.clone())
    }

    async fn get_process(&self, process_id: &str) -> Option<ProcessRecord> {
        self.try_get_process(process_id).await.ok().flatten()
    }

    async fn try_get_process(
        &self,
        process_id: &str,
    ) -> Result<Option<ProcessRecord>, PluginError> {
        if let Some(error) = self.process_read_error.lock().await.clone() {
            return Err(error);
        }
        let managed = self.managed.lock().await;
        Ok(managed.get(process_id).map(|record| record.record.clone()))
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
    ) -> Result<(Vec<ProcessRecord>, ProcessChangeCursor), PluginError> {
        if limit == 0 {
            return Ok((Vec::new(), cursor));
        }
        let managed = self.managed.lock().await;
        let mut rows = managed
            .values()
            .filter(|record| record.change_seq > cursor.store_sequence())
            .map(|record| (record.change_seq, record.record.clone()))
            .collect::<Vec<_>>();
        rows.sort_by(|(left_seq, left), (right_seq, right)| {
            left_seq.cmp(right_seq).then_with(|| left.id.cmp(&right.id))
        });
        rows.truncate(limit);
        let next_cursor = rows
            .last()
            .map(|(change_seq, _)| ProcessChangeCursor::from_store_sequence(*change_seq))
            .unwrap_or(cursor);
        Ok((
            rows.into_iter().map(|(_, record)| record).collect(),
            next_cursor,
        ))
    }

    async fn ack_wake(&self, process_id: &str, sequence: u64) -> Result<(), PluginError> {
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        };
        record.acked_wakes.insert(sequence);
        Ok(())
    }

    async fn list_non_terminal(&self) -> Result<Vec<ProcessRecord>, PluginError> {
        let managed = self.managed.lock().await;
        let mut records: Vec<ProcessRecord> = managed
            .values()
            .filter(|record| !record.record.is_terminal())
            .map(|record| record.record.clone())
            .collect();
        records.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(records)
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
        let mut leases = self.leases.lock().await;
        let now = current_epoch_ms();
        if let Some(current) = leases.get_mut(process_id)
            && !current.lease_token.is_empty()
            && current.expires_at_epoch_ms > now
        {
            if current.owner.same_incarnation(owner) {
                // Same incarnation re-enters its own live lease: extend the
                // expiry without changing token or fencing token.
                current.expires_at_epoch_ms = now.saturating_add(lease_ttl_ms);
                return Ok(ProcessLeaseClaimOutcome::Acquired(current.clone()));
            }
            return Ok(ProcessLeaseClaimOutcome::Busy {
                holder: current.clone(),
            });
        }
        // The fencing token increases monotonically even across completion: a
        // released lease retains its `fencing_token` so a re-claim never reuses
        // a stale writer's token (mirrors `SqliteProcessRegistry`).
        let fencing_token = leases
            .get(process_id)
            .map_or(0, |current| current.fencing_token)
            .saturating_add(1);
        let lease = acquire_test_lease(process_id, owner, fencing_token, now, lease_ttl_ms);
        leases.insert(process_id.to_string(), lease.clone());
        Ok(ProcessLeaseClaimOutcome::Acquired(lease))
    }

    async fn reclaim_process_lease(
        &self,
        process_id: &str,
        owner: &crate::LeaseOwnerIdentity,
        observed_holder: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLeaseClaimOutcome, PluginError> {
        let mut leases = self.leases.lock().await;
        let now = current_epoch_ms();
        let Some(current) = leases.get(process_id) else {
            let lease = acquire_test_lease(process_id, owner, 1, now, lease_ttl_ms);
            leases.insert(process_id.to_string(), lease.clone());
            return Ok(ProcessLeaseClaimOutcome::Acquired(lease));
        };
        if current.lease_token.is_empty() || current.expires_at_epoch_ms <= now {
            let lease = acquire_test_lease(
                process_id,
                owner,
                current.fencing_token.saturating_add(1),
                now,
                lease_ttl_ms,
            );
            leases.insert(process_id.to_string(), lease.clone());
            return Ok(ProcessLeaseClaimOutcome::Acquired(lease));
        }
        // Fenced CAS on the observed holder: identity, token, and fencing token
        // must all still match, and the holder must be definitely dead for this
        // claimant, else the live lease stays untouched.
        if observed_holder.process_id == process_id
            && current.owner.same_incarnation(&observed_holder.owner)
            && current.lease_token == observed_holder.lease_token
            && current.fencing_token == observed_holder.fencing_token
            && current.owner.is_definitely_dead_for_claimant(owner)
        {
            let lease = acquire_test_lease(
                process_id,
                owner,
                current.fencing_token.saturating_add(1),
                now,
                lease_ttl_ms,
            );
            leases.insert(process_id.to_string(), lease.clone());
            return Ok(ProcessLeaseClaimOutcome::Acquired(lease));
        }
        Ok(ProcessLeaseClaimOutcome::Busy {
            holder: current.clone(),
        })
    }

    async fn renew_process_lease(
        &self,
        lease: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLease, PluginError> {
        let mut leases = self.leases.lock().await;
        let now = current_epoch_ms();
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
        up_to_change_seq: Option<ProcessChangeCursor>,
    ) -> Result<ProcessPruneReport, PluginError> {
        let max_change_seq = up_to_change_seq.map(ProcessChangeCursor::store_sequence);
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
                .map(|(id, _)| id.clone())
                .collect();
            for id in &prunable {
                if let Some(record) = managed.remove(id) {
                    pruned_events += record.events.len();
                }
            }
            prunable.into_iter().collect()
        };
        {
            let mut grants = self.grants.lock().await;
            for session_grants in grants.values_mut() {
                session_grants.retain(|process_id, _| !prunable.contains(process_id));
            }
            grants.retain(|_, session_grants| !session_grants.is_empty());
        }
        self.leases
            .lock()
            .await
            .retain(|process_id, _| !prunable.contains(process_id));
        if let Some(trigger_store) = self.trigger_store.as_ref() {
            trigger_store.delete_deliveries_by_process_ids(&prunable)?;
            crate::TriggerStore::prune_mutation_receipts(trigger_store.as_ref(), cutoff_epoch_ms)
                .await?;
        }
        Ok(ProcessPruneReport {
            pruned_processes: prunable.len(),
            pruned_events,
        })
    }
}

fn acquire_test_lease(
    process_id: &str,
    owner: &crate::LeaseOwnerIdentity,
    fencing_token: u64,
    now: u64,
    lease_ttl_ms: u64,
) -> ProcessLease {
    ProcessLease {
        schema_version: PROCESS_LEASE_SCHEMA_VERSION,
        process_id: process_id.to_string(),
        owner: owner.clone(),
        lease_token: format!(
            "{process_id}:{}:{}:{now}:{fencing_token}",
            owner.owner_id, owner.incarnation_id
        ),
        fencing_token,
        claimed_at_epoch_ms: now,
        expires_at_epoch_ms: now.saturating_add(lease_ttl_ms),
    }
}

/// Loud, stable error for a superseded or expired process lease.
fn process_lease_expired(process_id: &str) -> PluginError {
    PluginError::ProcessLeaseSuperseded {
        process_id: process_id.to_string(),
    }
}

fn validate_in_memory_execution_authority(
    leases: &ManagedLeaseMap,
    process_id: &str,
    record: &ProcessRecord,
    authority: &ProcessExecutionWriteAuthority,
    start: Option<&ProcessStarted>,
    now: u64,
) -> Result<(), PluginError> {
    match authority {
        ProcessExecutionWriteAuthority::Invocation { .. } => {
            if let Some(started) = start {
                authority.validate_invocation_for_start(process_id, started)
            } else {
                authority.validate_invocation_for_write(process_id, record)
            }
        }
        ProcessExecutionWriteAuthority::Testing { .. } => {
            if let Some(started) = start {
                authority.validate_invocation_for_start(process_id, started)
            } else {
                authority.validate_invocation_for_write(process_id, record)
            }
        }
        ProcessExecutionWriteAuthority::Lease(lease) => {
            if lease.process_id != process_id {
                return Err(process_lease_expired(process_id));
            }
            if leases.get(process_id).is_some_and(|current| {
                !current.lease_token.is_empty()
                    && current.owner.same_incarnation(&lease.owner)
                    && current.lease_token == lease.lease_token
                    && current.fencing_token == lease.fencing_token
                    && current.expires_at_epoch_ms > now
            }) {
                Ok(())
            } else {
                Err(process_lease_expired(process_id))
            }
        }
    }
}

fn process_external_ref_conflict(
    process_id: &str,
    existing: &ProcessExternalRef,
    new: &ProcessExternalRef,
) -> PluginError {
    PluginError::Session(format!(
        "process `{process_id}` external ref conflict: existing {existing:?}, new {new:?}"
    ))
}

#[cfg(test)]
mod atomic_execution_write_tests {
    use super::*;

    #[tokio::test]
    async fn claim_cannot_interleave_between_authority_validation_and_append() {
        const PROCESS_ID: &str = "atomic-authority-append";
        let registry = Arc::new(TestLocalProcessRegistry::default());
        registry
            .register_process(
                ProcessRegistration::new(
                    PROCESS_ID,
                    crate::ProcessInput::Engine {
                        kind: "test".to_string(),
                        payload: serde_json::Value::Null,
                    },
                    crate::RecoveryDisposition::Rerunnable,
                    crate::ProcessProvenance::host(),
                )
                .with_execution_env_ref(Some(crate::ProcessExecutionEnvRef::new("test-env"))),
            )
            .await
            .expect("register");
        let owner = crate::LeaseOwnerIdentity::opaque("worker-a", "incarnation-a");
        let lease = registry
            .claim_process_lease(PROCESS_ID, &owner, 60_000)
            .await
            .expect("claim")
            .acquired()
            .expect("lease");
        registry
            .record_first_started_with_authority(
                PROCESS_ID,
                ProcessStarted {
                    owner: owner.clone(),
                    fencing_token: lease.fencing_token,
                    attempt: 1,
                    started_at_ms: 1,
                },
                &ProcessExecutionWriteAuthority::lease(lease.clone()),
            )
            .await
            .expect("start");

        let pause = registry.pause_next_execution_write_after_validation();
        let writer_registry = Arc::clone(&registry);
        let writer_lease = lease.clone();
        let writer = crate::task::spawn(async move {
            writer_registry
                .append_event_with_authority(
                    PROCESS_ID,
                    ProcessEventAppendRequest::cancel_requested(PROCESS_ID, Some("race".into())),
                    &ProcessExecutionWriteAuthority::lease(writer_lease),
                )
                .await
        });
        pause.wait_until_validated().await;

        let claimant_registry = Arc::clone(&registry);
        let claimant_lease = lease.clone();
        let claimant = crate::task::spawn(async move {
            claimant_registry
                .complete_process_lease(&ProcessLeaseCompletion::from_lease(&claimant_lease))
                .await?;
            claimant_registry
                .claim_process_lease(
                    PROCESS_ID,
                    &crate::LeaseOwnerIdentity::opaque("worker-b", "incarnation-b"),
                    60_000,
                )
                .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), async {
                while !claimant.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_err(),
            "claim must remain blocked while the validated append holds the lease lock"
        );

        pause.resume();
        writer
            .await
            .expect("writer joins")
            .expect("validated writer appends");
        let claimed = claimant
            .await
            .expect("claimant joins")
            .expect("claim after append")
            .acquired()
            .expect("new lease");
        assert!(claimed.fencing_token > lease.fencing_token);
    }
}
