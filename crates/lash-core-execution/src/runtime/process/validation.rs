use crate::ProcessId;
use lash_sansio::{CancelOrigin, CancelRequest};
use std::collections::HashSet;

use crate::SessionId;
use crate::plugin::PluginError;

use super::events::{
    ProcessAwaitOutput, ProcessEvent, ProcessEventAppendRequest, ProcessEventKind,
    ProcessEventSemanticsSpec, ProcessTerminalSemantics, ProcessWakeDelivery,
    default_process_event_types, is_runtime_lifecycle_event_type, runtime_lifecycle_event_type,
};
use super::materialization::materialize_process_event_semantics;
use super::model::{
    AbandonRequest, ProcessExternalRef, ProcessRecord, ProcessRegistration, ProcessStarted,
    ProcessStatus, RecoveryContract, WaitState,
};

pub fn validate_generic_process_event_append(
    request: &ProcessEventAppendRequest,
) -> Result<(), PluginError> {
    // The effect summary is runtime-owned: only an execution-authority append
    // may write it, so a host cannot pre-empt the runtime's replay key.
    if matches!(
        ProcessEventKind::from_event_type(&request.event_type),
        ProcessEventKind::UnknownRuntime
            | ProcessEventKind::EffectOutcome
            | ProcessEventKind::EffectOmissions
    ) {
        return Err(PluginError::ReservedProcessEvent {
            event_type: request.event_type.clone(),
        });
    }
    if matches!(
        request.event_type.as_str(),
        "process.observer_added"
            | "process.observer_removed"
            | "process.subscription_retargeted"
            | "process.parked"
            | "process.park_rerun_began"
    ) {
        return Err(PluginError::ReservedProcessEvent {
            event_type: request.event_type.clone(),
        });
    }
    Ok(())
}

use super::wake::{ProcessWakeDeliveryRequest, process_wake_delivery};

#[derive(Clone, Debug)]
pub enum ProcessEventAppendPlan {
    Insert {
        event: ProcessEvent,
        projected_record: ProcessRecord,
        wake_delivery: Option<ProcessWakeDelivery>,
    },
    Replay {
        event: ProcessEvent,
        repair_record: Option<ProcessRecord>,
        wake_delivery: Option<ProcessWakeDelivery>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessStartPlan {
    Append,
    AlreadyApplied,
    AlreadyStarted { by: crate::LeaseOwnerIdentity },
    AttemptsExhausted { attempts: u32, max_attempts: u32 },
}

/// A registry-owned lifecycle transition whose append disposition is shared
/// across every process-store implementation.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessTransition {
    /// Bind the process to durable work owned by another backend.
    SetExternalRef(ProcessExternalRef),
    /// Record a request for recovery to abandon the process.
    RequestAbandon(AbandonRequest),
    /// Record a typed process cancellation request.
    RequestCancel(CancelRequest),
    /// Record that the caller of an externally-owned process departed.
    RecordCallerDeparture,
    /// Enter a durable wait state.
    EnterWait(WaitState),
    ClearWait,
    /// Park the process: its body refused to replay its journal (NOW-B). A
    /// first refusal opens the park; a rerun's refusal re-parks it, keeping
    /// `since_ms` and `park_id` and counting the attempt. A park whose latest
    /// run already refused is unchanged, so a retried write never counts one
    /// refusal twice.
    Park(crate::store::ProcessParkWrite),
    /// A rerun of a parked process began: the park stays, and stops
    /// exempting the process's starts from the attempt budget until the
    /// rerun refuses again. Unchanged on a process with no refusing park.
    BeginParkedRerun,
}

/// The store action selected for a registry-owned lifecycle transition.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessTransitionPlan {
    /// The requested transition is already reflected in the record.
    Unchanged,
    /// Append the supplied request through the normal process-event fold.
    Append(Box<ProcessEventAppendRequest>),
}

const FOLD_VALIDATION_REPLAY_KEY_SUFFIX: &str = ":fold-validation";

/// Allocate the next process-event sequence from the live event tail and the
/// durable sender floor retained for the wake target.
///
/// Event sequences are small ordered identifiers. The sender floor survives
/// process pruning, so a reused process id cannot issue a sequence already
/// observed by the same target session.
pub fn allocate_process_event_sequence(
    last_sequence: Option<u64>,
    sender_floor: Option<u64>,
) -> Result<u64, PluginError> {
    let next_event = last_sequence
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| PluginError::Session("process event sequence exhausted".to_string()))?;
    let next_floor = sender_floor
        .map(|floor| {
            floor.checked_add(1).ok_or_else(|| {
                PluginError::Session("process wake allocation floor exhausted".to_string())
            })
        })
        .transpose()?
        .unwrap_or(1);
    let sequence = next_event.max(next_floor);
    if sequence > i64::MAX as u64 {
        return Err(PluginError::Session(
            "process event sequence exceeds the signed 64-bit persistence domain".to_string(),
        ));
    }
    Ok(sequence)
}

