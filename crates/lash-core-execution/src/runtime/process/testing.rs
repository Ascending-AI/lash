#![cfg(any(test, feature = "testing"))]
// Test-support module: these fixtures run inside a test, and a broken setup
// assumption must abort it loudly rather than be reshaped into a runtime error
// the test under way would then report as a runtime defect. Clippy's
// `allow-expect-in-tests` reaches `#[test]` functions only, not the fixtures
// they call.
#![expect(
    clippy::expect_used,
    reason = "test-support fixtures: a broken setup assumption aborts the test"
)]

use lash_sansio::sync::MutexExt;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::plugin::PluginError;

use super::events::{
    ProcessAwaitOutput, ProcessCompletionAuthority, ProcessEvent, ProcessEventAppendReceipt,
    ProcessEventAppendRequest, terminal_append_request,
};
use super::model::{
    AbandonRequest, ProcessChange, ProcessChangeCursor, ProcessCompletionOutcome,
    ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessId, ProcessIncarnation,
    ProcessLease, ProcessLeaseClaimOutcome, ProcessLeaseCompletion, ProcessListFilter,
    ProcessObserverBy, ProcessRecord, ProcessRegistration, ProcessSessionDeleteReport,
    ProcessStartOutcome, ProcessStarted, ProcessTombstone, SessionId, WaitState,
};
use super::references::ProcessLiveReferenceView;
use super::registry::{ProcessPruneReport, ProjectionWatermark};
use super::registry_transitions;
use super::validation::{
    ProcessStartPlan, ProcessTransition, ProcessTransitionPlan, prepare_process_event_append,
    prepare_process_registration, prepare_process_start, prepare_process_transition,
};

mod continuation;
mod effect_summary_faults;
mod event_log;
#[cfg(test)]
mod identity;
mod leases;
mod lifecycle;
mod local_helpers;
#[path = "testing/parent_end.rs"]
mod parent_end;
#[path = "testing/parent_end_fault.rs"]
mod parent_end_fault;
mod raw_state;
mod registration_refusals;
mod retention;
mod support;
mod types;
mod worklist;
pub use effect_summary_faults::EffectSummaryAppendFaults;
use local_helpers::{insert_process, next_change_seq, process_miss};
pub use parent_end_fault::fail_parent_end_once;
pub use registration_refusals::{
    REFUSAL_FIXTURE_PROCESS_ID as PROCESS_REFUSAL_FIXTURE_PROCESS_ID,
    accepted_process_registration, refused_process_registrations,
};
pub use support::TestProcessRegistryWriteExt;
use support::{ExecutionWritePause, process_lease_expired, validate_in_memory_execution_authority};
use types::{ManagedLeaseMap, ManagedProcessMap, ManagedProcessRecord, RegistryState};
pub use types::{RawProcessRegistryStateForTesting, TestLocalProcessRegistry};

/// A validated event append: the plan plus the scheduling coordinates the
/// apply step needs. Splitting plan from apply lets `complete_process_with_lease`
/// run its lease check between them without duplicating the preamble.
struct PlannedManagedEventAppend {
    prepared: super::ProcessEventAppendPlan,
    wake_session_id: Option<SessionId>,
    sequence: u64,
}

impl TestLocalProcessRegistry {
    async fn append_managed_event(
        &self,
        state: &mut RegistryState,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        let planned = self
            .plan_managed_event_append(state, process_id, request)
            .await?;
        self.apply_managed_event_append(state, process_id, planned)
            .await
    }

    async fn plan_managed_event_append(
        &self,
        state: &RegistryState,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
    ) -> Result<PlannedManagedEventAppend, PluginError> {
        let record = state
            .managed
            .get(process_id)
            .expect("event appends target a managed row");
        let replay_lookup = request
            .replay
            .as_ref()
            .and_then(|replay| record.keyed_events.get(replay.key.as_str()))
            .cloned();
        let last_sequence = record.events.last().map(|event| event.sequence);
        let wake_session_id = state.wake_targets.get(process_id).cloned();
        let sender_floor = match wake_session_id.as_ref() {
            Some(target_session_id) => state
                .wake_allocation_floors
                .get(&(target_session_id.clone(), process_id.clone()))
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
            wake_session_id.as_ref(),
        )?;
        Ok(PlannedManagedEventAppend {
            prepared,
            wake_session_id,
            sequence,
        })
    }

