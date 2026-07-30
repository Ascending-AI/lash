use std::collections::HashSet;

use crate::SessionId;
use crate::plugin::PluginError;

use super::events::{
    ProcessEvent, ProcessEventAppendRequest, ProcessEventSemanticsSpec, ProcessWakeDelivery,
    default_process_event_types, is_runtime_lifecycle_event_type, runtime_lifecycle_event_type,
};
use super::materialization::materialize_process_event_semantics;
use super::model::{
    ProcessRecord, ProcessRegistration, ProcessStarted, ProcessStatus, RecoveryDisposition,
};
use super::time::{epoch_ms_from_system_time, system_time_from_epoch_ms};

pub fn validate_generic_process_event_append(
    request: &ProcessEventAppendRequest,
) -> Result<(), PluginError> {
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
use super::wake::{ProcessWakeDeliveryRequest, process_wake_delivery};

#[derive(Clone, Debug)]
pub enum ProcessEventAppendPlan {
    Insert {
        event: ProcessEvent,
        payload_hash: String,
        projected_record: ProcessRecord,
        wake_delivery: Option<ProcessWakeDelivery>,
        occurred_at_ms: u64,
    },
    Replay {
        event: ProcessEvent,
        repair_record: Option<ProcessRecord>,
        wake_delivery: Option<ProcessWakeDelivery>,
        occurred_at_ms: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessStartPlan {
    Append,
    AlreadyApplied,
    AlreadyStarted { by: crate::LeaseOwnerIdentity },
    AttemptsExhausted { attempts: u32, max_attempts: u32 },
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
    if record.disposition == RecoveryDisposition::ExternallyOwned {
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
    authority.validate_resume_predecessor(record.id.as_str(), record.first_started.as_deref())?;

    let expected_attempt = match record.first_started.as_deref() {
        None => 1,
        Some(existing)
            if record.disposition == RecoveryDisposition::OwnerBound
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
    if let Some(max_attempts) = record.max_attempts
        && started.attempt > max_attempts
    {
        return Ok(ProcessStartPlan::AttemptsExhausted {
            attempts: started.attempt.saturating_sub(1),
            max_attempts,
        });
    }
    Ok(ProcessStartPlan::Append)
}

pub fn apply_process_status_projection(
    record: &mut ProcessRecord,
    status: ProcessStatus,
    updated_at_ms: u64,
) {
    record.status = status;
    if record.status.is_terminal() {
        record.wait = None;
    }
    record.updated_at_ms = updated_at_ms;
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

    match event.event_type.as_str() {
        "process.first_started" => {
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
                    if (record.disposition == RecoveryDisposition::Rerunnable
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
        "process.waiting" => {
            if record.is_terminal() {
                return Err(PluginError::Session(format!(
                    "terminal process `{}` cannot enter a wait state",
                    record.id
                )));
            }
            record.wait = Some(lifecycle_payload(event, "wait")?);
            record.status = ProcessStatus::Waiting;
        }
        "process.resumed" => {
            record.wait = None;
            record.status = ProcessStatus::Running;
        }
        "process.external_ref_set" => {
            let external_ref = lifecycle_payload(event, "external_ref")?;
            match record.external_ref.as_ref() {
                None => record.external_ref = Some(external_ref),
                Some(existing) if existing == &external_ref => {}
                Some(existing) => {
                    return Err(process_external_ref_conflict(
                        &record.id,
                        existing,
                        &external_ref,
                    ));
                }
            }
        }
        "process.abandon_requested" => {
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
        _ => {}
    }

    if let Some(terminal) = event.semantics.terminal.clone() {
        record.outcome = Some(terminal.outcome.clone());
        apply_process_status_projection(
            record,
            ProcessStatus::from_terminal(terminal),
            epoch_ms_from_system_time(event.occurred_at),
        );
    } else {
        record.updated_at_ms = epoch_ms_from_system_time(event.occurred_at);
    }
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
    process_id: &str,
    existing: &super::model::ProcessExternalRef,
    requested: &super::model::ProcessExternalRef,
) -> PluginError {
    PluginError::Session(format!(
        "process `{process_id}` external ref conflict: existing {} / {}, requested {} / {}",
        existing.backend, existing.id, requested.backend, requested.id
    ))
}

fn repair_lifecycle_projection(
    record: &ProcessRecord,
    event: &ProcessEvent,
) -> Result<Option<ProcessRecord>, PluginError> {
    let mut repaired = record.clone();
    let changed = match event.event_type.as_str() {
        "process.first_started" => {
            let value = Box::new(lifecycle_payload(event, "started")?);
            let changed = repaired.first_started.as_ref() != Some(&value);
            repaired.first_started = Some(value);
            changed
        }
        "process.external_ref_set" => {
            let value = lifecycle_payload(event, "external_ref")?;
            let changed = repaired.external_ref.as_ref() != Some(&value);
            repaired.external_ref = Some(value);
            changed
        }
        "process.abandon_requested" => {
            let value = Box::new(lifecycle_payload(event, "request")?);
            let changed = repaired.abandon_request.as_ref() != Some(&value);
            repaired.abandon_request = Some(value);
            changed
        }
        "process.waiting" => {
            let value = lifecycle_payload(event, "wait")?;
            let changed =
                repaired.wait.as_ref() != Some(&value) || repaired.status != ProcessStatus::Waiting;
            repaired.wait = Some(value);
            repaired.status = ProcessStatus::Waiting;
            changed
        }
        "process.resumed" => {
            let changed = repaired.wait.is_some() || repaired.status != ProcessStatus::Running;
            repaired.wait = None;
            repaired.status = ProcessStatus::Running;
            changed
        }
        _ => false,
    };
    Ok(changed.then_some(repaired))
}

pub fn prepare_process_event_append(
    record: &ProcessRecord,
    request: ProcessEventAppendRequest,
    sequence: u64,
    replay_lookup: Option<(String, ProcessEvent)>,
    occurred_at_ms: u64,
    wake_session_id: Option<&str>,
) -> Result<ProcessEventAppendPlan, PluginError> {
    let process_id = record.id.as_str();
    let payload_hash = process_event_payload_hash(&request.event_type, &request.payload)?;
    if let Some(replay_key) = request.replay.as_ref().map(|replay| replay.key.as_str())
        && let Some((existing_hash, existing)) = replay_lookup
    {
        if existing_hash == payload_hash {
            let occurred_at_ms = epoch_ms_from_system_time(existing.occurred_at);
            let repair_record = if existing.sequence.saturating_add(1) == sequence {
                let mut projected = record.clone();
                apply_process_event_projection(&mut projected, &existing)?;
                let projected_value = serde_json::to_value(&projected).map_err(|err| {
                    PluginError::Session(format!(
                        "failed to compare replay projection for process `{process_id}`: {err}"
                    ))
                })?;
                let record_value = serde_json::to_value(record).map_err(|err| {
                    PluginError::Session(format!(
                        "failed to compare stored projection for process `{process_id}`: {err}"
                    ))
                })?;
                (projected_value != record_value).then_some(projected)
            } else if !record.is_terminal() && existing.semantics.terminal.is_some() {
                let mut projected = record.clone();
                let terminal = existing
                    .semantics
                    .terminal
                    .clone()
                    .expect("terminal checked above");
                projected.outcome = Some(terminal.outcome.clone());
                apply_process_status_projection(
                    &mut projected,
                    ProcessStatus::from_terminal(terminal),
                    occurred_at_ms,
                );
                Some(projected)
            } else {
                repair_lifecycle_projection(record, &existing)?
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
            )?;
            return Ok(ProcessEventAppendPlan::Replay {
                event: existing,
                repair_record,
                wake_delivery,
                occurred_at_ms,
            });
        }
        return Err(PluginError::Session(format!(
            "process `{process_id}` event replay key `{replay_key}` conflicts with an existing event"
        )));
    }
    if record.is_terminal()
        && super::events::process_signal_name_from_event_type(&request.event_type).is_some()
    {
        return Err(PluginError::ProcessAlreadyTerminal {
            process_id: process_id.to_string(),
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
    let semantics = materialize_process_event_semantics(
        process_id,
        sequence,
        &request.payload,
        &declared.semantics,
    )?;
    if semantics.terminal.is_some() && record.is_terminal() {
        return Err(PluginError::ProcessAlreadyTerminal {
            process_id: process_id.to_string(),
            status: record.status,
        });
    }
    let occurred_at = system_time_from_epoch_ms(occurred_at_ms);
    let event = ProcessEvent {
        process_id: process_id.to_string(),
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
        occurred_at,
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
    )?;
    Ok(ProcessEventAppendPlan::Insert {
        event,
        payload_hash,
        projected_record,
        wake_delivery,
        occurred_at_ms,
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "wake delivery mirrors the persisted event plus its optional materialized wake"
)]
fn prepare_wake_delivery(
    process_id: &str,
    record: &ProcessRecord,
    sequence: u64,
    event_type: String,
    event_invocation: crate::RuntimeInvocation,
    occurred_at: std::time::SystemTime,
    wake: Option<super::events::ProcessWake>,
    wake_session_id: Option<&str>,
) -> Result<Option<ProcessWakeDelivery>, PluginError> {
    let Some(wake) = wake else {
        return Ok(None);
    };
    let Some(target_session_id) = wake_session_id else {
        return Ok(None);
    };
    process_wake_delivery(ProcessWakeDeliveryRequest {
        target_session_id: target_session_id.to_string(),
        process_id: process_id.to_string(),
        sequence,
        event_type,
        event_invocation,
        process_caused_by: record.provenance.caused_by.clone(),
        wake,
        occurred_at,
    })
    .map(Some)
}

pub fn prepare_process_registration(
    mut registration: ProcessRegistration,
) -> Result<(ProcessRegistration, String), PluginError> {
    validate_process_registration(&registration)?;
    ensure_core_event_types(&mut registration);
    let registration_hash = process_registration_hash(&registration)?;
    registration
        .event_types
        .retain(|event_type| !is_runtime_lifecycle_event_type(&event_type.name));
    Ok((registration, registration_hash))
}

pub fn process_registration_hash(
    registration: &ProcessRegistration,
) -> Result<String, PluginError> {
    let mut hash_view = registration.clone();
    // These three runtime facts were added after durable registration hashes
    // already existed. Waiting/resumed remain in the hash view because they
    // were part of the base-commit vocabulary.
    hash_view.event_types.retain(|event_type| {
        !matches!(
            event_type.name.as_str(),
            "process.first_started" | "process.external_ref_set" | "process.abandon_requested"
        )
    });
    crate::stable_hash::stable_json_sha256_hex(&hash_view).map_err(|err| {
        PluginError::Session(format!(
            "failed to hash process `{}` registration: {err}",
            registration.id
        ))
    })
}

/// Extend a normalized registration hash with its atomic initial observer set.
///
/// Initial observers participate in start idempotency: replaying a process id
/// with a different visibility set is a conflicting registration.
pub fn process_registration_with_observers_hash(
    registration_hash: String,
    observers: &[SessionId],
) -> Result<String, PluginError> {
    let mut observers = observers.to_vec();
    observers.sort();
    observers.dedup();
    crate::stable_hash::stable_json_sha256_hex(&(registration_hash, observers))
        .map_err(|error| PluginError::Session(error.to_string()))
}

pub fn process_event_payload_hash(
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<String, PluginError> {
    crate::stable_hash::stable_json_sha256_hex(&(event_type, payload)).map_err(|err| {
        PluginError::Session(format!(
            "failed to hash `{event_type}` process event: {err}"
        ))
    })
}

pub fn require_event_replay(
    process_id: &str,
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
                | "process.observer_added"
                | "process.observer_removed"
                | "process.subscription_retargeted"
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

pub(super) fn validate_process_registration(
    registration: &ProcessRegistration,
) -> Result<(), PluginError> {
    if registration.id.trim().is_empty() {
        return Err(PluginError::Session(
            "process id must be a non-empty string".to_string(),
        ));
    }
    if registration.id.contains('#') {
        return Err(PluginError::Session(format!(
            "process id `{}` contains reserved segment separator `#`",
            registration.id
        )));
    }
    if registration.max_attempts == Some(0) {
        return Err(PluginError::Session(format!(
            "process `{}` max_attempts must be greater than zero",
            registration.id
        )));
    }
    match registration.input.as_ref() {
        super::model::ProcessInput::ToolCall { .. } | super::model::ProcessInput::Engine { .. } => {
            if registration.env_ref.is_none() {
                return Err(PluginError::Session(format!(
                    "process `{}` requires a captured execution env",
                    registration.id
                )));
            }
        }
        super::model::ProcessInput::External { .. }
        | super::model::ProcessInput::SessionTurn { .. } => {
            if registration.env_ref.is_some() {
                return Err(PluginError::Session(format!(
                    "process `{}` must not capture an execution env for this input kind",
                    registration.id
                )));
            }
        }
    }
    let mut names = HashSet::new();
    for event_type in &registration.event_types {
        if event_type.name.trim().is_empty() {
            return Err(PluginError::Session(format!(
                "process `{}` declares an empty event type",
                registration.id
            )));
        }
        if !names.insert(event_type.name.as_str()) {
            return Err(PluginError::Session(format!(
                "process `{}` declares duplicate event type `{}`",
                registration.id, event_type.name
            )));
        }
        if let Some(runtime_owned) = runtime_lifecycle_event_type(&event_type.name)
            && event_type != &runtime_owned
        {
            return Err(PluginError::Session(format!(
                "process `{}` declares reserved runtime lifecycle event type `{}`",
                registration.id, event_type.name
            )));
        }
        if let Some(terminal) = &event_type.semantics.terminal {
            if !terminal.status.is_terminal() {
                return Err(PluginError::Session(format!(
                    "terminal event `{}` for process `{}` must declare a terminal status, got `{}`",
                    event_type.name,
                    registration.id,
                    terminal.status.label()
                )));
            }
            if terminal.status != ProcessStatus::Completed && terminal.await_output.is_none() {
                return Err(PluginError::Session(format!(
                    "terminal event `{}` for process `{}` must declare await output",
                    event_type.name, registration.id
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ProcessEventAppendPlan, prepare_process_event_append, prepare_process_registration,
    };
    use crate::{
        AbandonRequest, ProcessEventAppendRequest, ProcessExternalRef, ProcessInput,
        ProcessProvenance, ProcessRecord, ProcessRegistration, ProcessStarted, RecoveryDisposition,
        WaitKind, WaitState,
    };

    fn fixture_registration(id: &str) -> ProcessRegistration {
        ProcessRegistration::new(
            id,
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            RecoveryDisposition::ExternallyOwned,
            ProcessProvenance::host(),
        )
    }

    #[test]
    fn process_id_rejects_reserved_segment_separator() {
        let registration = fixture_registration("foo#1");
        let error = prepare_process_registration(registration)
            .expect_err("segment separator must be rejected");
        assert!(error.to_string().contains("reserved segment separator `#`"));
    }

    #[test]
    fn producer_cannot_override_runtime_lifecycle_event_types() {
        let mut collision =
            super::runtime_lifecycle_event_type("process.waiting").expect("reserved event type");
        collision.semantics.terminal = Some(crate::ProcessTerminalSpec {
            status: crate::ProcessStatus::Completed,
            await_output: None,
        });
        let registration = fixture_registration("reserved-collision").with_event_types([collision]);
        let error = prepare_process_registration(registration)
            .expect_err("reserved lifecycle collision must be rejected");
        assert!(
            error
                .to_string()
                .contains("reserved runtime lifecycle event type `process.waiting`")
        );
    }

    #[test]
    fn terminal_semantics_reject_non_terminal_status() {
        let registration =
            fixture_registration("invalid-terminal-status").with_extra_event_types([
                crate::ProcessEventType {
                    name: "producer.invalid_terminal".to_string(),
                    payload_schema: crate::LashSchema::any(),
                    semantics: crate::ProcessEventSemanticsSpec {
                        terminal: Some(crate::ProcessTerminalSpec {
                            status: crate::ProcessStatus::Running,
                            await_output: Some(crate::ProcessValueSelector::Payload),
                        }),
                        ..crate::ProcessEventSemanticsSpec::default()
                    },
                },
            ]);
        let error = prepare_process_registration(registration)
            .expect_err("non-terminal status must be rejected at registration");
        assert!(
            error
                .to_string()
                .contains("must declare a terminal status, got `running`")
        );
    }

    #[test]
    fn registration_hash_matches_pre_lifecycle_base_commit() {
        let (_, hash) =
            prepare_process_registration(fixture_registration("registration-hash-fixture"))
                .expect("prepare fixture registration");
        assert_eq!(
            hash,
            "57ac2c81468fdcfeb96e9cfed091fa61ad8670560a428469fd667cc808500563"
        );
    }

    #[test]
    fn persisted_record_without_lifecycle_declarations_accepts_runtime_events() {
        let (registration, registration_hash) =
            prepare_process_registration(fixture_registration("pre-upgrade-record"))
                .expect("prepare pre-upgrade fixture");
        assert!(
            registration
                .event_types
                .iter()
                .all(|event_type| !super::is_runtime_lifecycle_event_type(&event_type.name)),
            "runtime lifecycle types must not be persisted as producer declarations"
        );
        let encoded = serde_json::to_vec(&ProcessRecord::from_prepared_registration(
            registration,
            registration_hash,
            1,
        ))
        .expect("encode pre-upgrade row");
        let mut record: ProcessRecord =
            serde_json::from_slice(&encoded).expect("decode pre-upgrade row");
        let wait = WaitState {
            kind: WaitKind::Signal {
                name: "ready".to_string(),
                event_type: "signal.ready".to_string(),
                key: "process:pre-upgrade-record:signal.ready:1".to_string(),
                ordinal: 1,
            },
            since_ms: 2,
        };
        let requests = [
            ProcessEventAppendRequest::first_started(
                &record.id,
                &ProcessStarted {
                    owner: crate::LeaseOwnerIdentity::opaque("owner", "incarnation"),
                    fencing_token: 0,
                    attempt: 1,
                    started_at_ms: 2,
                },
                false,
            ),
            ProcessEventAppendRequest::wait_entered(&record.id, &wait),
            ProcessEventAppendRequest::wait_cleared(&record.id, &wait),
            ProcessEventAppendRequest::external_ref_set(
                &record.id,
                &ProcessExternalRef {
                    backend: "fixture".to_string(),
                    id: "external".to_string(),
                    metadata: None,
                },
            ),
            ProcessEventAppendRequest::abandon_requested(
                &record.id,
                &AbandonRequest {
                    requested_by: "fixture".to_string(),
                    requested_at_ms: 3,
                    reason: None,
                },
            ),
        ];
        for (index, request) in requests.into_iter().enumerate() {
            let sequence = index as u64 + 1;
            let plan =
                prepare_process_event_append(&record, request, sequence, None, sequence + 10, None)
                    .expect("runtime-owned lifecycle append must validate");
            let ProcessEventAppendPlan::Insert {
                projected_record, ..
            } = plan
            else {
                panic!("unique lifecycle fixture must insert")
            };
            record = projected_record;
        }
        assert!(record.first_started.is_some());
        assert!(record.wait.is_none());
        assert!(record.external_ref.is_some());
        assert!(record.abandon_request.is_some());
    }
}
