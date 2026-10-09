use crate::ProcessId;
use lash_sansio::CancelOrigin;

use crate::plugin::PluginError;

use super::events::{
    ProcessEvent, ProcessEventAppendRequest, ProcessLifecycleFact, ProcessTerminal,
};
use super::model::{
    ProcessLifecycleState, ProcessRecord, ProcessRegistration, ProcessStarted, ProcessStatus,
    WaitState,
};

/// A lifecycle append as its process's log stores it: the one fact a replay
/// lookup, a replay comparison, a released-payload digest and the inserted
/// event all see.
///
/// Built only by [`ProcessEventAppendRequest::canonical`], which every store
/// runs first on every append.
#[derive(Clone, Debug, PartialEq)]
pub struct CanonicalProcessEventAppend {
    request: ProcessEventAppendRequest,
    replay_key: String,
}

impl CanonicalProcessEventAppend {
    /// The fact the append stores.
    pub fn fact(&self) -> &ProcessLifecycleFact {
        &self.request.fact
    }

    /// The append's replay key: the key a store looks the event up under.
    pub fn replay_key(&self) -> &str {
        &self.replay_key
    }
}

impl ProcessEventAppendRequest {
    /// Prepare this append against `record` as it stands, before anything
    /// reads or compares it.
    ///
    /// A terminal takes the standing cancel request's origin
    /// ([`ProcessTerminal::with_cancel_origin`]), so the request that wrote
    /// a cancelled terminal is the same fact when it is presented again.
    /// `fleet_format` is the `F` the bound store recorded: an effect summary
    /// fact must be one this fleet writes (FIG-3796).
    ///
    /// # Errors
    ///
    /// A request without a replay key, or an effect summary fact its
    /// vocabulary refuses.
    pub fn canonical(
        mut self,
        record: &ProcessRecord,
        fleet_format: crate::FleetFormat,
    ) -> Result<CanonicalProcessEventAppend, PluginError> {
        let replay_key = self
            .replay
            .as_ref()
            .map(|replay| replay.key.clone())
            .filter(|key| !key.is_empty())
            .ok_or_else(|| {
                PluginError::Session(format!(
                    "process `{}` event `{}` requires a deterministic replay key",
                    record.id,
                    self.kind()
                ))
            })?;
        match &mut self.fact {
            ProcessLifecycleFact::EffectOutcome(occurrence) => {
                occurrence
                    .admit(fleet_format)
                    .map_err(|error| PluginError::Session(error.to_string()))?;
                if replay_key != occurrence.replay_key {
                    return Err(PluginError::Session(
                        "effect outcome payload replay_key must equal the append replay key"
                            .to_string(),
                    ));
                }
            }
            ProcessLifecycleFact::EffectOmissions(omissions) => {
                omissions
                    .admit(fleet_format)
                    .map_err(|error| PluginError::Session(error.to_string()))?;
            }
            ProcessLifecycleFact::Terminal { outcome, .. } => {
                *outcome = outcome.clone().with_cancel_origin(
                    record
                        .cancel_request
                        .as_deref()
                        .map(|request| request.origin),
                );
            }
            _ => {}
        }
        Ok(CanonicalProcessEventAppend {
            request: self,
            replay_key,
        })
    }
}

