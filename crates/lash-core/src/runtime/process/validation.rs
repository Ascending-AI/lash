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
    last_event_sequence: Option<u64>,
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
            let repair_record = if last_event_sequence == Some(existing.sequence) {
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
) -> Result<ProcessRegistration, PluginError> {
    validate_process_registration(&registration)?;
    ensure_core_event_types(&mut registration);
    registration
        .event_types
        .retain(|event_type| !is_runtime_lifecycle_event_type(&event_type.name));
    Ok(registration)
}

const PROCESS_REGISTRATION_FAMILY_VERSION: u8 = 2;

/// Permanent tag registry for the process-registration definition fingerprint.
///
/// Input kinds: 1 tool call, 2 engine, 3 session turn, 4 external. Recovery:
/// 1 rerunnable, 2 owner bound, 3 externally owned. Originators: 1 host,
/// 2 session. Causal refs: 1 turn, 2 effect, 3 tool call, 4 process,
/// 5 process event, 6 trigger occurrence, 7 session node. JSON values: 1 null,
/// 2 false, 3 true, 4 i64, 5 u64, 6 f64, 7 string, 8 array, 9 object. Tool
/// output contracts: 1 static, 2 from-input-schema. Value selectors: 1
/// payload, 2 pointer, 3 const, 4 template, 5 present. Process statuses: 1
/// running, 2 waiting, 3 completed, 4 failed, 5 cancelled, 6 abandoned.
/// Retired tags remain burned.
fn process_registration_fingerprint_preimage(
    registration: &ProcessRegistration,
    observers: &[SessionId],
) -> Vec<u8> {
    let ProcessRegistration {
        id: _,
        input,
        disposition,
        max_attempts,
        identity,
        event_types,
        provenance,
        env_ref,
        wake_session_id,
    } = registration;
    let mut fingerprint = crate::stable_identity::IdentityEncoder::new(
        "lash.process-registration-definition",
        PROCESS_REGISTRATION_FAMILY_VERSION,
    );

    match input.as_ref() {
        super::model::ProcessInput::ToolCall { call } => {
            let crate::PreparedToolCall {
                call_id,
                tool_id,
                tool_name,
                args,
                replay,
                prepared_payload,
            } = call;
            fingerprint.tag(1);
            fingerprint.string(call_id);
            fingerprint.string(tool_id.as_str());
            fingerprint.string(tool_name);
            project_registration_json_value(&mut fingerprint, args);
            fingerprint.optional(replay.as_ref(), |identity, replay| {
                let lash_sansio::llm::types::ProviderReplayMeta { item_id, opaque } = replay;
                identity.optional(item_id.as_deref(), |identity, value| identity.string(value));
                identity.optional(opaque.as_deref(), |identity, value| identity.string(value));
            });
            project_registration_json_value(&mut fingerprint, prepared_payload);
        }
        super::model::ProcessInput::Engine { kind, payload } => {
            fingerprint.tag(2);
            fingerprint.string(kind);
            project_registration_json_value(&mut fingerprint, payload);
        }
        super::model::ProcessInput::SessionTurn {
            definition_key,
            create_request: _,
            turn_input: _,
            output_contract,
        } => {
            fingerprint.tag(3);
            fingerprint.string(definition_key);
            project_registration_output_contract(&mut fingerprint, output_contract);
        }
        super::model::ProcessInput::External { metadata } => {
            fingerprint.tag(4);
            project_registration_json_value(&mut fingerprint, metadata);
        }
    }
    fingerprint.tag(match disposition {
        super::model::RecoveryDisposition::Rerunnable => 1,
        super::model::RecoveryDisposition::OwnerBound => 2,
        super::model::RecoveryDisposition::ExternallyOwned => 3,
    });
    fingerprint.optional(*max_attempts, crate::stable_identity::IdentityEncoder::u32);

    let super::model::ProcessIdentity {
        kind,
        label,
        definition,
    } = identity;
    fingerprint.string(kind);
    fingerprint.optional(label.as_deref(), |identity, label| identity.string(label));
    fingerprint.optional(definition.as_ref(), project_registration_json_value);

    let super::model::ProcessProvenance {
        originator,
        caused_by,
    } = provenance;
    match originator {
        super::model::ProcessOriginator::Host { scope } => {
            fingerprint.tag(1);
            fingerprint.optional(scope.as_deref(), |identity, scope| identity.string(scope));
        }
        super::model::ProcessOriginator::Session { session_id } => {
            fingerprint.tag(2);
            fingerprint.string(session_id);
        }
    }
    fingerprint.optional(caused_by.as_ref(), project_registration_causal_ref);
    fingerprint.optional(env_ref.as_ref(), |identity, env_ref| {
        identity.string(env_ref.as_str());
    });
    fingerprint.optional(wake_session_id.as_deref(), |identity, session_id| {
        identity.string(session_id);
    });

    // Runtime lifecycle declarations never participate. They are core-owned
    // vocabulary, not caller definition, so adding one cannot rotate a stored
    // process fingerprint again. Application declarations are projected in
    // full because schema and terminal/wake semantics are executable
    // definition, not display metadata.
    let core_event_names = default_process_event_types()
        .into_iter()
        .map(|event_type| event_type.name)
        .collect::<HashSet<_>>();
    let application_event_types = event_types
        .iter()
        .filter(|event_type| !core_event_names.contains(&event_type.name))
        .collect::<Vec<_>>();
    fingerprint.sequence(
        application_event_types.iter().copied(),
        |identity, event_type| {
            project_registration_event_type(identity, event_type);
        },
    );

    let mut observers = observers.to_vec();
    observers.sort();
    observers.dedup();
    fingerprint.sequence(observers.iter(), |identity, observer| {
        identity.string(observer);
    });
    fingerprint.finish()
}