pub fn prepare_process_start(
    record: &ProcessRecord,
    started: &ProcessStarted,
    authority: &super::model::ProcessExecutionWriteAuthority,
) -> Result<ProcessStartPlan, PluginError> {
    if record.is_terminal() {
        return Err(PluginError::Session(format!(
            "terminal process `{}` cannot start an execution attempt",
            record.id
        )));
    }
    if record.disposition == RecoveryContract::ExternallyOwned {
        return Err(PluginError::Session(format!(
            "externally-owned process `{}` cannot start an execution attempt",
            record.id
        )));
    }
    if record
        .first_started
        .as_deref()
        .is_some_and(|existing| existing.same_execution(started))
    {
        return Ok(ProcessStartPlan::AlreadyApplied);
    }
    authority.validate_resume_predecessor(&record.id, record.first_started.as_deref())?;

    let expected_attempt = match record.first_started.as_deref() {
        None => 1,
        Some(existing)
            if record.disposition == RecoveryContract::OwnerBound
                && !authority.permits_owner_bound_resume(existing) =>
        {
            return Ok(ProcessStartPlan::AlreadyStarted {
                by: existing.owner.clone(),
            });
        }
        Some(existing) => existing.attempt.saturating_add(1),
    };
    if started.attempt != expected_attempt {
        return Err(PluginError::Session(format!(
            "process `{}` execution attempt must be {}, got {}",
            record.id, expected_attempt, started.attempt
        )));
    }
    // A parked process (FIG-3586) re-runs to find out whether the build now
    // serving it can replay its journal; those runs refuse with nothing
    // dispatched, so they do not spend its attempt budget.
    let parked = record.is_refusing_park();
    if let Some(max_attempts) = record.max_attempts
        && started.attempt > max_attempts
        && !parked
    {
        return Ok(ProcessStartPlan::AttemptsExhausted {
            attempts: started.attempt.saturating_sub(1),
            max_attempts,
        });
    }
    Ok(ProcessStartPlan::Append)
}

/// Prepare one registry-owned lifecycle transition for the shared append path.
///
/// This planner recognizes only idempotent no-ops. Illegal transitions remain
/// append requests so [`apply_process_event_projection`] supplies the canonical
/// refusal. When a refused request would reuse an already-persisted lifecycle
/// replay key, the planner substitutes a deterministic validation-only key;
/// otherwise replay validation would shadow the fold's more specific error.
pub fn prepare_process_transition(
    record: &ProcessRecord,
    transition: ProcessTransition,
) -> Result<ProcessTransitionPlan, PluginError> {
    let append = match transition {
        ProcessTransition::SetExternalRef(external_ref) => {
            // Mirrors the fold's compare-and-set: a write that cannot displace
            // the recorded owner is an idempotent no-op, not an append, so a resubmitting
            // sweep never rewrites a row it coalesced onto.
            // a competing backend still reaches the fold's refusal.
            match record.external_ref.as_ref() {
                // An ended process names no successor segment: its stored
                // terminal revokes every execution that would carry it on
                // (FIG-3820). A root reference recorded after a fast
                // terminal names the carrier that ended it and still lands.
                _ if record.is_terminal()
                    && external_ref.segment_ordinal() > 0
                    && record
                        .external_ref
                        .as_ref()
                        .is_none_or(|existing| external_ref.supersedes(existing)) =>
                {
                    return Err(PluginError::ProcessAlreadyTerminal {
                        process_id: record.id.clone(),
                        status: record.status,
                    });
                }
                // Nothing recorded yet: this writer names the owner.
                None => ProcessEventAppendRequest::external_ref_set(&record.id, &external_ref),
                // A competing backend is a refusal at every ordinal, and the
                // fold is the single place that refusal is phrased.
                Some(existing) if existing.backend != external_ref.backend => {
                    let mut append =
                        ProcessEventAppendRequest::external_ref_set(&record.id, &external_ref);
                    route_transition_refusal_to_fold(&mut append)?;
                    append
                }
                // Only a strictly later segment displaces the recorded owner.
                Some(existing) if external_ref.supersedes(existing) => {
                    ProcessEventAppendRequest::external_ref_set(&record.id, &external_ref)
                }
                // Same or earlier segment on the same backend: an idempotent
                // no-op, so a resubmitting sweep never rewrites a row it
                // coalesced onto.
                Some(_) => return Ok(ProcessTransitionPlan::Unchanged),
            }
        }
        ProcessTransition::RequestCancel(request) => {
            if !record.is_terminal()
                && record
                    .cancel_request
                    .as_deref()
                    .is_some_and(|existing| existing.same_cancellation_as(&request))
            {
                return Ok(ProcessTransitionPlan::Unchanged);
            }
            let mut append = ProcessEventAppendRequest::cancel_requested(&record.id, &request);
            if record.is_terminal() || record.cancel_request.is_some() {
                route_transition_refusal_to_fold(&mut append)?;
            }
            append
        }
        ProcessTransition::RequestAbandon(request) => {
            if !record.is_terminal()
                && record
                    .abandon_request
                    .as_deref()
                    .is_some_and(|existing| abandon_requests_match(existing, &request))
            {
                return Ok(ProcessTransitionPlan::Unchanged);
            }
            let mut append = ProcessEventAppendRequest::abandon_requested(&record.id, &request);
            if record.is_terminal() || record.abandon_request.is_some() {
                route_transition_refusal_to_fold(&mut append)?;
            }
            append
        }
        ProcessTransition::RecordCallerDeparture => {
            if record.status == ProcessStatus::CallerDeparted {
                return Ok(ProcessTransitionPlan::Unchanged);
            }
            ProcessEventAppendRequest::caller_departed(&record.id)
        }
        ProcessTransition::EnterWait(wait) => {
            if record.status == ProcessStatus::Waiting && record.wait.as_ref() == Some(&wait) {
                return Ok(ProcessTransitionPlan::Unchanged);
            }
            let mut append = ProcessEventAppendRequest::wait_entered(&record.id, &wait);
            if record.is_terminal() || record.status == ProcessStatus::CallerDeparted {
                route_transition_refusal_to_fold(&mut append)?;
            }
            append
        }
        ProcessTransition::ClearWait => {
            let Some(wait) = record.wait.as_ref() else {
                return Ok(ProcessTransitionPlan::Unchanged);
            };
            ProcessEventAppendRequest::wait_cleared(&record.id, wait)
        }
        ProcessTransition::Park(park) => {
            if record.is_refusing_park() {
                return Ok(ProcessTransitionPlan::Unchanged);
            }
            let mut append =
                ProcessEventAppendRequest::parked(&record.id, &park, record.last_event_sequence);
            if record.is_terminal() || record.status == ProcessStatus::CallerDeparted {
                route_transition_refusal_to_fold(&mut append)?;
            }
            append
        }
        ProcessTransition::BeginParkedRerun => {
            let Some(park) = record.park.as_deref().filter(|park| park.refusing) else {
                return Ok(ProcessTransitionPlan::Unchanged);
            };
            ProcessEventAppendRequest::park_rerun_began(
                &record.id,
                park.park_id,
                record.last_event_sequence,
            )
        }
    };
    Ok(ProcessTransitionPlan::Append(Box::new(append)))
}

