use crate::ProcessId;
use lash_sansio::{CancelOrigin, CancelRequest};
use std::collections::HashSet;

use crate::SessionId;
use crate::plugin::PluginError;

use super::events::{
    ProcessEvent, ProcessEventAppendRequest, ProcessEventKind, ProcessEventSemanticsSpec,
    ProcessTerminal, ProcessTerminalSemantics, ProcessWakeDelivery, default_process_event_types,
    is_runtime_lifecycle_event_type, runtime_lifecycle_event_type,
};
use super::materialization::materialize_process_event_semantics;
use super::model::{
    ProcessExternalRef, ProcessLifecycleState, ProcessRecord, ProcessRegistration, ProcessStarted,
    ProcessStatus, TerminalProcessStatus, WaitState,
};

pub fn validate_generic_process_event_append(
    request: &ProcessEventAppendRequest,
) -> Result<(), PluginError> {
    validate_process_signal_append(request)?;
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
        "process.observer_added" | "process.observer_removed" | "process.subscription_retargeted"
    ) {
        return Err(PluginError::ReservedProcessEvent {
            event_type: request.event_type.clone(),
        });
    }
    Ok(())
}

fn validate_process_signal_append(request: &ProcessEventAppendRequest) -> Result<(), PluginError> {
    let valid = match &request.signal_identity {
        Some(identity) => {
            request.event_type == identity.event_type()
                && request.replay.as_ref().map(|replay| replay.key.as_str())
                    == Some(identity.append_key().as_str())
                && !request.wake_suppressed
        }
        None => !request.event_type.starts_with("signal."),
    };
    if !valid {
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
}

/// A registry-owned lifecycle transition whose append disposition is shared
/// across every process-store implementation.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessTransition {
    /// Bind the process to durable work owned by another backend.
    SetExternalRef(ProcessExternalRef),
    /// Record a typed process cancellation request.
    RequestCancel(CancelRequest),
    /// Enter a durable wait state.
    EnterWait(WaitState),
    ClearWait,
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

/// Plan the write of an execution-started fact.
///
/// The engine decides whether a start may run (ADR 0110): lash never re-runs
/// started work from scratch. Recovery continues from committed state; a
/// started `Once` execution without an outcome is interrupted. The registry
/// only keeps the fact consistent: the same execution is idempotent, and a
/// successor execution resumed from committed state takes the next attempt.
pub fn prepare_process_start(
    record: &ProcessRecord,
    started: &ProcessStarted,
) -> Result<ProcessStartPlan, PluginError> {
    if record.is_terminal() {
        return Err(PluginError::Session(format!(
            "terminal process `{}` cannot start an execution attempt",
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
    let expected_attempt = record
        .first_started
        .as_deref()
        .map_or(1, |existing| existing.attempt.saturating_add(1));
    if started.attempt != expected_attempt {
        return Err(PluginError::Session(format!(
            "process `{}` execution attempt must be {}, got {}",
            record.id, expected_attempt, started.attempt
        )));
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
            // pass never rewrites a row it coalesced onto.
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
                        status: record.status(),
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
                // no-op, so a resubmitting pass never rewrites a row it
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
        ProcessTransition::EnterWait(wait) => {
            if record.wait() == Some(&wait) {
                return Ok(ProcessTransitionPlan::Unchanged);
            }
            let mut append = ProcessEventAppendRequest::wait_entered(&record.id, &wait);
            if record.is_terminal() {
                route_transition_refusal_to_fold(&mut append)?;
            }
            append
        }
        ProcessTransition::ClearWait => {
            let Some(wait) = record.wait() else {
                return Ok(ProcessTransitionPlan::Unchanged);
            };
            ProcessEventAppendRequest::wait_cleared(&record.id, wait)
        }
    };
    Ok(ProcessTransitionPlan::Append(Box::new(append)))
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
            match record.first_started.as_deref() {
                None => record.first_started = Some(Box::new(started)),
                Some(existing) if existing.same_execution(&started) => {}
                Some(existing) if started.attempt == existing.attempt.saturating_add(1) => {
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
        ProcessEventKind::Waiting => match &record.lifecycle {
            ProcessLifecycleState::Terminal { .. } => {
                return Err(PluginError::Session(format!(
                    "terminal process `{}` cannot enter a wait state",
                    record.id
                )));
            }
            ProcessLifecycleState::Running { .. } | ProcessLifecycleState::Waiting { .. } => {
                record.lifecycle = ProcessLifecycleState::Waiting {
                    wait: lifecycle_payload(event, "wait")?,
                };
            }
        },
        ProcessEventKind::Resumed => match &record.lifecycle {
            // An ended process stays ended: no later fact takes its outcome
            // back, so a resume cannot return it to running.
            ProcessLifecycleState::Terminal { .. } => {
                return Err(PluginError::ProcessAlreadyTerminal {
                    process_id: record.id.clone(),
                    status: record.status(),
                });
            }
            ProcessLifecycleState::Running { .. } | ProcessLifecycleState::Waiting { .. } => {
                record.lifecycle = ProcessLifecycleState::running();
            }
        },
        ProcessEventKind::ExternalRefSet => {
            let external_ref = lifecycle_payload(event, "external_ref")?;
            // Compare-and-set on the segment ordinal, never last-write-wins.
            // A live host and the recovery pass may both submit the same
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
            let own_terminal_replay =
                record.status() == ProcessStatus::Cancelled
                    && record.last_event_sequence == event.sequence
                    && request.origin == CancelOrigin::StartFailed
                    && event.semantics.terminal.as_ref().is_some_and(|terminal| {
                        terminal.status() == TerminalProcessStatus::Cancelled
                    });
            if record.is_terminal() && !own_terminal_replay {
                return Err(PluginError::ProcessAlreadyTerminal {
                    process_id: record.id.clone(),
                    status: record.status(),
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

    if let Some(terminal) = event.semantics.terminal.as_ref() {
        if record.is_terminal() {
            return Ok(());
        }
        // The outcome is the state: it takes the wait with it.
        record.lifecycle = ProcessLifecycleState::Terminal {
            outcome: terminal.outcome.clone(),
        };
    }
    record.updated_at_ms = event.occurred_at;
    record.last_event_sequence = event.sequence;
    Ok(())
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

/// Admit `request`, a signal's append, against `record` as it stands,
/// writing nothing: what a store-local signal checks before its call's
/// outcome commits it. The append in that commit applies the same rules;
/// a target that ended meanwhile takes nothing.
///
/// # Errors
///
/// The refusal the append would answer: a malformed signal, a signal to
/// another process, a terminal target, an undeclared signal, or a payload
/// its declaration refuses.
pub fn admit_process_signal_append(
    record: &ProcessRecord,
    request: &ProcessEventAppendRequest,
) -> Result<(), PluginError> {
    validate_process_signal_append(request)?;
    if super::events::process_signal_name_from_event_type(&request.event_type).is_none()
        || request
            .signal_identity
            .as_ref()
            .is_none_or(|identity| *identity.process_id() != record.id)
    {
        return Err(PluginError::ReservedProcessEvent {
            event_type: request.event_type.clone(),
        });
    }
    if record.is_terminal() {
        return Err(PluginError::ProcessAlreadyTerminal {
            process_id: record.id.clone(),
            status: record.status(),
        });
    }
    let declared = record
        .event_types
        .iter()
        .find(|declared| declared.name == request.event_type)
        .ok_or_else(|| {
            PluginError::Session(format!(
                "process `{}` emitted undeclared event type `{}`",
                record.id, request.event_type
            ))
        })?;
    require_event_replay(&record.id, request, &declared.semantics)?;
    declared
        .payload_schema
        .validate(&request.payload)
        .map_err(|err| PluginError::ValueMismatch {
            context: format!("`{}` payload", request.event_type),
            source: Box::new(err),
        })
}

#[allow(
    clippy::too_many_arguments,
    reason = "the append plan carries the journal's own positional facts; fleet format joins them as one more stamped input (FIG-3796)"
)]
/// `signal_events_before` is how many events of the request's type the
/// process's log already holds, counted by the store in the append's own
/// transaction. A store passes it for a signal append (its event type names a
/// signal) and `None` for every other: the append selects the wait a new
/// signal resolves from it and the process's current wait
/// ([`select_process_signal_wait`]).
pub fn prepare_process_event_append(
    record: &ProcessRecord,
    request: ProcessEventAppendRequest,
    sequence: u64,
    last_event_sequence: Option<u64>,
    replay_lookup: Option<ProcessEvent>,
    signal_events_before: Option<u64>,
    occurred_at_ms: u64,
    wake_session_id: Option<&SessionId>,
    fleet_format: crate::FleetFormat,
) -> Result<ProcessEventAppendPlan, PluginError> {
    let process_id = &record.id;
    validate_process_signal_append(&request)?;
    if request
        .signal_identity
        .as_ref()
        .is_some_and(|identity| identity.process_id() != process_id)
    {
        return Err(PluginError::ReservedProcessEvent {
            event_type: request.event_type.clone(),
        });
    }
    let wake_suppressed = request.wake_suppressed;
    if ProcessEventKind::from_event_type(&request.event_type) == ProcessEventKind::UnknownRuntime {
        return Err(PluginError::ReservedProcessEvent {
            event_type: request.event_type.clone(),
        });
    }
    match ProcessEventKind::from_event_type(&request.event_type) {
        ProcessEventKind::EffectOutcome => {
            let outcome = super::effect_summary::ProcessEffectOccurrence::decode(
                request.payload.clone(),
                fleet_format,
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
            super::effect_summary::ProcessEffectOmissions::decode(
                request.payload.clone(),
                fleet_format,
            )
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
                existing.occurred_at,
                existing.semantics.wake.clone(),
                &existing.semantics.trace_cause,
                wake_session_id,
                wake_suppressed,
                fleet_format,
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
            status: record.status(),
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
        .map_err(|err| PluginError::ValueMismatch {
            context: format!("`{}` payload", request.event_type),
            source: Box::new(err),
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
                outcome: ProcessTerminal::from_tool_output(crate::ToolCallOutput::cancelled(
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
            status: record.status(),
        });
    }
    semantics.trace_cause = request.trace_cause;
    semantics.signal_wait = super::events::process_signal_name_from_event_type(&request.event_type)
        .map(|signal_name| {
            select_process_signal_wait(
                record,
                signal_name,
                &request.event_type,
                signal_events_before,
            )
        })
        .transpose()?;
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
        event.occurred_at,
        semantics.wake.clone(),
        &semantics.trace_cause,
        wake_session_id,
        wake_suppressed,
        fleet_format,
    )?;
    debug_assert!(
        !is_runtime_lifecycle_event_type(&event.event_type)
            || event
                .invocation
                .effect_replay_key()
                .is_none_or(|key| { !key.ends_with(FOLD_VALIDATION_REPLAY_KEY_SUFFIX) }),
        "fold-validation replay keys must be refused before a process-event insert is planned"
    );
    Ok(ProcessEventAppendPlan::Insert {
        event,
        projected_record,
        wake_delivery,
    })
}

/// The wait a newly admitted signal resolves (FIG-4298): the ordinal of the
/// wait `record` is parked on for this signal, or, when it is parked on none,
/// the signal's position among the events of its type, which is the ordinal
/// the process's next wait for the name declares.
///
/// The declared ordinal wins over the count: a process's wait ordinals are
/// its own, and they need not match how many signals of the name its log
/// holds.
fn select_process_signal_wait(
    record: &ProcessRecord,
    signal_name: &str,
    event_type: &str,
    signal_events_before: Option<u64>,
) -> Result<super::events::ProcessSignalWaitBinding, PluginError> {
    if let Some(super::model::WaitState {
        kind:
            super::model::WaitKind::Signal {
                name,
                event_type: waiting_type,
                ordinal,
                ..
            },
        ..
    }) = record.wait()
        && name == signal_name
        && waiting_type == event_type
    {
        return Ok(super::events::ProcessSignalWaitBinding { ordinal: *ordinal });
    }
    let before = signal_events_before.ok_or_else(|| {
        PluginError::Session(format!(
            "process `{}` signal `{event_type}` append was prepared without the count of its \
             prior events the store must supply",
            record.id
        ))
    })?;
    Ok(super::events::ProcessSignalWaitBinding {
        ordinal: before.saturating_add(1),
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
    occurred_at: u64,
    wake: Option<super::events::ProcessWake>,
    event_trace_cause: &lash_trace::TraceCause,
    wake_session_id: Option<&SessionId>,
    wake_suppressed: bool,
    fleet_format: crate::FleetFormat,
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
        target_session_id: target_session_id.clone(),
        process_id: process_id.clone(),
        sequence,
        event_type,
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
        // The wake's producer is whoever caused the event that woke the
        // session; an event no one outside caused is the process's own.
        trace_cause: if event_trace_cause.is_root() {
            record
                .trace
                .as_ref()
                .map(lash_trace::DurableTraceScope::linked_cause)
                .unwrap_or_default()
        } else {
            event_trace_cause.clone()
        },
        occurred_at_ms: occurred_at,
        fleet_format,
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

/// A start whose consumer hold was abandoned by its cancelled logical Run.
/// The opener already owns the drain of the hold's obligations.
pub fn abandoned_consumer_refusal(start_key: Option<&crate::StartKey>, key: &str) -> PluginError {
    PluginError::Runtime(crate::RuntimeError::new(
        crate::RuntimeErrorCode::RuntimeToolRunCancelDecided,
        format!(
            "cannot register process start {start_key:?}: the call holding it under `{key}` \
             was abandoned"
        ),
    ))
}

/// Decides whether a start that found `retained` under its key may be
/// returned it (ADR 0107).
///
/// A key lash derives from an admitted operation is trusted: the retained
/// process is returned whatever the start submitted. A host's key (one it
/// supplied, or its keyless start's derived key) fences its start: the
/// retained process is returned only to a start that presents the same
/// input, lifetime decision, ancestry, originator, wake target and
/// environment. Each is what the host stated: a start carries nothing lash
/// derives from a catalog or a deployment default (FIG-4594), so a retry
/// after either changed presents the same start. A host key is global, so the retained process may be another
/// originator's; any other start under it is a
/// [`PluginError::StartKeyConflict`] that names the key and nothing of the
/// process it is bound to.
///
/// `retained_wake_session_id` is the retained row's wake target, which the
/// registrar stores beside the record rather than in it.
///
/// # Errors
///
/// A registration that does not validate, or the conflict.
pub fn check_retained_start(
    registration: &ProcessRegistration,
    retained: &ProcessRecord,
    retained_wake_session_id: Option<&SessionId>,
) -> Result<(), PluginError> {
    let Some(start_key) = registration.start_key.as_ref() else {
        return Ok(());
    };
    if !start_key.fences_content() {
        return Ok(());
    }
    let submitted = prepare_process_registration(registration.clone())?;
    let same = submitted.input == retained.input
        && submitted.lifetime == retained.lifetime
        && submitted.ancestry == retained.ancestry
        && submitted.session_capability == retained.session_capability
        && submitted.identity == retained.identity
        && submitted.event_types == retained.event_types
        && submitted.provenance == retained.provenance
        && submitted.wake_session_id.as_ref() == retained_wake_session_id
        && submitted.env_ref == retained.env_ref;
    if same {
        Ok(())
    } else {
        Err(PluginError::StartKeyConflict {
            start_key: start_key.clone(),
        })
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
/// core parity test and the core admission law iterate
/// [`ProcessRegistrationRefusal::ALL`]. Adding a core rule means adding a
/// variant, which stops the exhaustive fixture match from compiling until the
/// new shape is also fed through the remote decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProcessRegistrationRefusal {
    LifetimeScopeUnreachable,
    HostGrantOutsideRoot,
    SessionCapabilityUnreachable,
    ExecutionEnvMissing,
    EmptySessionTurnDefinitionKey,
    EmptyEventTypeName,
    DuplicateEventType,
    ReservedRuntimeEventType,
    TerminalEventWithoutAwaitOutput,
}

impl ProcessRegistrationRefusal {
    /// Every refusal rule, in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::LifetimeScopeUnreachable,
        Self::HostGrantOutsideRoot,
        Self::SessionCapabilityUnreachable,
        Self::ExecutionEnvMissing,
        Self::EmptySessionTurnDefinitionKey,
        Self::EmptyEventTypeName,
        Self::DuplicateEventType,
        Self::ReservedRuntimeEventType,
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
    // Reachability (FIG-3607 R3), checked on every new start in every build:
    // a runtime start's lifetime names a scope in its admitted ancestry, and
    // a host's session-lookup grant is a root's only grant.
    match &registration.lifetime {
        super::model::LifetimeDecision::Detached => {}
        super::model::LifetimeDecision::Until {
            scope,
            grant: super::model::ScopeGrant::Ancestor,
        } => {
            if !registration.ancestry.contains(scope) {
                return Err(refuse(
                    ProcessRegistrationRefusal::LifetimeScopeUnreachable,
                    format!(
                        "{} names lifetime scope `{scope}`, which is not in its admitted ancestry",
                        registration_name(registration)
                    ),
                ));
            }
        }
        super::model::LifetimeDecision::Until {
            scope,
            grant: super::model::ScopeGrant::HostSessionLookup,
        } => {
            if !registration.ancestry.is_root()
                || !matches!(scope, super::model::ScopeId::Session(_))
            {
                return Err(refuse(
                    ProcessRegistrationRefusal::HostGrantOutsideRoot,
                    format!(
                        "{} holds a host session-lookup grant for `{scope}`, which only a root start's session may carry",
                        registration_name(registration)
                    ),
                ));
            }
        }
    }
    if let Some(session_id) = registration.session_capability.as_ref() {
        let scope = super::model::ScopeId::Session(session_id.clone());
        let held = registration.ancestry.contains(&scope)
            || (registration.ancestry.is_root()
                && registration.lifetime
                    == (super::model::LifetimeDecision::Until {
                        scope,
                        grant: super::model::ScopeGrant::HostSessionLookup,
                    }));
        if !held {
            return Err(refuse(
                ProcessRegistrationRefusal::SessionCapabilityUnreachable,
                format!(
                    "{} carries session capability `{session_id}`, which neither its ancestry nor a host lookup grants",
                    registration_name(registration)
                ),
            ));
        }
    }
    match registration.input.as_ref() {
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
            // A session-turn process runs under the environment its start
            // captured, exactly as an engine process does (FIG-4396).
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
        if let Some(terminal) = &event_type.semantics.terminal
            && terminal.status != TerminalProcessStatus::Completed
            && terminal.await_output.is_none()
        {
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