fn project_registration_output_contract(
    identity: &mut crate::stable_identity::IdentityEncoder,
    contract: &crate::ToolOutputContract,
) {
    match contract {
        crate::ToolOutputContract::Static => identity.tag(1),
        crate::ToolOutputContract::FromInputSchema {
            input_field,
            default_schema,
        } => {
            identity.tag(2);
            identity.string(input_field);
            identity.optional(default_schema.as_ref(), project_registration_json_value);
        }
    }
}

fn project_registration_event_type(
    identity: &mut crate::stable_identity::IdentityEncoder,
    event_type: &super::events::ProcessEventType,
) {
    let super::events::ProcessEventType {
        name,
        payload_schema,
        semantics,
    } = event_type;
    identity.string(name);
    let crate::LashSchema { schema } = payload_schema;
    project_registration_json_value(identity, schema);
    let super::events::ProcessEventSemanticsSpec { terminal, wake } = semantics;
    identity.optional(terminal.as_ref(), |identity, terminal| {
        let super::events::ProcessTerminalSpec {
            status,
            await_output,
        } = terminal;
        identity.tag(match status {
            super::model::ProcessStatus::Running => 1,
            super::model::ProcessStatus::Waiting => 2,
            super::model::ProcessStatus::Completed => 3,
            super::model::ProcessStatus::Failed => 4,
            super::model::ProcessStatus::Cancelled => 5,
            super::model::ProcessStatus::Abandoned => 6,
        });
        identity.optional(await_output.as_ref(), project_registration_value_selector);
    });
    identity.optional(wake.as_ref(), |identity, wake| {
        let super::events::ProcessWakeSpec { when, input } = wake;
        identity.optional(when.as_ref(), project_registration_value_selector);
        project_registration_value_selector(identity, input);
    });
}