fn abandon_requests_match(existing: &AbandonRequest, requested: &AbandonRequest) -> bool {
    existing.requested_by == requested.requested_by && existing.reason == requested.reason
}

fn route_transition_refusal_to_fold(
    append: &mut ProcessEventAppendRequest,
) -> Result<(), PluginError> {
    let event_type = append.event_type.clone();
    let replay = append.replay.as_mut().ok_or_else(|| {
        PluginError::Session(format!(
            "registry lifecycle transition event `{event_type}` requires a deterministic replay key"
        ))
    })?;
    replay.key.push_str(FOLD_VALIDATION_REPLAY_KEY_SUFFIX);
    Ok(())
}

pub fn apply_process_status_projection(
    record: &mut ProcessRecord,
    status: ProcessStatus,
    updated_at_ms: u64,
) {
    record.status = status;
    if record.status.is_terminal() {
        record.wait = None;
        record.park = None;
    }
    record.updated_at_ms = updated_at_ms;
}

/// Apply the caller-departure transition to a process record fold.
///
/// The legal transitions are exactly `running -> caller_departed` and the
/// idempotent `caller_departed -> caller_departed`. Everything else is
/// refused, which is what keeps the state honest:
///
/// * only an `ExternallyOwned` row can reach it, because only a row lash never
///   executes can outlive the caller that registered it with no outcome
///   anybody could write;
/// * a terminal row can never reach it, because an outcome is already
///   recorded and departure cannot retract it;
/// * a waiting row can never reach it, because waiting is an execution state
///   an externally-owned row never enters.
pub(super) fn apply_caller_departure(record: &mut ProcessRecord) -> Result<(), PluginError> {
    if record.disposition != crate::RecoveryContract::ExternallyOwned {
        return Err(PluginError::Session(format!(
            "process `{}` is not externally-owned and cannot record a caller departure",
            record.id
        )));
    }
    match record.status {
        ProcessStatus::CallerDeparted => Ok(()),
        ProcessStatus::Running => {
            record.status = ProcessStatus::CallerDeparted;
            Ok(())
        }
        status if status.is_terminal() => Err(PluginError::Session(format!(
            "terminal process `{}` cannot record a caller departure",
            record.id
        ))),
        status => Err(PluginError::Session(format!(
            "process `{}` cannot record a caller departure from `{}`",
            record.id,
            status.label()
        ))),
    }
}