#[derive(Clone, Debug)]
pub enum ProcessEventAppendPlan {
    Insert {
        event: ProcessEvent,
        projected_record: ProcessRecord,
    },
    Replay {
        event: ProcessEvent,
        repair_record: Option<ProcessRecord>,
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
    SetExternalRef(super::model::ProcessExternalRef),
    /// Record a typed process cancellation request.
    RequestCancel(lash_sansio::CancelRequest),
    /// Enter a durable wait, beside any the process already has.
    EnterWait(WaitState),
    /// End the oldest wait the record lists.
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

/// Allocate the next process-event sequence from the live event tail.
///
/// Event sequences are small ordered identifiers, dense per process.
pub fn allocate_process_event_sequence(last_sequence: Option<u64>) -> Result<u64, PluginError> {
    let sequence = last_sequence
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| PluginError::Session("process event sequence exhausted".to_string()))?;
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
            if record.waits().contains(&wait) {
                return Ok(ProcessTransitionPlan::Unchanged);
            }
            let mut append = ProcessEventAppendRequest::wait_entered(&record.id, &wait);
            if record.is_terminal() {
                route_transition_refusal_to_fold(&mut append)?;
            }
            append
        }
        ProcessTransition::ClearWait => {
            let Some(wait) = record.waits().first() else {
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
    let event_type = append.kind();
    let replay = append.replay.as_mut().ok_or_else(|| {
        PluginError::Session(format!(
            "registry lifecycle transition event `{event_type}` requires a deterministic replay key"
        ))
    })?;
    replay.key.push_str(FOLD_VALIDATION_REPLAY_KEY_SUFFIX);
    Ok(())
}

/// Whether `request`, a cancel, ends `record` as it stands: a start that
/// failed before its process ran anything is cancelled by its own request.
fn start_failed_cancel_ends(record: &ProcessRecord, request: &lash_sansio::CancelRequest) -> bool {
    request.origin == CancelOrigin::StartFailed
        && record.first_started.is_none()
        && record.external_ref.is_none()
}

/// The outcome a start-failed cancel ends its process in.
fn start_failed_outcome() -> ProcessTerminal {
    let cancellation = crate::ToolCancellation::runtime("process start failed before execution")
        .with_origin(CancelOrigin::StartFailed);
    ProcessTerminal::from_tool_output(crate::ToolCallOutput::cancelled(cancellation))
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

    let mut ends: Option<ProcessTerminal> = None;
    match &event.fact {
        ProcessLifecycleFact::Started { started } => match record.first_started.as_deref() {
            None => record.first_started = Some(Box::new(started.clone())),
            Some(existing) if existing.same_execution(started) => {}
            Some(existing) if started.attempt == existing.attempt.saturating_add(1) => {
                record.first_started = Some(Box::new(started.clone()));
            }
            Some(_) => {
                return Err(PluginError::Session(format!(
                    "process `{}` has an invalid execution-started attempt",
                    record.id
                )));
            }
        },
        ProcessLifecycleFact::Waiting { wait } => match &record.lifecycle {
            ProcessLifecycleState::Terminal { .. } => {
                return Err(PluginError::Session(format!(
                    "terminal process `{}` cannot enter a wait state",
                    record.id
                )));
            }
            ProcessLifecycleState::Running { .. } | ProcessLifecycleState::Waiting { .. } => {
                record.lifecycle = record.lifecycle.entering(wait);
            }
        },
        ProcessLifecycleFact::Resumed { wait } => match &record.lifecycle {
            // An ended process stays ended: no later fact takes its outcome
            // back, so a resume cannot return it to running.
            ProcessLifecycleState::Terminal { .. } => {
                return Err(PluginError::ProcessAlreadyTerminal {
                    process_id: record.id.clone(),
                    status: record.status(),
                });
            }
            ProcessLifecycleState::Running { .. } | ProcessLifecycleState::Waiting { .. } => {
                record.lifecycle = record.lifecycle.leaving(wait);
            }
        },
        ProcessLifecycleFact::ExternalRefSet { external_ref } => {
            // Compare-and-set on the segment ordinal, never last-write-wins.
            // A live host and the recovery pass may both submit the same
            // segment and mint different backend identities for it; the first
            // recorded one stays, because both run the same coalesced work.
            // Only a strictly later segment names a new owner, and a reference
            // for an earlier segment is a stale writer that must not displace
            // it.
            match record.external_ref.as_ref() {
                None => record.external_ref = Some(external_ref.clone()),
                Some(existing) if existing == external_ref => {}
                // Two backends claiming one row is a model error at every
                // ordinal: a row has exactly one durable owner substrate, so
                // this is checked before the ordinal comparison — a later
                // segment never licenses a change of substrate.
                Some(existing) if existing.backend != external_ref.backend => {
                    return Err(process_external_ref_conflict(
                        &record.id,
                        existing,
                        external_ref,
                    ));
                }
                Some(existing) if external_ref.supersedes(existing) => {
                    record.external_ref = Some(external_ref.clone());
                }
                Some(_) => {}
            }
        }
        ProcessLifecycleFact::CancelRequested(request) => {
            // Replaying the stored start-failed cancel must repair its own
            // fold even though that same event made this record terminal.
            let own_terminal_replay = record.status() == ProcessStatus::Cancelled
                && record.last_event_sequence == event.sequence
                && request.origin == CancelOrigin::StartFailed;
            if record.is_terminal() && !own_terminal_replay {
                return Err(PluginError::ProcessAlreadyTerminal {
                    process_id: record.id.clone(),
                    status: record.status(),
                });
            }
            if !record.is_terminal() && start_failed_cancel_ends(record, request) {
                ends = Some(start_failed_outcome());
            }
            match record.cancel_request.as_deref() {
                None => record.cancel_request = Some(Box::new(request.clone())),
                Some(existing) if existing.same_cancellation_as(request) => {}
                Some(existing) => {
                    return Err(PluginError::ProcessCancelConflict {
                        process_id: record.id.clone(),
                        existing: Box::new(existing.clone()),
                        requested: Box::new(request.clone()),
                    });
                }
            }
        }
        ProcessLifecycleFact::Terminal { outcome, .. } => ends = Some(outcome.clone()),
        ProcessLifecycleFact::ObserverAdded { .. }
        | ProcessLifecycleFact::ObserverRemoved { .. }
        | ProcessLifecycleFact::EffectOutcome(_)
        | ProcessLifecycleFact::EffectOmissions(_) => {}
    }

    if let Some(outcome) = ends {
        if record.is_terminal() {
            return Ok(());
        }
        // The outcome is the state: it takes the wait with it.
        record.lifecycle = ProcessLifecycleState::Terminal { outcome };
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

fn repair_lifecycle_projection(
    record: &ProcessRecord,
    event: &ProcessEvent,
) -> Result<Option<ProcessRecord>, PluginError> {
    let mut repaired = record.clone();
    apply_process_event_projection(&mut repaired, event)?;
    Ok((repaired != *record).then_some(repaired))
}

/// Plan one lifecycle append against `record` as it stands.
///
/// `replay_lookup` is the event the store found under the append's replay
/// key, if any: the same fact answers it (repairing the record's fold when it
/// is the process's last event), and any other fact is a durable-identity
/// conflict.
pub fn prepare_process_event_append(
    record: &ProcessRecord,
    append: CanonicalProcessEventAppend,
    sequence: u64,
    last_event_sequence: Option<u64>,
    replay_lookup: Option<ProcessEvent>,
    occurred_at_ms: u64,
) -> Result<ProcessEventAppendPlan, PluginError> {
    let process_id = &record.id;
    let CanonicalProcessEventAppend {
        request,
        replay_key,
    } = append;
    if let Some(existing) = replay_lookup {
        if existing.fact.same_fact(&request.fact) {
            let repair_record = if last_event_sequence == Some(existing.sequence) {
                repair_lifecycle_projection(record, &existing)?
            } else {
                None
            };
            return Ok(ProcessEventAppendPlan::Replay {
                event: existing,
                repair_record,
            });
        }
        return Err(crate::durable_identity_conflict(format!(
            "process `{process_id}` event replay key `{replay_key}` conflicts with an existing event"
        )));
    }
    let fact = request.fact;
    if matches!(fact, ProcessLifecycleFact::Terminal { .. }) && record.is_terminal() {
        return Err(PluginError::ProcessAlreadyTerminal {
            process_id: process_id.clone(),
            status: record.status(),
        });
    }
    let kind = fact.kind();
    let event = ProcessEvent {
        process_id: process_id.clone(),
        sequence,
        invocation: crate::runtime::causal::process_event_invocation(
            process_id,
            sequence,
            kind.as_str(),
            request.replay,
        ),
        fact,
        trace_cause: request.trace_cause,
        occurred_at: occurred_at_ms,
    };
    let mut projected_record = record.clone();
    apply_process_event_projection(&mut projected_record, &event)?;
    debug_assert!(
        !replay_key.ends_with(FOLD_VALIDATION_REPLAY_KEY_SUFFIX),
        "fold-validation replay keys must be refused before a process-event insert is planned"
    );
    Ok(ProcessEventAppendPlan::Insert {
        event,
        projected_record,
    })
}

pub fn prepare_process_registration(
    registration: ProcessRegistration,
) -> Result<ProcessRegistration, PluginError> {
    validate_process_registration(&registration)?;
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
/// input, lifetime decision, ancestry, originator and environment. Each is what the host stated: a start carries nothing lash
/// derives from a catalog or a deployment default (FIG-4594), so a retry
/// after either changed presents the same start. A host key is global, so the retained process may be another
/// originator's; any other start under it is a
/// [`PluginError::StartKeyConflict`] that names the key and nothing of the
/// process it is bound to.
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
        && submitted.lifetime == retained.lifetime
        && submitted.ancestry == retained.ancestry
        && submitted.session_capability == retained.session_capability
        && submitted.identity == retained.identity
        && submitted.provenance == retained.provenance
        && submitted.env_ref == retained.env_ref;
    if same {
        Ok(())
    } else {
        Err(PluginError::StartKeyConflict {
            start_key: start_key.clone(),
        })
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
}

impl ProcessRegistrationRefusal {
    /// Every refusal rule, in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::LifetimeScopeUnreachable,
        Self::HostGrantOutsideRoot,
        Self::SessionCapabilityUnreachable,
        Self::ExecutionEnvMissing,
        Self::EmptySessionTurnDefinitionKey,
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