fn project_registration_value_selector(
    identity: &mut crate::stable_identity::IdentityEncoder,
    selector: &super::events::ProcessValueSelector,
) {
    match selector {
        super::events::ProcessValueSelector::Payload => identity.tag(1),
        super::events::ProcessValueSelector::Pointer(pointer) => {
            identity.tag(2);
            identity.string(pointer);
        }
        super::events::ProcessValueSelector::Const(value) => {
            identity.tag(3);
            project_registration_json_value(identity, value);
        }
        super::events::ProcessValueSelector::Template { template, fields } => {
            identity.tag(4);
            identity.string(template);
            identity.sequence(fields.iter(), |identity, (name, selector)| {
                identity.string(name);
                project_registration_value_selector(identity, selector);
            });
        }
        super::events::ProcessValueSelector::Present(pointer) => {
            identity.tag(5);
            identity.string(pointer);
        }
    }
}

fn project_registration_causal_ref(
    identity: &mut crate::stable_identity::IdentityEncoder,
    caused_by: &crate::CausalRef,
) {
    match caused_by {
        crate::CausalRef::Turn {
            session_id,
            turn_id,
        } => {
            identity.tag(1);
            identity.string(session_id);
            identity.string(turn_id);
        }
        crate::CausalRef::Effect {
            session_id,
            turn_id,
            effect_id,
        } => {
            identity.tag(2);
            identity.string(session_id);
            identity.optional(turn_id.as_deref(), |identity, turn_id| {
                identity.string(turn_id)
            });
            identity.string(effect_id);
        }
        crate::CausalRef::ToolCall {
            session_id,
            call_id,
        } => {
            identity.tag(3);
            identity.string(session_id);
            identity.string(call_id);
        }
        crate::CausalRef::Process { process_id } => {
            identity.tag(4);
            identity.string(process_id);
        }
        crate::CausalRef::ProcessEvent {
            process_id,
            sequence,
        } => {
            identity.tag(5);
            identity.string(process_id);
            identity.u64(*sequence);
        }
        crate::CausalRef::TriggerOccurrence {
            occurrence_id,
            subscription_id,
            subscription_incarnation,
            subscription_revision,
        } => {
            identity.tag(6);
            identity.string(occurrence_id);
            identity.optional(subscription_id.as_deref(), |identity, value| {
                identity.string(value)
            });
            identity.optional(subscription_incarnation.as_deref(), |identity, value| {
                identity.string(value);
            });
            identity.optional(
                *subscription_revision,
                crate::stable_identity::IdentityEncoder::u64,
            );
        }
        crate::CausalRef::SessionNode {
            session_id,
            node_id,
        } => {
            identity.tag(7);
            identity.string(session_id);
            identity.string(node_id);
        }
    }
}

fn project_registration_json_value(
    identity: &mut crate::stable_identity::IdentityEncoder,
    value: &serde_json::Value,
) {
    match value {
        serde_json::Value::Null => identity.tag(1),
        serde_json::Value::Bool(false) => identity.tag(2),
        serde_json::Value::Bool(true) => identity.tag(3),
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                identity.tag(4);
                identity.u64(value as u64);
            } else if let Some(value) = number.as_u64() {
                identity.tag(5);
                identity.u64(value);
            } else {
                identity.tag(6);
                identity.u64(
                    number
                        .as_f64()
                        .expect("serde_json numbers are i64, u64, or finite f64")
                        .to_bits(),
                );
            }
        }
        serde_json::Value::String(value) => {
            identity.tag(7);
            identity.string(value);
        }
        serde_json::Value::Array(values) => {
            identity.tag(8);
            identity.sequence(values, project_registration_json_value);
        }
        serde_json::Value::Object(values) => {
            identity.tag(9);
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| *key);
            identity.sequence(entries, |identity, (key, value)| {
                identity.string(key);
                project_registration_json_value(identity, value);
            });
        }
    }
}