/// Apply one persisted event to the process record fold.
///
/// Callers must supply events in sequence order when rebuilding a record. The
/// append path uses this same function before inserting the event, then saves
/// the returned projection in the event-insert transaction.
pub fn apply_process_event_projection(
    record: &mut ProcessRecord,
    event: &ProcessEvent,
) -> Result<(), PluginError> {
    if event.process_id != record.id {
        return Err(PluginError::Session(format!(
            "process event for `{}` cannot project record `{}`",
            event.process_id, record.id
        )));
    }

    let kind = ProcessEventKind::from_event_type(&event.event_type);
    match kind {
        ProcessEventKind::FirstStarted => {
            let started = lifecycle_payload(event, "started")?;
            let resumed_from_handover = event
                .payload
                .get("resumed_from_handover")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            match record.first_started.as_deref() {
                None => record.first_started = Some(Box::new(started)),
                Some(existing) if existing.same_execution(&started) => {}
                Some(existing)
                    if (record.disposition == RecoveryContract::Rerunnable
                        || resumed_from_handover)
                        && started.attempt == existing.attempt.saturating_add(1) =>
                {
                    record.first_started = Some(Box::new(started));
                }
                Some(_) => {
                    return Err(PluginError::Session(format!(
                        "process `{}` has an invalid execution-started attempt",
                        record.id
                    )));
                }
            }
        }
        ProcessEventKind::Waiting => {
            if record.is_terminal() {
                return Err(PluginError::Session(format!(
                    "terminal process `{}` cannot enter a wait state",
                    record.id
                )));
            }
            if record.status == ProcessStatus::CallerDeparted {
                return Err(PluginError::Session(format!(
                    "caller-departed process `{}` cannot enter a wait state",
                    record.id
                )));
            }
            record.wait = Some(lifecycle_payload(event, "wait")?);
            record.status = ProcessStatus::Waiting;
        }
        ProcessEventKind::Resumed => {
            if record.status == ProcessStatus::CallerDeparted {
                return Err(PluginError::Session(format!(
                    "caller-departed process `{}` cannot resume",
                    record.id
                )));
            }
            record.wait = None;
            record.status = ProcessStatus::Running;
        }
        ProcessEventKind::ExternalRefSet => {
            let external_ref = lifecycle_payload(event, "external_ref")?;
            // Compare-and-set on the segment ordinal, never last-write-wins.
            // A live host and the recovery sweep may both submit the same
            // segment and mint different backend identities for it; the first
            // recorded one stays, because both run the same coalesced work.
            // Only a strictly later segment names a new owner, and a reference
            // for an earlier segment is a stale writer that must not displace
            // it.
            match record.external_ref.as_ref() {
                None => record.external_ref = Some(external_ref),
                Some(existing) if existing == &external_ref => {}
                // Two backends claiming one row is a model error at every
                // ordinal: a row has exactly one durable owner substrate, so
                // this is checked before the ordinal comparison — a later
                // segment never licenses a change of substrate.
                Some(existing) if existing.backend != external_ref.backend => {
                    return Err(process_external_ref_conflict(
                        &record.id,
                        existing,
                        &external_ref,
                    ));
                }
                Some(existing) if external_ref.supersedes(existing) => {
                    record.external_ref = Some(external_ref);
                }
                Some(_) => {}
            }
        }
        ProcessEventKind::CancelRequested => {
            let request = cancel_request_payload(&event.payload)?;
            // Replaying the stored StartFailed event must repair its own fold
            // even though that same event made this record terminal.
            let own_terminal_replay = record.status == ProcessStatus::Cancelled
                && record.last_event_sequence == event.sequence
                && request.origin == CancelOrigin::StartFailed
                && event
                    .semantics
                    .terminal
                    .as_ref()
                    .is_some_and(|terminal| terminal.status == ProcessStatus::Cancelled);
            if record.is_terminal() && !own_terminal_replay {
                return Err(PluginError::ProcessAlreadyTerminal {
                    process_id: record.id.clone(),
                    status: record.status,
                });
            }
            match record.cancel_request.as_deref() {
                None => record.cancel_request = Some(Box::new(request)),
                Some(existing) if existing.same_cancellation_as(&request) => {}
                Some(existing) => {
                    return Err(PluginError::ProcessCancelConflict {
                        process_id: record.id.clone(),
                        existing: Box::new(existing.clone()),
                        requested: Box::new(request),
                    });
                }
            }
        }
        ProcessEventKind::AbandonRequested => {
            if record.is_terminal() {
                return Err(PluginError::Session(format!(
                    "terminal process `{}` cannot accept an abandon request",
                    record.id
                )));
            }
            let request = lifecycle_payload(event, "request")?;
            match record.abandon_request.as_deref() {
                None => record.abandon_request = Some(Box::new(request)),
                Some(existing) if existing == &request => {}
                Some(_) => {
                    return Err(PluginError::Session(format!(
                        "process `{}` already has a different abandon request",
                        record.id
                    )));
                }
            }
        }
        ProcessEventKind::CallerDeparted => {
            apply_caller_departure(record)?;
        }
        ProcessEventKind::Parked => {
            if record.is_terminal() || record.status == ProcessStatus::CallerDeparted {
                return Err(PluginError::Session(format!(
                    "process `{}` cannot park from `{}`",
                    record.id,
                    record.status.label()
                )));
            }
            let reason: crate::store::ParkReason = lifecycle_payload(event, "reason")?;
            let engine: Option<crate::store::EnginePark> = event
                .payload
                .get("engine")
                .map(|engine| serde_json::from_value(engine.clone()))
                .transpose()
                .map_err(|error| {
                    PluginError::Session(format!(
                        "process event `{}` has an invalid engine park handle: {error}",
                        event.event_type
                    ))
                })?;
            match record.park.as_deref_mut() {
                Some(park) => {
                    park.reason = reason;
                    if engine.is_some() {
                        park.engine = engine;
                    }
                    park.last_refused_ms = event.occurred_at;
                    park.attempts = park.attempts.saturating_add(1);
                    park.refusing = true;
                }
                None => {
                    record.park = Some(Box::new(crate::store::ProcessPark {
                        reason,
                        park_id: crate::store::ParkId::from_feed_sequence(event.sequence),
                        since_ms: event.occurred_at,
                        last_refused_ms: event.occurred_at,
                        attempts: 1,
                        refusing: true,
                        engine,
                    }));
                }
            }
        }
        ProcessEventKind::ParkRerunBegan => {
            if let Some(park) = record.park.as_deref_mut() {
                park.refusing = false;
            }
        }
        ProcessEventKind::ObserverAdded
        | ProcessEventKind::ObserverRemoved
        | ProcessEventKind::SubscriptionRetargeted
        | ProcessEventKind::EffectOutcome
        | ProcessEventKind::EffectOmissions
        | ProcessEventKind::Custom => {}
        ProcessEventKind::UnknownRuntime => {
            return Err(PluginError::ReservedProcessEvent {
                event_type: event.event_type.clone(),
            });
        }
    }

    // The first fact of the process's own execution past a refusal ends its
    // park (NOW-B): the run got past replay. Registry-side facts — a start
    // marker, a cancel or abandon request, observer edges — say nothing about
    // whether the body can replay, so the park outlives them.
    if matches!(
        kind,
        ProcessEventKind::EffectOutcome
            | ProcessEventKind::EffectOmissions
            | ProcessEventKind::Waiting
            | ProcessEventKind::Resumed
            | ProcessEventKind::Custom
    ) {
        record.park = None;
    }

    if let Some(terminal) = event.semantics.terminal.clone() {
        if record.is_terminal() {
            return Ok(());
        }
        record.outcome = Some(terminal.outcome.clone());
        apply_process_status_projection(record, terminal.into_status(), event.occurred_at);
    } else {
        record.updated_at_ms = event.occurred_at;
    }
    record.last_event_sequence = event.sequence;
    Ok(())
}

