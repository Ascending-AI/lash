//! Binding, launch recovery and discharge of admitted process starts.
use super::*;
use crate::runtime::process::{DeclaredStartPhase, StartCancelDecision, WorkerTerminationReceipt};

/// The hold key of a call's declared start: the call's own id, so a call
/// holds at most one process.
fn start_hold_key(call_id: &ToolCallId) -> String {
    format!("{call_id}:start")
}

/// Bind a body's declared start to the Run: the Run's environment, when lash
/// executes the process, and a consumer hold, owned by the Run's opener, that
/// carries the call's recorded cancel policy.
pub(super) fn bind_start(
    call: &SingletonToolCall,
    policy: &RuntimeCallPolicy,
    mut registration: ProcessStartRegistration,
) -> Result<DeclaredStartObligation, DeclaredStartObligationRefusal> {
    registration.env_ref = if registration.input.is_externally_owned() {
        None
    } else {
        call.environment.clone()
    };
    registration.consumer_hold = Some(ConsumerHold {
        key: start_hold_key(&call.call_id),
        owner: ScopeId::Opener(call.owner.clone()),
        cancels: policy.cancel == ExternalCancelPolicy::CancelExternalWork,
    });
    DeclaredStartObligation::new(call.call_id.clone(), registration)
}

/// The obligation a recorded attempt owns, checked against the key and call
/// the capture names.
pub(super) fn recorded_obligation(
    journal: &RunJournal<'_>,
    call_id: &ToolCallId,
    start: &SingletonStart,
) -> Result<DeclaredStartObligation, SingletonRunError> {
    let obligation: DeclaredStartObligation = journal.materials.decode(&start.obligation)?;
    if obligation.start_key() != &start.start_key || &obligation.call_id != call_id {
        return Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::EffectReplayDivergence,
            format!(
                "call {call_id}'s recorded start obligation does not name start {}",
                start.start_key
            ),
        )
        .into());
    }
    Ok(obligation)
}

/// K5 inside the protected drain: register the admitted start under its key,
/// then discharge it. The Run's cancellation is read once, inside the
/// discharge step, and the recorded cancel policy decides what it does to
/// the launched process.
pub(super) async fn drain_start(
    journal: &mut RunJournal<'_>,
    call_id: &ToolCallId,
    obligation: &DeclaredStartObligation,
    isolated: Option<&RecordedIsolatedStart>,
    handlers: &dyn SingletonToolHandlers,
) -> Result<(ProcessId, Option<WorkerTerminationReceipt>), SingletonRunError> {
    let process_id = launch_start(journal, call_id, obligation, handlers).await?;
    discharge_start(journal, call_id, obligation, isolated, handlers, process_id).await
}

pub(super) async fn launch_start(
    journal: &mut RunJournal<'_>,
    call_id: &ToolCallId,
    obligation: &DeclaredStartObligation,
    handlers: &dyn SingletonToolHandlers,
) -> Result<ProcessId, SingletonRunError> {
    let start_key = obligation.start_key().clone();
    let launch_record = journal.record(Vec::new());
    let (step_call, key) = (call_id.clone(), start_key.clone());
    let launch = Box::pin(async move {
        let process_id = handlers.launch_start(obligation).await?;
        Ok(RunJournalEntry {
            state: Vec::new(),
            record: RunRecord {
                events: vec![RunEvent::StartLaunched {
                    call_id: step_call,
                    start_key: key,
                    process_id,
                }],
                ..launch_record
            },
            materials: Vec::new(),
        })
    });
    let launched = journal
        .append(record_name(call_id, "start:launch"), launch)
        .await?;
    let Some(RunEvent::StartLaunched { process_id, .. }) = launched.events.first() else {
        return Err(RunEventRefusal::StartOrder {
            call_id: call_id.clone(),
            start_key,
        }
        .into());
    };
    let process_id = process_id.clone();

    Ok(process_id)
}

pub(super) async fn discharge_start(
    journal: &mut RunJournal<'_>,
    call_id: &ToolCallId,
    obligation: &DeclaredStartObligation,
    isolated: Option<&RecordedIsolatedStart>,
    handlers: &dyn SingletonToolHandlers,
    process_id: ProcessId,
) -> Result<(ProcessId, Option<WorkerTerminationReceipt>), SingletonRunError> {
    let start_key = obligation.start_key().clone();
    let engine = isolated
        .map(|binding| require_isolated_engine(handlers, &binding.engine_kind, binding.boundary))
        .transpose()?;
    let hard =
        isolated.is_some_and(|binding| binding.boundary == ProcessExecutionBoundary::WorkerProcess);
    let owner = journal.materials.owner.clone();
    let closing = journal.ledger.lifecycle() == crate::tool_run::RunLifecycle::Closing;
    let discharge_record = journal.record(Vec::new());
    let (step_call, launched_id) = (call_id.clone(), process_id.clone());
    let discharge = Box::pin(async move {
        let cancel = (closing || handlers.run_cancel_requested().await?)
            && matches!(
                obligation.on_cancel(DeclaredStartPhase::Launched),
                StartCancelDecision::RecoverAndDischarge {
                    cancel_process: true,
                    ..
                }
            );
        let mut materials = Vec::new();
        if cancel && hard {
            let worker = engine
                .as_ref()
                .and_then(|engine| engine.physical_worker())
                .ok_or("the admitted physical worker is unavailable")?;
            let receipt = worker
                .terminate_worker(&launched_id)
                .await
                .map_err(|error| error.to_string())?;
            if receipt.process_id != launched_id {
                return Err(IsolatedStartRefusal::TerminationOwner.to_string());
            }
            let (_, entry) = mint(&owner, MaterialRole::AttemptOutput, encode(&receipt)?)?;
            materials.push(entry);
        }
        handlers
            .discharge_start(obligation, &launched_id, cancel)
            .await?;
        Ok(RunJournalEntry {
            state: Vec::new(),
            record: RunRecord {
                events: vec![RunEvent::StartDischarged {
                    call_id: step_call,
                    start_key,
                    cancelled: cancel,
                }],
                ..discharge_record
            },
            materials,
        })
    });
    let (record, references) = journal
        .append_entry(record_name(call_id, "start:discharge"), discharge)
        .await?;
    let receipt = match references.first() {
        Some(reference) => {
            let receipt: WorkerTerminationReceipt = journal.materials.decode(reference)?;
            if receipt.process_id != process_id {
                return Err(IsolatedStartRefusal::TerminationOwner.into());
            }
            Some(receipt)
        }
        _ => None,
    };
    if hard
        && receipt.is_none()
        && record.events.iter().any(|event| {
            matches!(
                event,
                RunEvent::StartDischarged {
                    cancelled: true,
                    ..
                }
            )
        })
    {
        return Err(IsolatedStartRefusal::TerminationMissing.into());
    }
    Ok((process_id, receipt))
}