/// Fingerprint the normalized registration definition plus its atomic initial
/// observer set. The process id remains the independent lookup address; this
/// separately versioned value is compared only after that lookup succeeds.
/// Version 2 is a reject-and-recreate cutover coordinated by the Lash store
/// schema versions. External process registries must apply the same lifecycle
/// policy before accepting v2 fingerprints.
///
/// Initial observers participate in start idempotency: replaying a process id
/// with a different visibility set is a conflicting registration.
pub fn process_registration_fingerprint(
    registration: &ProcessRegistration,
    observers: &[SessionId],
) -> String {
    let preimage = process_registration_fingerprint_preimage(registration, observers);
    crate::stable_identity::rendered_hash(
        "process-registration-definition",
        PROCESS_REGISTRATION_FAMILY_VERSION,
        &preimage,
    )
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
        super::model::ProcessInput::External { .. } => {
            if registration.env_ref.is_some() {
                return Err(PluginError::Session(format!(
                    "process `{}` must not capture an execution env for this input kind",
                    registration.id
                )));
            }
        }
        super::model::ProcessInput::SessionTurn { definition_key, .. } => {
            if definition_key.trim().is_empty() {
                return Err(PluginError::Session(format!(
                    "process `{}` session-turn definition_key must not be empty",
                    registration.id
                )));
            }
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
        process_registration_fingerprint,
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

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn registration_for_input(input: ProcessInput) -> ProcessRegistration {
        ProcessRegistration::new(
            "lookup-id-is-not-in-the-fingerprint",
            input,
            RecoveryDisposition::Rerunnable,
            ProcessProvenance::host(),
        )
    }

    #[test]
    fn process_registration_identity_golden_corpus() {
        let inputs = [
            ProcessInput::ToolCall {
                call: crate::PreparedToolCall::from_parts(
                    "call",
                    crate::ToolId::new("tool-id"),
                    "tool",
                    serde_json::json!({"ignored": true}),
                    None,
                    serde_json::Value::Null,
                ),
            },
            ProcessInput::Engine {
                kind: "engine".to_string(),
                payload: serde_json::json!({"ignored": true}),
            },
            ProcessInput::SessionTurn {
                definition_key: "registration-golden-session-turn:v1".to_string(),
                create_request: Box::new(
                    crate::SessionCreateRequest::root(
                        crate::SessionStartPoint::Empty,
                        crate::PluginOptions::default(),
                    )
                    .with_session_id("child"),
                ),
                turn_input: Box::new(crate::TurnInput::empty()),
                output_contract: crate::ToolOutputContract::Static,
            },
            ProcessInput::SessionTurn {
                definition_key: "registration-golden-dynamic-session-turn:v1".to_string(),
                create_request: Box::new(
                    crate::SessionCreateRequest::root(
                        crate::SessionStartPoint::Empty,
                        crate::PluginOptions::default(),
                    )
                    .with_session_id("dynamic-child"),
                ),
                turn_input: Box::new(crate::TurnInput::empty()),
                output_contract: crate::ToolOutputContract::from_input_schema(
                    "result_schema",
                    Some(serde_json::json!({"type": "object"})),
                ),
            },
            ProcessInput::External {
                metadata: serde_json::json!({"ignored": true}),
            },
        ];
        let causes = [
            crate::CausalRef::Turn {
                session_id: "s".to_string(),
                turn_id: "t".to_string(),
            },
            crate::CausalRef::Effect {
                session_id: "s".to_string(),
                turn_id: None,
                effect_id: "e".to_string(),
            },
            crate::CausalRef::ToolCall {
                session_id: "s".to_string(),
                call_id: "c".to_string(),
            },
            crate::CausalRef::Process {
                process_id: "p".to_string(),
            },
            crate::CausalRef::ProcessEvent {
                process_id: "p".to_string(),
                sequence: 0,
            },
            crate::CausalRef::TriggerOccurrence {
                occurrence_id: "o".to_string(),
                subscription_id: Some("s".to_string()),
                subscription_incarnation: None,
                subscription_revision: Some(0),
            },
            crate::CausalRef::SessionNode {
                session_id: "s".to_string(),
                node_id: "n".to_string(),
            },
        ];
        let mut registrations = inputs
            .into_iter()
            .enumerate()
            .map(|(index, input)| {
                let mut registration = registration_for_input(input);
                registration.disposition = match index {
                    0 => RecoveryDisposition::Rerunnable,
                    1 => RecoveryDisposition::OwnerBound,
                    _ => RecoveryDisposition::ExternallyOwned,
                };
                registration
            })
            .collect::<Vec<_>>();
        registrations.extend(causes.into_iter().map(|cause| {
            let mut registration = registration_for_input(ProcessInput::External {
                metadata: serde_json::Value::Null,
            });
            registration.provenance.caused_by = Some(cause);
            registration
        }));
        let mut enriched = registration_for_input(ProcessInput::External {
            metadata: serde_json::Value::Null,
        });
        enriched.max_attempts = Some(0);
        enriched.identity = crate::ProcessIdentity::new("kind")
            .with_label(Some("a:b"))
            .with_definition(Some(serde_json::json!([
                null, false, true, -1, 0, u64::MAX, 1.5, "a:b", [], {"x": 0}
            ])));
        enriched.provenance.originator = crate::ProcessOriginator::session(
            crate::SessionScope::for_agent_frame("session", "frame"),
        );
        enriched.env_ref = Some(crate::ProcessExecutionEnvRef::new("env"));
        enriched.wake_session_id = Some("wake".to_string());
        let mut selector_fields = std::collections::BTreeMap::new();
        selector_fields.insert(
            "const".to_string(),
            crate::ProcessValueSelector::Const(serde_json::json!(0)),
        );
        selector_fields.insert("payload".to_string(), crate::ProcessValueSelector::Payload);
        selector_fields.insert(
            "pointer".to_string(),
            crate::ProcessValueSelector::Pointer("/x".to_string()),
        );
        selector_fields.insert(
            "present".to_string(),
            crate::ProcessValueSelector::Present("/y".to_string()),
        );
        enriched.event_types = vec![crate::ProcessEventType {
            name: "app.event".to_string(),
            payload_schema: crate::LashSchema::new(serde_json::json!({"type": "object"})),
            semantics: crate::ProcessEventSemanticsSpec {
                terminal: Some(crate::ProcessTerminalSpec {
                    status: crate::ProcessStatus::Completed,
                    await_output: Some(crate::ProcessValueSelector::Template {
                        template: "{payload}:{pointer}:{const}:{present}".to_string(),
                        fields: selector_fields,
                    }),
                }),
                wake: Some(crate::ProcessWakeSpec {
                    when: None,
                    input: crate::ProcessValueSelector::Payload,
                }),
            },
        }];
        registrations.push(enriched);

        let mut terminal_statuses = registration_for_input(ProcessInput::External {
            metadata: serde_json::Value::Null,
        });
        terminal_statuses.event_types = [
            crate::ProcessStatus::Running,
            crate::ProcessStatus::Waiting,
            crate::ProcessStatus::Completed,
            crate::ProcessStatus::Failed,
            crate::ProcessStatus::Cancelled,
            crate::ProcessStatus::Abandoned,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, status)| crate::ProcessEventType {
            name: format!("status.{index}"),
            payload_schema: crate::LashSchema::new(serde_json::Value::Bool(true)),
            semantics: crate::ProcessEventSemanticsSpec {
                terminal: Some(crate::ProcessTerminalSpec {
                    status,
                    await_output: (status != crate::ProcessStatus::Completed)
                        .then_some(crate::ProcessValueSelector::Payload),
                }),
                wake: None,
            },
        })
        .collect();
        registrations.push(terminal_statuses);

        let actual = registrations
            .iter()
            .map(|registration| {
                let observers = ["ab".to_string(), "a".to_string(), "ab".to_string()];
                (
                    hex(&super::process_registration_fingerprint_preimage(
                        registration,
                        &observers,
                    )),
                    process_registration_fingerprint(registration, &observers),
                )
            })
            .collect::<Vec<_>>();
        let expected = [
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e01000000000000000463616c6c0000000000000007746f6f6c2d69640000000000000004746f6f6c090000000000000001000000000000000769676e6f72656403000101000000000000000004746f6f6c010000000000000004746f6f6c0001000000000000000000000000000000000000000200000000000000016100000000000000026162",
                "process-registration-definition:v2:sha256:2d8acec808001b06a4c78a7acfa638e40e2d076c8834122516d2e9c44247ce29",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e020000000000000006656e67696e65090000000000000001000000000000000769676e6f7265640302000000000000000006656e67696e65000001000000000000000000000000000000000000000200000000000000016100000000000000026162",
                "process-registration-definition:v2:sha256:6fe2e83910adb766ce57c2bb8c854a723a741d52ae67f878886fd64772c2e178",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e030000000000000023726567697374726174696f6e2d676f6c64656e2d73657373696f6e2d7475726e3a7631010300000000000000000c73657373696f6e5f7475726e0100000000000000056368696c640001000000000000000000000000000000000000000200000000000000016100000000000000026162",
                "process-registration-definition:v2:sha256:e3fb715f1bdb9b700378a7338ca748223a11b9e931f89af669e6df1b1499ffb7",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e03000000000000002b726567697374726174696f6e2d676f6c64656e2d64796e616d69632d73657373696f6e2d7475726e3a763102000000000000000d726573756c745f736368656d61010900000000000000010000000000000004747970650700000000000000066f626a6563740300000000000000000c73657373696f6e5f7475726e01000000000000000d64796e616d69632d6368696c640001000000000000000000000000000000000000000200000000000000016100000000000000026162",
                "process-registration-definition:v2:sha256:6bd6e3774e404725b6aa0ab8a9fd00e236f020485acad0f5e6f4823532936276",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e04090000000000000001000000000000000769676e6f726564030300000000000000000865787465726e616c000001000000000000000000000000000000000000000200000000000000016100000000000000026162",
                "process-registration-definition:v2:sha256:ed70558618eac147a173ca4222556d16a50faeea924a0cc828cae2a096d7c5ec",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e04010100000000000000000865787465726e616c00000100010100000000000000017300000000000000017400000000000000000000000000000000000200000000000000016100000000000000026162",
                "process-registration-definition:v2:sha256:11cbe26f30606f383330b139ee977ae40ee2ca5e5e10f2609a42471e9e1323b9",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e04010100000000000000000865787465726e616c0000010001020000000000000001730000000000000000016500000000000000000000000000000000000200000000000000016100000000000000026162",
                "process-registration-definition:v2:sha256:f57777ddfe718ecc96df8aaa5dd1c2c6a8773871f5824ad0d780211c51b348d1",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e04010100000000000000000865787465726e616c00000100010300000000000000017300000000000000016300000000000000000000000000000000000200000000000000016100000000000000026162",
                "process-registration-definition:v2:sha256:295644f7eec5decf5801a0caecf84c555a10abe1a2307a81119d058bd20365f3",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e04010100000000000000000865787465726e616c00000100010400000000000000017000000000000000000000000000000000000200000000000000016100000000000000026162",
                "process-registration-definition:v2:sha256:f8b458311fc88fe78951ce78c8312d34e51e245a41318da203e8644fad248b94",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e04010100000000000000000865787465726e616c000001000105000000000000000170000000000000000000000000000000000000000000000000000200000000000000016100000000000000026162",
                "process-registration-definition:v2:sha256:40ec80a3460947e2895a6260c926dca4b7f77450129d22ecb1a801fcaf251eed",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e04010100000000000000000865787465726e616c00000100010600000000000000016f010000000000000001730001000000000000000000000000000000000000000000000000000200000000000000016100000000000000026162",
                "process-registration-definition:v2:sha256:b0ae2dbbad25922c859e97f36d57971d3b0749fb4a9275773ab2e07c9b485d26",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e04010100000000000000000865787465726e616c00000100010700000000000000017300000000000000016e00000000000000000000000000000000000200000000000000016100000000000000026162",
                "process-registration-definition:v2:sha256:0470e29540550531b9df98a6e5c9923768c9c048d919cb8b9421ada78ee6d936",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e040101010000000000000000000000046b696e64010000000000000003613a620108000000000000000a01020304ffffffffffffffff04000000000000000005ffffffffffffffff063ff8000000000000070000000000000003613a6208000000000000000009000000000000000100000000000000017804000000000000000002000000000000000773657373696f6e00010000000000000003656e7601000000000000000477616b65000000000000000100000000000000096170702e6576656e740900000000000000010000000000000004747970650700000000000000066f626a6563740103010400000000000000257b7061796c6f61647d3a7b706f696e7465727d3a7b636f6e73747d3a7b70726573656e747d00000000000000040000000000000005636f6e73740304000000000000000000000000000000077061796c6f6164010000000000000007706f696e7465720200000000000000022f78000000000000000770726573656e740500000000000000022f79010001000000000000000200000000000000016100000000000000026162",
                "process-registration-definition:v2:sha256:b16178c141a06ba065fd4c8a02a9d72708be090a84d738be8840d1edf78c537d",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e04010100000000000000000865787465726e616c00000100000000000000000000000600000000000000087374617475732e3003010101010000000000000000087374617475732e3103010201010000000000000000087374617475732e32030103000000000000000000087374617475732e3303010401010000000000000000087374617475732e3403010501010000000000000000087374617475732e35030106010100000000000000000200000000000000016100000000000000026162",
                "process-registration-definition:v2:sha256:7c6f4b84fffb087c42da0996120fe40443c0363af31443531c22df7a48658938",
            ),
        ];
        assert_eq!(actual.len(), expected.len());
        for ((preimage, key), (expected_preimage, expected_key)) in actual.iter().zip(expected) {
            assert_eq!(preimage, expected_preimage);
            assert_eq!(key, expected_key);
        }
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
    fn lookup_id_and_core_event_vocabulary_are_outside_registration_fingerprint() {
        let with_core_events = fixture_registration("first-lookup-id");
        let mut without_core_events = fixture_registration("second-lookup-id");
        without_core_events.event_types.clear();
        assert_eq!(
            process_registration_fingerprint(&with_core_events, &[]),
            process_registration_fingerprint(&without_core_events, &[])
        );
    }

    #[test]
    fn executable_registration_changes_rotate_the_definition_fingerprint() {
        let base = registration_for_input(ProcessInput::Engine {
            kind: "engine".to_string(),
            payload: serde_json::json!({"revision": 1}),
        });
        let changed_input = registration_for_input(ProcessInput::Engine {
            kind: "engine".to_string(),
            payload: serde_json::json!({"revision": 2}),
        });
        assert_ne!(
            process_registration_fingerprint(&base, &[]),
            process_registration_fingerprint(&changed_input, &[])
        );

        let mut changed_event = base.clone();
        changed_event.event_types = vec![crate::ProcessEventType {
            name: "app.event".to_string(),
            payload_schema: crate::LashSchema::new(serde_json::json!({"type": "string"})),
            semantics: crate::ProcessEventSemanticsSpec::default(),
        }];
        let mut other_event = changed_event.clone();
        other_event.event_types[0].payload_schema =
            crate::LashSchema::new(serde_json::json!({"type": "number"}));
        assert_ne!(
            process_registration_fingerprint(&changed_event, &[]),
            process_registration_fingerprint(&other_event, &[])
        );
    }

    #[test]
    fn persisted_record_without_lifecycle_declarations_accepts_runtime_events() {
        let registration = prepare_process_registration(fixture_registration("pre-upgrade-record"))
            .expect("prepare pre-upgrade fixture");
        let registration_fingerprint = process_registration_fingerprint(&registration, &[]);
        assert!(
            registration
                .event_types
                .iter()
                .all(|event_type| !super::is_runtime_lifecycle_event_type(&event_type.name)),
            "runtime lifecycle types must not be persisted as producer declarations"
        );
        let encoded = serde_json::to_vec(&ProcessRecord::from_prepared_registration(
            registration,
            registration_fingerprint,
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
            let plan = prepare_process_event_append(
                &record,
                request,
                sequence,
                (sequence > 1).then_some(sequence - 1),
                None,
                sequence + 10,
                None,
            )
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