/// The park feed transitions one appended event made to a process record
/// (NOW-B), given the record's park before the append and the projected
/// record after it. Every process store appends exactly these, in order, to
/// its process park feed inside the event's own transaction: a first park is
/// `Parked`, a re-park and a rerun's start are no transition at all, and the
/// fact that ends a park closes it as `Unparked` or `Cancelled`.
pub fn process_park_transitions(
    before: Option<&crate::store::ProcessPark>,
    after: &ProcessRecord,
) -> Vec<(crate::store::ParkId, crate::store::ParkEventKind)> {
    use crate::store::{ParkCancelCause, ParkEventKind, UnparkCause};
    let closing = |park: &crate::store::ProcessPark| {
        let kind = if after.status == ProcessStatus::Cancelled {
            ParkEventKind::Cancelled {
                cause: ParkCancelCause::ProcessCancelled {
                    origin: after
                        .cancel_request
                        .as_deref()
                        .map(|request| request.origin),
                },
            }
        } else if after.is_terminal() {
            ParkEventKind::Unparked {
                cause: UnparkCause::ProcessTerminal {
                    status: after.status,
                },
            }
        } else {
            ParkEventKind::Unparked {
                cause: UnparkCause::ProcessProgressed,
            }
        };
        (park.park_id, kind)
    };
    let opening = |park: &crate::store::ProcessPark| {
        (
            park.park_id,
            ParkEventKind::Parked {
                reason: park.reason.clone(),
            },
        )
    };
    match (before, after.park.as_deref()) {
        (None, None) => Vec::new(),
        (None, Some(opened)) => vec![opening(opened)],
        (Some(closed), None) => vec![closing(closed)],
        (Some(previous), Some(current)) if previous.park_id == current.park_id => Vec::new(),
        (Some(previous), Some(current)) => vec![closing(previous), opening(current)],
    }
}

/// Rebuild a process record by folding its persisted events in sequence order.
pub fn fold_process_record(
    mut record: ProcessRecord,
    events: &[ProcessEvent],
) -> Result<ProcessRecord, PluginError> {
    for event in events {
        apply_process_event_projection(&mut record, event)?;
    }
    Ok(record)
}

fn lifecycle_payload<T>(event: &ProcessEvent, field: &str) -> Result<T, PluginError>
where
    T: serde::de::DeserializeOwned,
{
    let value = event.payload.get(field).ok_or_else(|| {
        PluginError::Session(format!(
            "process event `{}` is missing lifecycle payload field `{field}`",
            event.event_type
        ))
    })?;
    serde_json::from_value(value.clone()).map_err(|err| {
        PluginError::Session(format!(
            "process event `{}` has invalid lifecycle payload field `{field}`: {err}",
            event.event_type
        ))
    })
}

fn process_external_ref_conflict(
    process_id: &ProcessId,
    existing: &super::model::ProcessExternalRef,
    requested: &super::model::ProcessExternalRef,
) -> PluginError {
    PluginError::Session(format!(
        "process `{process_id}` external ref conflict: existing {} / {}, requested {} / {}",
        existing.backend, existing.id, requested.backend, requested.id
    ))
}

fn cancel_request_payload(payload: &serde_json::Value) -> Result<CancelRequest, PluginError> {
    serde_json::from_value(payload.clone()).map_err(|error| {
        PluginError::Session(format!("invalid process.cancel_requested payload: {error}"))
    })
}

fn process_replay_payloads_match(
    event_type: &str,
    existing: &serde_json::Value,
    requested: &serde_json::Value,
) -> Result<bool, PluginError> {
    if ProcessEventKind::from_event_type(event_type) == ProcessEventKind::CancelRequested {
        return Ok(cancel_request_payload(existing)?
            .same_cancellation_as(&cancel_request_payload(requested)?));
    }
    Ok(crate::identity_json::payloads_equal(existing, requested))
}

fn repair_lifecycle_projection(
    record: &ProcessRecord,
    event: &ProcessEvent,
) -> Result<Option<ProcessRecord>, PluginError> {
    let mut repaired = record.clone();
    apply_process_event_projection(&mut repaired, event)?;
    Ok((repaired != *record).then_some(repaired))
}

