use std::collections::{HashMap, HashSet};

use crate::SessionId;
use crate::plugin::PluginError;

use super::events::{
    ProcessEvent, ProcessEventAppendRequest, ProcessEventSemanticsSpec, ProcessWakeDelivery,
    default_process_event_types, is_runtime_lifecycle_event_type, runtime_lifecycle_event_type,
};
use super::materialization::materialize_process_event_semantics;
use super::model::{
    ProcessRecord, ProcessRegistration, ProcessStarted, ProcessStatus, RecoveryContract,
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
    authority.validate_resume_predecessor(record.id.as_str(), record.first_started.as_deref())?;

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
        "process.waiting" => {
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
        "process.resumed" => {
            if record.status == ProcessStatus::CallerDeparted {
                return Err(PluginError::Session(format!(
                    "caller-departed process `{}` cannot resume",
                    record.id
                )));
            }
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
        "process.caller_departed" => {
            apply_caller_departure(record)?;
        }
        _ => {}
    }

    if let Some(terminal) = event.semantics.terminal.clone() {
        if record.is_terminal() {
            return Ok(());
        }
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
    wake_session_id: Option<&str>,
) -> Result<ProcessEventAppendPlan, PluginError> {
    let process_id = record.id.as_str();
    if let Some(replay_key) = request.replay.as_ref().map(|replay| replay.key.as_str())
        && let Some(existing) = replay_lookup
    {
        if existing.event_type == request.event_type
            && crate::identity_json::payloads_equal(&existing.payload, &request.payload)
        {
            let occurred_at_ms = epoch_ms_from_system_time(existing.occurred_at);
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

const LEGACY_PROCESS_REGISTRATION_FAMILY_VERSION: u8 = 2;
// Bumped to 4 (FIG-1383): the definition preimage's process-status tag registry
// gained `caller_departed`. The versioned-surface guard fails closed on any
// preimage edit, so the family version moves with it even though tag 7 is
// additive and every pre-existing preimage encodes byte-identically.
const PROCESS_REGISTRATION_FAMILY_VERSION: u8 = 4;

fn process_registration_family_version(registration: &ProcessRegistration) -> u8 {
    match registration.input.as_ref() {
        super::model::ProcessInput::ToolCall { call }
            if call
                .replay
                .as_ref()
                .and_then(|replay| replay.origin.as_ref())
                .is_some() =>
        {
            PROCESS_REGISTRATION_FAMILY_VERSION
        }
        _ => LEGACY_PROCESS_REGISTRATION_FAMILY_VERSION,
    }
}

/// Permanent tag registry for the process-registration definition fingerprint.
///
/// Input kinds: 1 tool call, 2 engine, 3 session turn, 4 external. Recovery:
/// 1 rerunnable, 2 owner bound, 3 externally owned. Originators: 1 host,
/// 2 session. Causal refs: 1 turn, 2 effect, 3 tool call, 4 process,
/// 5 process event, 6 trigger occurrence, 7 session node. Arbitrary JSON and
/// schemas are each one canonical opaque bytes leaf. Tool output contracts: 1
/// static, 2 from-input-schema. Value selectors: 1
/// payload, 2 pointer, 3 const, 4 template, 5 present. Process statuses: 1
/// running, 2 waiting, 3 completed, 4 failed, 5 cancelled, 6 abandoned,
/// 7 caller departed. Retired tags remain burned.
fn process_registration_fingerprint_preimage(
    registration: &ProcessRegistration,
    observers: &[SessionId],
) -> Vec<u8> {
    let family_version = process_registration_family_version(registration);
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
        family_version,
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
            project_registration_payload_leaf(&mut fingerprint, args);
            fingerprint.optional(replay.as_ref(), |identity, replay| {
                let lash_sansio::llm::types::ProviderReplayMeta {
                    item_id,
                    opaque,
                    origin,
                } = replay;
                identity.optional(item_id.as_deref(), |identity, value| identity.string(value));
                identity.optional(opaque.as_deref(), |identity, value| identity.string(value));
                if family_version == PROCESS_REGISTRATION_FAMILY_VERSION {
                    identity.optional(origin.as_ref(), crate::stable_identity::provider_route);
                }
            });
            project_registration_payload_leaf(&mut fingerprint, prepared_payload);
        }
        super::model::ProcessInput::Engine { kind, payload } => {
            fingerprint.tag(2);
            fingerprint.string(kind);
            project_registration_payload_leaf(&mut fingerprint, payload);
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
            project_registration_payload_leaf(&mut fingerprint, metadata);
        }
    }
    fingerprint.tag(match disposition {
        super::model::RecoveryContract::Rerunnable => 1,
        super::model::RecoveryContract::OwnerBound => 2,
        super::model::RecoveryContract::ExternallyOwned => 3,
    });
    fingerprint.optional(*max_attempts, crate::stable_identity::IdentityEncoder::u32);

    let super::model::ProcessIdentity {
        kind,
        label,
        definition,
    } = identity;
    fingerprint.string(kind);
    fingerprint.optional(label.as_deref(), |identity, label| identity.string(label));
    fingerprint.optional(definition.as_ref(), project_registration_payload_leaf);

    let super::model::ProcessProvenance {
        originator,
        caused_by,
    } = provenance;
    match originator {
        super::model::ProcessOriginator::Host { scope } => {
            fingerprint.tag(1);
            fingerprint.optional(scope.as_deref(), |identity, scope| identity.string(scope));
        }
        super::model::ProcessOriginator::Session {
            session_id,
            agent_frame_id,
        } => {
            fingerprint.tag(2);
            fingerprint.string(session_id);
            if let Some(agent_frame_id) = agent_frame_id {
                // Preserve the v2 preimage for historical unframed session
                // originators while making a real elevation change conflict.
                fingerprint.tag(3);
                fingerprint.string(agent_frame_id);
            }
        }
    }
    fingerprint.optional(caused_by.as_ref(), project_registration_causal_ref);
    fingerprint.optional(env_ref.as_ref(), |identity, env_ref| {
        identity.string(env_ref.as_str());
    });
    fingerprint.optional(wake_session_id.as_deref(), |identity, session_id| {
        identity.string(session_id);
    });

    // A built-in declaration is excluded only when it is byte-for-byte the
    // default. A caller override of a core name changes executable semantics
    // and must therefore conflict. Source order is not definition-bearing.
    let core_event_types = default_process_event_types()
        .into_iter()
        .map(|event_type| (event_type.name.clone(), event_type))
        .collect::<HashMap<_, _>>();
    let mut application_event_types = event_types
        .iter()
        .filter(|event_type| core_event_types.get(&event_type.name) != Some(event_type))
        .collect::<Vec<_>>();
    application_event_types.sort_by(|left, right| left.name.cmp(&right.name));
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
            identity.optional(default_schema.as_ref(), project_registration_schema_leaf);
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
    project_registration_schema_leaf(identity, schema);
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
            super::model::ProcessStatus::CallerDeparted => 7,
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
            project_registration_payload_leaf(identity, value);
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

fn project_registration_payload_leaf(
    identity: &mut crate::stable_identity::IdentityEncoder,
    value: &serde_json::Value,
) {
    identity.bytes(&crate::identity_json::payload_leaf(value));
}

fn project_registration_schema_leaf(
    identity: &mut crate::stable_identity::IdentityEncoder,
    value: &serde_json::Value,
) {
    identity.bytes(&crate::identity_json::schema_leaf(value));
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
    let family_version = process_registration_family_version(registration);
    let preimage = process_registration_fingerprint_preimage(registration, observers);
    crate::stable_identity::rendered_hash(
        "process-registration-definition",
        family_version,
        &preimage,
    )
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
#[path = "validation_tests.rs"]
mod tests;
