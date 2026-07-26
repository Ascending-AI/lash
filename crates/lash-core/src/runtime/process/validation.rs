use std::collections::HashSet;

use crate::plugin::PluginError;

use super::events::{
    ProcessEvent, ProcessEventAppendRequest, ProcessEventSemanticsSpec, ProcessTerminalState,
    ProcessWakeDelivery, default_process_event_types,
};
use super::materialization::materialize_process_event_semantics;
use super::model::{ProcessRecord, ProcessRegistration, ProcessStatus};
use super::time::{epoch_ms_from_system_time, system_time_from_epoch_ms};
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
            match record.first_started.as_deref() {
                None => record.first_started = Some(Box::new(started)),
                Some(existing) if existing == &started => {}
                Some(_) => {
                    return Err(PluginError::Session(format!(
                        "process `{}` already has a different first-started fact",
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
        }
        "process.resumed" => {
            record.wait = None;
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

fn repair_monotonic_lifecycle_projection(
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
                (serde_json::to_value(&projected).ok() != serde_json::to_value(record).ok())
                    .then_some(projected)
            } else if !record.is_terminal() && existing.semantics.terminal.is_some() {
                let mut projected = record.clone();
                apply_process_status_projection(
                    &mut projected,
                    ProcessStatus::from_terminal(
                        existing
                            .semantics
                            .terminal
                            .clone()
                            .expect("terminal checked above"),
                    ),
                    occurred_at_ms,
                );
                Some(projected)
            } else {
                repair_monotonic_lifecycle_projection(record, &existing)?
            };
            let wake_delivery = prepare_wake_delivery(
                process_id,
                record,
                existing.sequence,
                existing.event_type.clone(),
                existing.invocation.clone(),
                existing.occurred_at,
                existing.semantics.wake.clone(),
                request
                    .wake_target_scope
                    .clone()
                    .or_else(|| record.wake_target.clone()),
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
    let declared = record
        .event_types
        .iter()
        .find(|declared| declared.name == request.event_type)
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
        return Err(PluginError::Session(format!(
            "process `{process_id}` is already terminal"
        )));
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
        request
            .wake_target_scope
            .or_else(|| record.wake_target.clone()),
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
    wake_target_scope: Option<super::model::SessionScope>,
) -> Result<Option<ProcessWakeDelivery>, PluginError> {
    let Some(wake) = wake else {
        return Ok(None);
    };
    let Some(target_scope) = wake_target_scope else {
        return Ok(None);
    };
    process_wake_delivery(ProcessWakeDeliveryRequest {
        target_scope,
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
    ensure_core_event_types(&mut registration);
    validate_process_registration(&registration)?;
    let registration_hash = process_registration_hash(&registration)?;
    Ok((registration, registration_hash))
}

pub fn process_registration_hash(
    registration: &ProcessRegistration,
) -> Result<String, PluginError> {
    crate::stable_hash::stable_json_sha256_hex(registration).map_err(|err| {
        PluginError::Session(format!(
            "failed to hash process `{}` registration: {err}",
            registration.id
        ))
    })
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
    for event_type in default_process_event_types() {
        if let Some(existing) = registration
            .event_types
            .iter_mut()
            .find(|existing| existing.name == event_type.name)
        {
            *existing = event_type;
        } else {
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
        if let Some(terminal) = &event_type.semantics.terminal
            && terminal.state != ProcessTerminalState::Completed
            && terminal.await_output.is_none()
        {
            return Err(PluginError::Session(format!(
                "terminal event `{}` for process `{}` must declare await output",
                event_type.name, registration.id
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::prepare_process_registration;
    use crate::{ProcessInput, ProcessProvenance, ProcessRegistration, RecoveryDisposition};

    #[test]
    fn process_id_rejects_reserved_segment_separator() {
        let registration = ProcessRegistration::new(
            "foo#1",
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            RecoveryDisposition::ExternallyOwned,
            ProcessProvenance::host(),
        );
        let error = prepare_process_registration(registration)
            .expect_err("segment separator must be rejected");
        assert!(error.to_string().contains("reserved segment separator `#`"));
    }
}