pub fn prepare_process_event_append(
    record: &ProcessRecord,
    request: ProcessEventAppendRequest,
    sequence: u64,
    last_event_sequence: Option<u64>,
    replay_lookup: Option<ProcessEvent>,
    occurred_at_ms: u64,
    wake_session_id: Option<&SessionId>,
) -> Result<ProcessEventAppendPlan, PluginError> {
    let process_id = &record.id;
    let wake_suppressed = request.wake_suppressed;
    if ProcessEventKind::from_event_type(&request.event_type) == ProcessEventKind::UnknownRuntime {
        return Err(PluginError::ReservedProcessEvent {
            event_type: request.event_type.clone(),
        });
    }
    match ProcessEventKind::from_event_type(&request.event_type) {
        ProcessEventKind::EffectOutcome => {
            let outcome = super::effect_summary::ProcessEffectSummaryOccurrence::decode(
                request.payload.clone(),
            )
            .map_err(|error| PluginError::Session(error.to_string()))?;
            if request.replay.as_ref().map(|replay| replay.key.as_str())
                != Some(outcome.replay_key.as_str())
            {
                return Err(PluginError::Session(
                    "effect outcome payload replay_key must equal the append replay key"
                        .to_string(),
                ));
            }
        }
        ProcessEventKind::EffectOmissions => {
            super::effect_summary::ProcessEffectOmissions::decode(request.payload.clone())
                .map_err(|error| PluginError::Session(error.to_string()))?;
        }
        _ => {}
    }
    if let Some(replay_key) = request.replay.as_ref().map(|replay| replay.key.as_str())
        && let Some(existing) = replay_lookup
    {
        if existing.event_type == request.event_type
            && process_replay_payloads_match(
                &request.event_type,
                &existing.payload,
                &request.payload,
            )?
        {
            let repair_record = if last_event_sequence == Some(existing.sequence) {
                repair_lifecycle_projection(record, &existing)?
            } else {
                None
            };
            let wake_delivery = prepare_wake_delivery(
                process_id,
                record,
                existing.sequence,
                existing.event_type.clone(),
                existing.invocation.clone(),
                existing.occurred_at,
                existing.semantics.wake.clone(),
                wake_session_id,
                wake_suppressed,
            )?;
            return Ok(ProcessEventAppendPlan::Replay {
                event: existing,
                repair_record,
                wake_delivery,
            });
        }
        return Err(crate::durable_identity_conflict(format!(
            "process `{process_id}` event replay key `{replay_key}` conflicts with an existing event"
        )));
    }
    if record.is_terminal()
        && super::events::process_signal_name_from_event_type(&request.event_type).is_some()
    {
        return Err(PluginError::ProcessAlreadyTerminal {
            process_id: process_id.clone(),
            status: record.status,
        });
    }
    let runtime_owned = runtime_lifecycle_event_type(&request.event_type);
    let declared = runtime_owned
        .as_ref()
        .or_else(|| {
            record
                .event_types
                .iter()
                .find(|declared| declared.name == request.event_type)
        })
        .ok_or_else(|| {
            PluginError::Session(format!(
                "process `{process_id}` emitted undeclared event type `{}`",
                request.event_type
            ))
        })?;
    require_event_replay(process_id, &request, &declared.semantics)?;
    declared
        .payload_schema
        .validate(&request.payload)
        .map_err(|err| {
            PluginError::Session(format!("invalid `{}` payload: {err}", request.event_type))
        })?;
    let mut semantics = materialize_process_event_semantics(
        process_id,
        sequence,
        &request.payload,
        &declared.semantics,
    )?;
    if ProcessEventKind::from_event_type(&request.event_type) == ProcessEventKind::CancelRequested {
        let cancel = cancel_request_payload(&request.payload)?;
        if cancel.origin == CancelOrigin::StartFailed
            && record.first_started.is_none()
            && record.external_ref.is_none()
        {
            let cancellation =
                crate::ToolCancellation::runtime("process start failed before execution")
                    .with_origin(cancel.origin);
            semantics.terminal = Some(ProcessTerminalSemantics {
                status: ProcessStatus::Cancelled,
                outcome: ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::cancelled(
                    cancellation,
                )),
            });
        }
    }
    if let Some(terminal) = semantics.terminal.as_mut() {
        terminal.outcome = terminal.outcome.clone().with_cancel_origin(
            record
                .cancel_request
                .as_deref()
                .map(|request| request.origin),
        );
    }
    if semantics.terminal.is_some() && record.is_terminal() {
        return Err(PluginError::ProcessAlreadyTerminal {
            process_id: process_id.clone(),
            status: record.status,
        });
    }
    let event = ProcessEvent {
        process_id: process_id.clone(),
        sequence,
        event_type: request.event_type,
        payload: request.payload,
        invocation: crate::runtime::causal::process_event_invocation(
            process_id,
            sequence,
            declared.name.as_str(),
            request.replay,
        ),
        semantics: semantics.clone(),
        occurred_at: occurred_at_ms,
    };
    let mut projected_record = record.clone();
    apply_process_event_projection(&mut projected_record, &event)?;
    let wake_delivery = prepare_wake_delivery(
        process_id,
        record,
        event.sequence,
        event.event_type.clone(),
        event.invocation.clone(),
        event.occurred_at,
        semantics.wake.clone(),
        wake_session_id,
        wake_suppressed,
    )?;
    debug_assert!(
        !is_runtime_lifecycle_event_type(&event.event_type)
            || event
                .invocation
                .replay_key()
                .is_none_or(|key| { !key.ends_with(FOLD_VALIDATION_REPLAY_KEY_SUFFIX) }),
        "fold-validation replay keys must be refused before a process-event insert is planned"
    );
    Ok(ProcessEventAppendPlan::Insert {
        event,
        projected_record,
        wake_delivery,
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "wake delivery mirrors the persisted event plus its optional materialized wake"
)]
fn prepare_wake_delivery(
    process_id: &ProcessId,
    record: &ProcessRecord,
    sequence: u64,
    event_type: String,
    event_invocation: crate::RuntimeInvocation,
    occurred_at: u64,
    wake: Option<super::events::ProcessWake>,
    wake_session_id: Option<&SessionId>,
    wake_suppressed: bool,
) -> Result<Option<ProcessWakeDelivery>, PluginError> {
    // A suppressed append still materializes its event and its semantics; it
    // only withholds the delivery. The wake is the one thing a session would
    // observe, so withholding it here — after the event and its semantics are
    // settled — is the whole of the suppression, and an observer reading the
    // journal cannot tell a suppressed append from an unsuppressed one.
    if wake_suppressed {
        return Ok(None);
    }
    let Some(wake) = wake else {
        return Ok(None);
    };
    let Some(target_session_id) = wake_session_id else {
        return Ok(None);
    };
    process_wake_delivery(ProcessWakeDeliveryRequest {
        target_session_id: SessionId::from(target_session_id.to_string()),
        process_id: process_id.clone(),
        sequence,
        event_type,
        event_invocation,
        process_caused_by: record.provenance.caused_by.clone(),
        authority: match &record.provenance.originator {
            super::model::ProcessOriginator::Host { .. } => {
                crate::QueuedWorkAuthority::new(record.originator_id())
            }
            super::model::ProcessOriginator::Session {
                session_id,
                agent_frame_id,
            } => {
                let authority = crate::QueuedWorkAuthority::new(session_id.clone());
                match agent_frame_id {
                    Some(frame_id) => authority.with_elevation(frame_id.clone()),
                    None => authority,
                }
            }
        },
        wake,
        occurred_at_ms: occurred_at,
    })
    .map(Some)
}