    async fn apply_managed_event_append(
        &self,
        state: &mut RegistryState,
        process_id: &ProcessId,
        planned: PlannedManagedEventAppend,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        match planned.prepared {
            super::ProcessEventAppendPlan::Replay {
                event,
                repair_record,
                wake_delivery,
                ..
            } => {
                self.insert_wake_delivery(state, wake_delivery.as_ref())?;
                if let Some(repaired) = repair_record {
                    let change_seq = next_change_seq(state);
                    let record = Arc::make_mut(
                        state
                            .managed
                            .get_mut(process_id)
                            .expect("event appends target a managed row"),
                    );
                    record.record = repaired;
                    record.change_seq = change_seq;
                }
                let record = state
                    .managed
                    .get(process_id)
                    .expect("event appends target a managed row");
                Ok(ProcessEventAppendReceipt {
                    last_event_sequence: record.record.last_event_sequence,
                    realization: crate::StoreRealization::Coalesced,
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
                self.insert_wake_delivery(state, wake_delivery.as_ref())?;
                Self::advance_wake_allocation_floor(
                    state,
                    planned.wake_session_id.as_ref(),
                    process_id,
                    planned.sequence,
                );
                self.pause_append_after_outbox().await;
                let change_seq = next_change_seq(state);
                let record = Arc::make_mut(
                    state
                        .managed
                        .get_mut(process_id)
                        .expect("event appends target a managed row"),
                );
                record.record = projected_record;
                record.change_seq = change_seq;
                record.events.push(event.clone());
                if let Some(replay) = event.invocation.replay.clone() {
                    record.keyed_events.insert(replay.key, event.clone());
                }
                Ok(ProcessEventAppendReceipt {
                    last_event_sequence: event.sequence,
                    realization: crate::StoreRealization::Realized,
                    event,
                    wake_delivery,
                })
            }
        }
    }

    fn insert_wake_delivery(
        &self,
        state: &mut RegistryState,
        wake: Option<&super::ProcessWakeDelivery>,
    ) -> Result<(), PluginError> {
        let Some(wake) = wake else {
            return Ok(());
        };
        let delivery = super::WakeDelivery::pending(wake.clone(), self.wake_delivery_config)?;
        state
            .wake_deliveries
            .entry(delivery.delivery_id.clone())
            .or_insert(delivery);
        Ok(())
    }

    fn advance_wake_allocation_floor(
        state: &mut RegistryState,
        target_session_id: Option<&SessionId>,
        process_id: &ProcessId,
        sequence: u64,
    ) {
        let Some(target_session_id) = target_session_id else {
            return;
        };
        state
            .wake_allocation_floors
            .insert((target_session_id.clone(), process_id.clone()), sequence);
    }
}

#[async_trait::async_trait]
impl super::registry::ProcessQuery for TestLocalProcessRegistry {
    async fn get_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<ProcessRecord>, PluginError> {
        if let Some(reason) = crate::store::process_key::invalid_process_key_reason(process_id) {
            return Err(PluginError::Session(reason.into()));
        }
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
        let state = self.state.lock().await;
        if let Some(record) = state.managed.get(process_id) {
            return Ok(Some(record.record.clone()));
        }
        if state
            .tombstones
            .keys()
            .any(|(tombstoned_process_id, _)| tombstoned_process_id == process_id)
        {
            return Err(process_miss(&state, process_id));
        }
        Ok(None)
    }

    async fn list_processes(
        &self,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        let state = self.state.lock().await;
        let mut records = state
            .managed
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
        let state = self.state.lock().await;
        let horizon = state.tombstone_compaction_horizon;
        if cursor.store_sequence() < horizon {
            return Err(PluginError::ProcessChangeCursorPruned {
                requested_cursor: cursor,
                tombstone_compaction_horizon: ProcessChangeCursor::from_store_sequence(horizon),
            });
        }
        if limit == 0 {
            return Ok((Vec::new(), cursor));
        }
        let mut rows = state
            .managed
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
        rows.extend(
            state
                .tombstones
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

    async fn list_non_terminal_page(
        &self,
        limit: std::num::NonZeroUsize,
        continuation: Option<super::ProcessWorklistCursor>,
    ) -> Result<super::ProcessWorklistPage, PluginError> {
        worklist::list_non_terminal_page(self, limit, continuation).await
    }

    async fn live_reference_summary(&self) -> Result<Vec<ProcessLiveReferenceView>, PluginError> {
        let state = self.state.lock().await;
        Ok(ProcessLiveReferenceView::from_records(
            state.managed.values().map(|record| &record.record),
        ))
    }

    async fn count_non_terminal_processes(&self) -> Result<usize, PluginError> {
        let state = self.state.lock().await;
        Ok(state
            .managed
            .values()
            .filter(|record| !record.record.status.is_retired())
            .count())
    }
}

#[async_trait::async_trait]
impl super::registry::ProcessRegistrar for TestLocalProcessRegistry {
    async fn register_process_reporting_disposition(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<crate::ProcessRegistrationOutcome, PluginError> {
        let process_id = registration.id.clone();
        self.write(async |state| {
            let inserted = insert_process(self, state, registration, observers)?;
            if !inserted.is_created() {
                return Ok(inserted);
            }
            for session_id in observers {
                self.append_managed_event(
                    state,
                    &process_id,
                    ProcessEventAppendRequest::observer_added(
                        &process_id,
                        session_id,
                        &ProcessObserverBy::host("registration"),
                    ),
                )
                .await?;
            }
            let record = state
                .managed
                .get(&process_id)
                .expect("registration inserted process")
                .record
                .clone();
            // Same critical section as the insert: a fence that cannot be
            // lifted fails the registration and the staged state is dropped.
            self.scope_fence_hosts
                .reinstate_process_scope(&process_id)
                .await?;
            Ok(crate::ProcessRegistrationOutcome::created(record))
        })
        .await
    }

    fn bind_effect_host(&self, effect_host: &Arc<dyn crate::EffectHost>) {
        self.scope_fence_hosts
            .bind(effect_host, support::registry_binding(&self.state));
    }

    async fn set_external_ref(
        &self,
        process_id: &ProcessId,
        external_ref: ProcessExternalRef,
    ) -> Result<ProcessRecord, PluginError> {
        if let Some(error) = self.external_ref_write_error.lock().await.take() {
            return Err(error);
        }
        self.write(async |state| {
            let Some(record) = state.managed.get(process_id) else {
                return Err(process_miss(state, process_id));
            };
            match prepare_process_transition(
                &record.record,
                ProcessTransition::SetExternalRef(external_ref),
            )? {
                ProcessTransitionPlan::Unchanged => return Ok(record.record.clone()),
                ProcessTransitionPlan::Append(request) => {
                    self.append_managed_event(state, process_id, *request)
                        .await?;
                }
            }
            Ok(state
                .managed
                .get(process_id)
                .expect("event appends target a managed row")
                .record
                .clone())
        })
        .await
    }
}

#[async_trait::async_trait]
impl super::registry::ProcessObserverRegistry for TestLocalProcessRegistry {
    async fn add_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        self.write(async |state| {
            if !state.managed.contains_key(process_id) {
                return Err(process_miss(state, process_id));
            }
            let inserted = state
                .observers
                .entry(session_id.clone())
                .or_default()
                .insert(process_id.clone());
            if inserted {
                self.append_managed_event(
                    state,
                    process_id,
                    ProcessEventAppendRequest::observer_added(process_id, session_id, &by),
                )
                .await?;
            }
            Ok(())
        })
        .await
    }

    async fn remove_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        self.write(async |state| {
            if !state.managed.contains_key(process_id) {
                return Err(process_miss(state, process_id));
            }
            let removed = state
                .observers
                .get_mut(session_id)
                .is_some_and(|processes| processes.remove(process_id));
            if removed {
                self.append_managed_event(
                    state,
                    process_id,
                    ProcessEventAppendRequest::observer_removed(process_id, session_id, &by),
                )
                .await?;
            }
            Ok(())
        })
        .await
    }

    async fn transfer_observers(
        &self,
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: &[ProcessId],
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        self.write(async |state| {
            for process_id in process_ids {
                if !state.managed.contains_key(process_id) {
                    return Err(process_miss(state, process_id));
                }
                let removed = state
                    .observers
                    .get_mut(from_session_id)
                    .is_some_and(|processes| processes.remove(process_id));
                if !removed {
                    return Err(PluginError::Session(format!(
                        "process `{process_id}` is not observed by session `{from_session_id}`"
                    )));
                }
                state
                    .observers
                    .entry(to_session_id.clone())
                    .or_default()
                    .insert(ProcessId::from(process_id.clone().to_string()));
                self.append_managed_event(
                    state,
                    process_id,
                    ProcessEventAppendRequest::observer_removed(process_id, from_session_id, &by),
                )
                .await?;
                self.append_managed_event(
                    state,
                    process_id,
                    ProcessEventAppendRequest::observer_added(process_id, to_session_id, &by),
                )
                .await?;
            }
            Ok(())
        })
        .await
    }

    async fn list_observed_by(
        &self,
        session_id: &SessionId,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        let state = self.state.lock().await;
        let mut records = state
            .observers
            .get(session_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|process_id| state.managed.get(&process_id).map(|row| row.record.clone()))
            .filter(|record| filter.matches_record(record))
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(records)
    }

    async fn observers_for_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<Vec<SessionId>, PluginError> {
        let state = self.state.lock().await;
        if !state.managed.contains_key(process_id) {
            return Err(process_miss(&state, process_id));
        }
        let mut sessions = state
            .observers
            .iter()
            .filter(|(_, processes)| processes.contains(process_id))
            .map(|(session_id, _)| session_id.clone())
            .collect::<Vec<_>>();
        sessions.sort();
        Ok(sessions)
    }

    async fn retarget_subscription(
        &self,
        process_id: &ProcessId,
        target: Option<&str>,
    ) -> Result<(), PluginError> {
        self.write(async |state| {
            if !state.managed.contains_key(process_id) {
                return Err(process_miss(state, process_id));
            }
            let old_target = state.wake_targets.get(process_id).cloned();
            if old_target.as_deref() == target {
                return Ok(());
            }
            self.append_managed_event(
                state,
                process_id,
                ProcessEventAppendRequest::subscription_retargeted(process_id, target),
            )
            .await?;
            match target {
                Some(target) => {
                    state
                        .wake_targets
                        .insert(process_id.clone(), SessionId::from(target));
                }
                None => {
                    state.wake_targets.remove(process_id);
                }
            }
            if let Some(old_target) = old_target {
                for delivery in state.wake_deliveries.values_mut() {
                    if delivery.state() == super::WakeDeliveryState::Pending
                        && delivery.wake.process_id == process_id
                        && delivery.wake.target_session_id == old_target
                    {
                        delivery.disposition = super::WakeDeliveryDisposition::Discarded {
                            reason: super::WakeDiscardReason::Retargeted,
                        };
                    }
                }
            }
            Ok(())
        })
        .await
    }

    async fn delete_session_process_state(
        &self,
        session_id: &SessionId,
    ) -> Result<ProcessSessionDeleteReport, PluginError> {
        self.write(async |state| {
            let removed_observer_count = state
                .observers
                .remove(session_id)
                .map_or(0, |processes| processes.len());
            let cleared_processes = state
                .wake_targets
                .iter()
                .filter(|(_, target)| target.as_str() == session_id)
                .map(|(process_id, _)| process_id.clone())
                .collect::<Vec<_>>();
            state.wake_targets.retain(|_, target| target != session_id);
            state
                .wake_allocation_floors
                .retain(|(target_session_id, _), _| target_session_id != session_id);
            let mut discarded_wake_delivery_count = 0;
            for delivery in state.wake_deliveries.values_mut() {
                if delivery.state() == super::WakeDeliveryState::Pending
                    && delivery.wake.target_session_id == session_id
                {
                    delivery.disposition = super::WakeDeliveryDisposition::Discarded {
                        reason: super::WakeDiscardReason::TargetGone,
                    };
                    discarded_wake_delivery_count += 1;
                }
            }
            Ok(ProcessSessionDeleteReport {
                session_id: session_id.clone(),
                removed_observer_count,
                discarded_wake_delivery_count,
                cleared_subscription_count: cleared_processes.len(),
            })
        })
        .await
    }
}

#[async_trait::async_trait]
impl super::registry::ProcessToolIntents for TestLocalProcessRegistry {
    async fn admit_tool_intent_submission(
        &self,
        submission: crate::ToolIntentSubmissionRecord,
    ) -> Result<crate::ToolIntentSubmissionAdmission, PluginError> {
        self.write(async |state| {
            let replay_key = submission.identity.replay_key.clone();
            if let Some(existing) = state.tool_intent_submissions.get(&replay_key) {
                return Ok(crate::ToolIntentSubmissionAdmission::Existing(Box::new(
                    existing.clone(),
                )));
            }
            state.tool_intent_submissions.insert(replay_key, submission);
            Ok(crate::ToolIntentSubmissionAdmission::Admitted)
        })
        .await
    }

    async fn complete_tool_intent_submission(
        &self,
        replay_key: &str,
        outcome: crate::ToolIntentExecutionOutcome,
    ) -> Result<crate::ToolIntentSubmissionRecord, PluginError> {
        self.write(async |state| {
            let submission = state
                .tool_intent_submissions
                .get_mut(replay_key)
                .ok_or_else(|| {
                    PluginError::Session(format!("unknown tool-intent submission `{replay_key}`"))
                })?;
            if submission.outcome.is_none() {
                submission.outcome = Some(outcome);
            }
            Ok(submission.clone())
        })
        .await
    }
}

#[async_trait::async_trait]
impl super::registry::ProcessWakeOutbox for TestLocalProcessRegistry {
    fn wake_delivery_config(&self) -> super::WakeDeliveryConfig {
        self.wake_delivery_config
    }

    async fn claim_pending_wake_deliveries(
        &self,
        limit: usize,
    ) -> Result<Vec<super::WakeDelivery>, PluginError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.write(async |state| {
            let now = self.clock.timestamp_ms();
            let deliveries = &mut state.wake_deliveries;
            for delivery in deliveries.values_mut() {
                if delivery.state() == super::WakeDeliveryState::Enqueuing
                    && delivery.next_attempt_at_ms <= now
                {
                    delivery.disposition = super::WakeDeliveryDisposition::Pending;
                }
            }
            let mut ids = deliveries
                .values()
                .filter(|delivery| delivery.state() == super::WakeDeliveryState::Pending)
                .filter(|delivery| delivery.next_attempt_at_ms <= now)
                .filter(|candidate| {
                    !deliveries.values().any(|earlier| {
                        let discarded_non_blocking = match &earlier.disposition {
                            super::WakeDeliveryDisposition::Discarded { reason } => {
                                !reason.blocks_ordering_group()
                            }
                            super::WakeDeliveryDisposition::DiscardedUnattributed => true,
                            _ => false,
                        };
                        earlier.state() != super::WakeDeliveryState::Enqueued
                            && !discarded_non_blocking
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
                    delivery.disposition = super::WakeDeliveryDisposition::Enqueuing {
                        claim_token: uuid::Uuid::new_v4().to_string(),
                    };
                    delivery.attempts = delivery.attempts.saturating_add(1);
                    delivery.first_attempt_ms.get_or_insert(now);
                    delivery.next_attempt_at_ms =
                        now.saturating_add(self.wake_delivery_config.enqueuing_stale_after_ms);
                    Some(delivery.clone())
                })
                .collect())
        })
        .await
    }

    async fn list_wake_deliveries(
        &self,
        state: Option<super::WakeDeliveryState>,
    ) -> Result<Vec<super::WakeDelivery>, PluginError> {
        let registry_state = self.state.lock().await;
        let mut deliveries = registry_state
            .wake_deliveries
            .values()
            .filter(|delivery| state.is_none_or(|state| delivery.state() == state))
            .cloned()
            .collect::<Vec<_>>();
        deliveries.sort_by(|left, right| left.delivery_id.cmp(&right.delivery_id));
        Ok(deliveries)
    }

    async fn wake_delivery_report(&self) -> Result<super::WakeDeliveryReport, PluginError> {
        let state = self.state.lock().await;
        Ok(super::WakeDeliveryReport::from_deliveries(
            state.wake_deliveries.values(),
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
        self.write(async |state| {
            let delivery = state.wake_deliveries.get_mut(delivery_id).ok_or_else(|| {
                PluginError::Session(format!("unknown wake delivery `{delivery_id}`"))
            })?;
            if !matches!(
                &delivery.disposition,
                super::WakeDeliveryDisposition::Enqueuing {
                    claim_token: current
                } if current == claim_token
            ) {
                return Ok(super::WakeDeliveryClaimOutcome::ClaimLost {
                    state: delivery.state(),
                });
            }
            delivery.disposition = super::WakeDeliveryDisposition::Enqueued;
            Ok(super::WakeDeliveryClaimOutcome::Applied)
        })
        .await
    }

    async fn discard_wake_delivery(
        &self,
        delivery_id: &str,
        claim_token: &str,
        reason: super::WakeDiscardReason,
    ) -> Result<super::WakeDeliveryClaimOutcome, PluginError> {
        self.write(async |state| {
            let delivery = state.wake_deliveries.get_mut(delivery_id).ok_or_else(|| {
                PluginError::Session(format!("unknown wake delivery `{delivery_id}`"))
            })?;
            if !matches!(
                &delivery.disposition,
                super::WakeDeliveryDisposition::Enqueuing {
                    claim_token: current
                } if current == claim_token
            ) {
                return Ok(super::WakeDeliveryClaimOutcome::ClaimLost {
                    state: delivery.state(),
                });
            }
            delivery.disposition = super::WakeDeliveryDisposition::Discarded { reason };
            Ok(super::WakeDeliveryClaimOutcome::Applied)
        })
        .await
    }

    async fn redrive_wake_delivery(&self, delivery_id: &str) -> Result<(), PluginError> {
        self.write(async |state| {
            let delivery = state.wake_deliveries.get_mut(delivery_id).ok_or_else(|| {
                PluginError::Session(format!("unknown wake delivery `{delivery_id}`"))
            })?;
            if delivery.state() != super::WakeDeliveryState::Discarded {
                return Err(PluginError::Session(format!(
                    "wake delivery `{delivery_id}` is not discarded"
                )));
            }
            delivery.disposition = super::WakeDeliveryDisposition::Pending;
            delivery.attempts = 0;
            delivery.first_attempt_ms = None;
            delivery.next_attempt_at_ms = self.clock.timestamp_ms();
            delivery.expires_at_ms = self
                .clock
                .timestamp_ms()
                .saturating_add(self.wake_delivery_config.delivery_expiry_ms);
            Ok(())
        })
        .await
    }

    async fn defer_wake_delivery(
        &self,
        delivery_id: &str,
        claim_token: &str,
        next_attempt_at_ms: u64,
    ) -> Result<super::WakeDeliveryClaimOutcome, PluginError> {
        self.write(async |state| {
            let delivery = state.wake_deliveries.get_mut(delivery_id).ok_or_else(|| {
                PluginError::Session(format!("unknown wake delivery `{delivery_id}`"))
            })?;
            if !matches!(
                &delivery.disposition,
                super::WakeDeliveryDisposition::Enqueuing {
                    claim_token: current
                } if current == claim_token
            ) {
                return Ok(super::WakeDeliveryClaimOutcome::ClaimLost {
                    state: delivery.state(),
                });
            }
            delivery.disposition = super::WakeDeliveryDisposition::Pending;
            delivery.next_attempt_at_ms = next_attempt_at_ms;
            Ok(super::WakeDeliveryClaimOutcome::Applied)
        })
        .await
    }
}
impl TestLocalProcessRegistry {
    fn processes_with_pending_deliveries(state: &RegistryState) -> HashSet<ProcessId> {
        state
            .wake_deliveries
            .values()
            .filter(|delivery| {
                matches!(
                    delivery.state(),
                    super::WakeDeliveryState::Pending | super::WakeDeliveryState::Enqueuing
                )
            })
            .map(|delivery| delivery.wake.process_id.clone())
            .collect()
    }

    /// The prune eligibility predicate, shared by the survey and the prune so
    /// the two can never drift.
    fn prunable_process_ids(
        managed: &ManagedProcessMap,
        cutoff_epoch_ms: u64,
        filter: Option<&ProcessListFilter>,
        watermark: ProjectionWatermark,
        processes_with_pending_deliveries: &HashSet<ProcessId>,
    ) -> Vec<ProcessId> {
        let max_change_seq = match watermark {
            ProjectionWatermark::UpTo(cursor) => Some(cursor.store_sequence()),
            ProjectionWatermark::NoProjector => None,
        };
        let mut prunable: Vec<ProcessId> = managed
            .iter()
            .filter(|(_, record)| {
                record.record.status.is_retired() && record.record.updated_at_ms < cutoff_epoch_ms
            })
            .filter(|(_, record)| filter.is_none_or(|filter| filter.matches_record(&record.record)))
            .filter(|(_, record)| max_change_seq.is_none_or(|max| record.change_seq <= max))
            .filter(|(id, _)| !processes_with_pending_deliveries.contains(*id))
            .map(|(id, _)| id.clone())
            .collect();
        prunable.sort();
        prunable
    }
}

#[cfg(test)]
mod atomic_execution_write_tests;

#[async_trait::async_trait]
impl super::registry::ProcessRegistryTestSupport for TestLocalProcessRegistry {
    async fn wake_allocation_floor_for_testing(
        &self,
        target_session_id: &SessionId,
        process_id: &ProcessId,
    ) -> Result<Option<u64>, PluginError> {
        Ok(self
            .state
            .lock()
            .await
            .wake_allocation_floors
            .get(&(target_session_id.clone(), process_id.clone()))
            .copied())
    }
}

impl super::registry::ProcessClockRebind for TestLocalProcessRegistry {}