pub fn prepare_process_registration(
    mut registration: ProcessRegistration,
) -> Result<ProcessRegistration, PluginError> {
    validate_process_registration(&registration)?;
    ensure_core_event_types(&mut registration);
    registration
        .event_types
        .retain(|event_type| !is_runtime_lifecycle_event_type(&event_type.name));
    Ok(registration)
}

/// Decides whether a start that found `retained` under its key may be
/// returned it (ADR 0107).
///
/// A key lash derives from an admitted operation is trusted: the retained
/// process is returned whatever the start submitted. A host's key (one it
/// supplied, or its keyless start's derived key) is the host's claim that the
/// two starts are one, so a start under it with different content is a
/// [`durable_identity_conflict`](crate::durable_identity_conflict) rather than
/// a silent return of another start's process.
///
/// # Errors
///
/// A registration that does not validate, or the conflict.
pub fn check_retained_start(
    registration: &ProcessRegistration,
    retained: &ProcessRecord,
) -> Result<(), PluginError> {
    let Some(start_key) = registration.start_key.as_ref() else {
        return Ok(());
    };
    if !start_key.fences_content() {
        return Ok(());
    }
    let submitted = prepare_process_registration(registration.clone())?;
    let same = submitted.input == retained.input
        && submitted.disposition == retained.disposition
        && submitted.lifecycle == retained.lifecycle
        && submitted.max_attempts == retained.max_attempts
        && submitted.identity == retained.identity
        && submitted.event_types == retained.event_types
        && submitted.provenance == retained.provenance
        && submitted.env_ref == retained.env_ref;
    if same {
        Ok(())
    } else {
        Err(crate::durable_identity_conflict(format!(
            "process start key `{start_key}` is bound to process `{}`, which was started with different content",
            retained.id
        )))
    }
}

pub fn require_event_replay(
    process_id: &ProcessId,
    request: &ProcessEventAppendRequest,
    spec: &ProcessEventSemanticsSpec,
) -> Result<(), PluginError> {
    let requires_key = spec.terminal.is_some()
        || matches!(
            request.event_type.as_str(),
            "process.cancel_requested"
                | "process.first_started"
                | "process.waiting"
                | "process.resumed"
                | "process.external_ref_set"
                | "process.abandon_requested"
                | "process.parked"
                | "process.park_rerun_began"
                | "process.observer_added"
                | "process.observer_removed"
                | "process.subscription_retargeted"
                | super::effect_summary::PROCESS_EFFECT_OUTCOME_EVENT_TYPE
                | super::effect_summary::PROCESS_EFFECT_OMISSIONS_EVENT_TYPE
        );
    if requires_key
        && request
            .replay
            .as_ref()
            .is_none_or(|replay| replay.key.is_empty())
    {
        return Err(PluginError::Session(format!(
            "process `{process_id}` event `{}` requires a deterministic replay key",
            request.event_type
        )));
    }
    Ok(())
}

pub(super) fn ensure_core_event_types(registration: &mut ProcessRegistration) {
    let mut existing = registration
        .event_types
        .iter()
        .map(|event_type| event_type.name.clone())
        .collect::<HashSet<_>>();
    for event_type in default_process_event_types() {
        if existing.insert(event_type.name.clone()) {
            registration.event_types.push(event_type);
        }
    }
}

/// One refusal rule enforced by [`validate_process_registration`].
///
/// Remote ingress must refuse every shape core refuses (FIG-2985), so this
/// registry is the shared vocabulary of the two validators: each variant has a
/// fixture in [`crate::testing::refused_process_registrations`], and both the
/// core parity test and the `lash-remote-protocol` decoder parity test iterate
/// [`ProcessRegistrationRefusal::ALL`]. Adding a core rule means adding a
/// variant, which stops the exhaustive fixture match from compiling until the
/// new shape is also fed through the remote decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProcessRegistrationRefusal {
    HostParentCancels,
    TurnParentSessionMismatch,
    ZeroMaxAttempts,
    ToolCallWithoutCallId,
    ToolCallWithoutToolName,
    ExecutionEnvMissing,
    ExecutionEnvNotAllowed,
    EmptySessionTurnDefinitionKey,
    EmptyEventTypeName,
    DuplicateEventType,
    ReservedRuntimeEventType,
    NonTerminalTerminalStatus,
    TerminalEventWithoutAwaitOutput,
}

impl ProcessRegistrationRefusal {
    /// Every refusal rule, in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::HostParentCancels,
        Self::TurnParentSessionMismatch,
        Self::ZeroMaxAttempts,
        Self::ToolCallWithoutCallId,
        Self::ToolCallWithoutToolName,
        Self::ExecutionEnvMissing,
        Self::ExecutionEnvNotAllowed,
        Self::EmptySessionTurnDefinitionKey,
        Self::EmptyEventTypeName,
        Self::DuplicateEventType,
        Self::ReservedRuntimeEventType,
        Self::NonTerminalTerminalStatus,
        Self::TerminalEventWithoutAwaitOutput,
    ];
}

/// A registration core refuses: the rule that fired, with the error callers see.
pub(crate) type ProcessRegistrationRefused = (ProcessRegistrationRefusal, PluginError);

fn refuse(rule: ProcessRegistrationRefusal, message: String) -> ProcessRegistrationRefused {
    (rule, PluginError::Session(message))
}

/// How a refusal names the start it refused: a registration carries no process
/// id until the registrar mints one, so it is named by its key when it has one.
fn registration_name(registration: &ProcessRegistration) -> String {
    registration.refusal_name()
}

/// Validates a registration and names the rule that refused it.
///
/// [`validate_process_registration`] is the plain-error face of this function;
/// the rule tag exists so the parity fixtures can assert that each fixture
/// trips the rule it was written for rather than an unrelated earlier check.
pub(crate) fn classify_process_registration(
    registration: &ProcessRegistration,
) -> Result<(), ProcessRegistrationRefused> {
    match &registration.lifecycle.parent {
        super::model::ParentScope::Host
            if registration.lifecycle.on_parent_end == super::model::OnParentEnd::Cancel =>
        {
            return Err(refuse(
                ProcessRegistrationRefusal::HostParentCancels,
                "Host parent scope cannot declare Cancel: a host scope never ends".to_string(),
            ));
        }
        super::model::ParentScope::Owned(
            crate::EffectOpener::Turn { session_id, .. }
            | crate::EffectOpener::QueueDrain { session_id, .. },
        ) if !matches!(
            &registration.provenance.originator,
            super::model::ProcessOriginator::Session { session_id: originator, .. }
                if originator == session_id
        ) =>
        {
            return Err(refuse(
                ProcessRegistrationRefusal::TurnParentSessionMismatch,
                "turn or drain parent session must match the process originator session"
                    .to_string(),
            ));
        }
        _ => {}
    }
    if registration.max_attempts == Some(0) {
        return Err(refuse(
            ProcessRegistrationRefusal::ZeroMaxAttempts,
            format!(
                "process `{}` max_attempts must be greater than zero",
                registration_name(registration)
            ),
        ));
    }
    match registration.input.as_ref() {
        super::model::ProcessInput::ToolCall { call } => {
            if call.call_id.trim().is_empty() {
                return Err(refuse(
                    ProcessRegistrationRefusal::ToolCallWithoutCallId,
                    format!(
                        "process `{}` tool call must carry a call id",
                        registration_name(registration)
                    ),
                ));
            }
            if call.tool_name.trim().is_empty() {
                return Err(refuse(
                    ProcessRegistrationRefusal::ToolCallWithoutToolName,
                    format!(
                        "process `{}` tool call must carry a tool name",
                        registration_name(registration)
                    ),
                ));
            }
            if registration.env_ref.is_none() {
                return Err(refuse(
                    ProcessRegistrationRefusal::ExecutionEnvMissing,
                    format!(
                        "process `{}` requires a captured execution env",
                        registration_name(registration)
                    ),
                ));
            }
        }
        super::model::ProcessInput::Engine { .. } => {
            if registration.env_ref.is_none() {
                return Err(refuse(
                    ProcessRegistrationRefusal::ExecutionEnvMissing,
                    format!(
                        "process `{}` requires a captured execution env",
                        registration_name(registration)
                    ),
                ));
            }
        }
        super::model::ProcessInput::External { .. } => {
            if registration.env_ref.is_some() {
                return Err(refuse(
                    ProcessRegistrationRefusal::ExecutionEnvNotAllowed,
                    format!(
                        "process `{}` must not capture an execution env for this input kind",
                        registration_name(registration)
                    ),
                ));
            }
        }
        super::model::ProcessInput::SessionTurn { definition_key, .. } => {
            if definition_key.trim().is_empty() {
                return Err(refuse(
                    ProcessRegistrationRefusal::EmptySessionTurnDefinitionKey,
                    format!(
                        "process `{}` session-turn definition_key must not be empty",
                        registration_name(registration)
                    ),
                ));
            }
            if registration.env_ref.is_some() {
                return Err(refuse(
                    ProcessRegistrationRefusal::ExecutionEnvNotAllowed,
                    format!(
                        "process `{}` must not capture an execution env for this input kind",
                        registration_name(registration)
                    ),
                ));
            }
        }
    }
    let mut names = HashSet::new();
    for event_type in &registration.event_types {
        if event_type.name.trim().is_empty() {
            return Err(refuse(
                ProcessRegistrationRefusal::EmptyEventTypeName,
                format!(
                    "process `{}` declares an empty event type",
                    registration_name(registration)
                ),
            ));
        }
        if !names.insert(event_type.name.as_str()) {
            return Err(refuse(
                ProcessRegistrationRefusal::DuplicateEventType,
                format!(
                    "process `{}` declares duplicate event type `{}`",
                    registration_name(registration),
                    event_type.name
                ),
            ));
        }
        if let Some(runtime_owned) = runtime_lifecycle_event_type(&event_type.name)
            && event_type != &runtime_owned
        {
            return Err(refuse(
                ProcessRegistrationRefusal::ReservedRuntimeEventType,
                format!(
                    "process `{}` declares reserved runtime lifecycle event type `{}`",
                    registration_name(registration),
                    event_type.name
                ),
            ));
        }
        if let Some(terminal) = &event_type.semantics.terminal {
            if !terminal.status.is_terminal() {
                return Err(refuse(
                    ProcessRegistrationRefusal::NonTerminalTerminalStatus,
                    format!(
                        "terminal event `{}` for process `{}` must declare a terminal status, got `{}`",
                        event_type.name,
                        registration_name(registration),
                        terminal.status.label()
                    ),
                ));
            }
            if terminal.status != ProcessStatus::Completed && terminal.await_output.is_none() {
                return Err(refuse(
                    ProcessRegistrationRefusal::TerminalEventWithoutAwaitOutput,
                    format!(
                        "terminal event `{}` for process `{}` must declare await output",
                        event_type.name,
                        registration_name(registration)
                    ),
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_process_registration(
    registration: &ProcessRegistration,
) -> Result<(), PluginError> {
    classify_process_registration(registration).map_err(|(_rule, error)| error)
}

#[cfg(test)]
#[path = "validation_tests.rs"]
mod tests;
