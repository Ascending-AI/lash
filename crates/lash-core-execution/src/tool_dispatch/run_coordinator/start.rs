//! Binding, launch recovery and discharge of admitted process starts.
use super::*;
use crate::runtime::process::{
    DeclaredStartPhase, StartCancelDecision, StartKey, WorkerTerminationReceipt,
};
use std::future::Future;

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

/// The owner's answer to a proposed `start:launch` carrier.
type LaunchAnswer = tokio::sync::oneshot::Sender<Result<ProcessId, String>>;

/// The launch's `start:launch` carrier: proposed at its first poll, its
/// step answers only with the process the external launch later
/// registered. The command's journal position is the poll position, not
/// the launch's completion, so a replay reproduces it no matter when the
/// registrar answers; a served carrier never runs the step, so a stale
/// send just fails.
pub(super) fn launch_carrier<'a>(
    scoped: &'a ScopedEffectController<'a>,
    template: RunRecord,
    call_id: &ToolCallId,
    start_key: StartKey,
) -> Result<
    (
        LaunchAnswer,
        impl Future<Output = Result<RunJournalEntry, SingletonRunError>> + Send + 'a,
    ),
    SingletonRunError,
> {
    scoped.admit_journal_write()?;
    let (send, receive) = tokio::sync::oneshot::channel();
    let launched_call = call_id.clone();
    let carrier = scoped.controller().record_run_record(
        record_name(call_id, "start:launch"),
        Box::pin(async move {
            let process_id = receive
                .await
                .map_err(|_| "the launch ended before its carrier".to_owned())??;
            Ok(RunJournalEntry {
                state: Vec::new(),
                record: RunRecord {
                    events: vec![RunEvent::StartLaunched {
                        call_id: launched_call,
                        start_key,
                        process_id,
                    }],
                    ..template
                },
                materials: Vec::new(),
            })
        }),
    );
    Ok((send, async move { Ok(carrier.await?) }))
}

/// The served or produced launch carrier, checked to be exactly this
/// call's `StartLaunched`.
pub(super) fn served_launch(
    entry: &RunJournalEntry,
    call_id: &ToolCallId,
    start_key: &StartKey,
) -> Result<ProcessId, SingletonRunError> {
    let Some(RunEvent::StartLaunched {
        call_id: served_call,
        start_key: served_key,
        process_id,
    }) = entry.record.events.first()
    else {
        return Err(RunEventRefusal::StartOrder {
            call_id: call_id.clone(),
            start_key: start_key.clone(),
        }
        .into());
    };
    if entry.record.events.len() != 1 || served_call != call_id || served_key != start_key {
        return Err(RunEventRefusal::StartOrder {
            call_id: served_call.clone(),
            start_key: served_key.clone(),
        }
        .into());
    }
    Ok(process_id.clone())
}

/// Register the admitted start under its key. The launch itself runs
/// outside any `ctx.run` body — again on every replay that reaches it, so
/// the registrar must answer the process it registered first; only the
/// compact `start:launch` carrier is journaled.
pub(super) async fn launch_start(
    scoped: &ScopedEffectController<'_>,
    template: RunRecord,
    call_id: &ToolCallId,
    obligation: &DeclaredStartObligation,
    handlers: &dyn SingletonToolHandlers,
) -> Result<RunJournalEntry, SingletonRunError> {
    let start_key = obligation.start_key().clone();
    let (send, carrier) = launch_carrier(scoped, template, call_id, start_key.clone())?;
    let (entry, launched) = futures_util::future::join(carrier, async {
        let launched = handlers.launch_start(obligation).await;
        let _ = send.send(launched.clone());
        launched
    })
    .await;
    let launched = launched.map_err(|message| {
        RuntimeEffectControllerError::new(crate::RuntimeErrorCode::EngineEffectController, message)
    })?;
    let entry = entry?;
    let process_id = served_launch(&entry, call_id, &start_key)?;
    if process_id != launched {
        return Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::EffectReplayDivergence,
            format!("call {call_id}'s recorded launch names another process"),
        )
        .into());
    }
    Ok(entry)
}

/// The live discharge decision, asked of the authoritative gate: closing
/// or a requested cancel discharges only a launch whose policy recovers.
/// Shared by a deferred start's carrier body and a presentation's
/// preparation.
pub(super) async fn decide_discharge(
    obligation: &DeclaredStartObligation,
    handlers: &dyn SingletonToolHandlers,
    closing: bool,
) -> Result<bool, String> {
    decide_on_policy(
        obligation.on_cancel(DeclaredStartPhase::Launched),
        handlers,
        closing,
    )
    .await
}

/// The gate half of the live discharge decision over an owned cancel
/// policy, so a carrier step can journal without borrowing the obligation.
async fn decide_on_policy(
    on_cancel: StartCancelDecision,
    handlers: &dyn SingletonToolHandlers,
    closing: bool,
) -> Result<bool, String> {
    Ok((closing || handlers.run_cancel_requested().await?)
        && matches!(
            on_cancel,
            StartCancelDecision::RecoverAndDischarge {
                cancel_process: true,
                ..
            }
        ))
}

/// The discharge's `start:discharge` carrier: proposed at its first poll,
/// its step asks the gate and journals the decision. A replay serves the
/// decision. Only the deferred path owns one; a presentation's
/// preparation decides live and lets V's record carry the answer.
pub(super) fn discharge_carrier<'a>(
    scoped: &'a ScopedEffectController<'a>,
    template: RunRecord,
    call_id: &ToolCallId,
    obligation: &DeclaredStartObligation,
    handlers: Handlers<'a>,
    closing: bool,
) -> Result<
    impl Future<Output = Result<RunJournalEntry, SingletonRunError>> + Send + 'a,
    SingletonRunError,
> {
    scoped.admit_journal_write()?;
    let discharged_call = call_id.clone();
    let discharged_key = obligation.start_key().clone();
    let on_cancel = obligation.on_cancel(DeclaredStartPhase::Launched);
    let carrier = scoped.controller().record_run_record(
        record_name(call_id, "start:discharge"),
        Box::pin(async move {
            let cancel = decide_on_policy(on_cancel, handlers.get(), closing).await?;
            Ok(RunJournalEntry {
                state: Vec::new(),
                record: RunRecord {
                    events: vec![RunEvent::StartDischarged {
                        call_id: discharged_call,
                        start_key: discharged_key,
                        cancelled: cancel,
                    }],
                    ..template
                },
                materials: Vec::new(),
            })
        }),
    );
    Ok(async move { Ok(carrier.await?) })
}

/// The served or produced discharge carrier, checked to be exactly this
/// call's `StartDischarged`; the answer is the journaled `cancelled`.
pub(super) fn discharged(
    entry: &RunJournalEntry,
    call_id: &ToolCallId,
    start_key: &StartKey,
) -> Result<bool, SingletonRunError> {
    let Some(RunEvent::StartDischarged {
        call_id: served_call,
        start_key: served_key,
        cancelled,
    }) = entry.record.events.first()
    else {
        return Err(RunEventRefusal::StartOrder {
            call_id: call_id.clone(),
            start_key: start_key.clone(),
        }
        .into());
    };
    if entry.record.events.len() != 1 || served_call != call_id || served_key != start_key {
        return Err(RunEventRefusal::StartOrder {
            call_id: served_call.clone(),
            start_key: served_key.clone(),
        }
        .into());
    }
    Ok(*cancelled)
}

/// Follow the journaled discharge decision — terminate a hard-isolated
/// worker, release the hold — outside the carrier, on every replay.
pub(super) async fn discharge_effects(
    call_id: &ToolCallId,
    obligation: &DeclaredStartObligation,
    isolated: Option<&RecordedIsolatedStart>,
    handlers: &dyn SingletonToolHandlers,
    process_id: &ProcessId,
    cancelled: bool,
) -> Result<Option<WorkerTerminationReceipt>, SingletonRunError> {
    let engine = isolated
        .map(|binding| require_isolated_engine(handlers, &binding.engine_kind, binding.boundary))
        .transpose()?;
    let hard =
        isolated.is_some_and(|binding| binding.boundary == ProcessExecutionBoundary::WorkerProcess);
    let mut receipt = None;
    if cancelled && hard {
        let binding = isolated.ok_or_else(|| boundary(call_id))?;
        let worker = engine
            .as_ref()
            .and_then(|engine| engine.physical_worker())
            .ok_or_else(|| IsolatedStartRefusal::Unavailable {
                kind: binding.engine_kind.clone(),
            })?;
        let terminated = worker.terminate_worker(process_id).await.map_err(|error| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::EngineEffectController,
                error.to_string(),
            )
        })?;
        if terminated.process_id != *process_id {
            return Err(IsolatedStartRefusal::TerminationOwner.into());
        }
        receipt = Some(terminated);
    }
    handlers
        .discharge_start(obligation, process_id, cancelled)
        .await
        .map_err(|message| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::EngineEffectController,
                message,
            )
        })?;
    Ok(receipt)
}

/// Journal a deferred start's cancel decision as its `start:discharge`
/// carrier, then follow the recorded decision — release the hold —
/// outside the carrier, on every replay. A deferred start is never
/// isolated, so no worker is terminated here.
pub(super) async fn discharge_start<'a>(
    scoped: &'a ScopedEffectController<'a>,
    template: RunRecord,
    call_id: &ToolCallId,
    obligation: &DeclaredStartObligation,
    handlers: Handlers<'a>,
    process_id: ProcessId,
    closing: bool,
) -> Result<RunJournalEntry, SingletonRunError> {
    let start_key = obligation.start_key().clone();
    let carrier = discharge_carrier(
        scoped,
        template,
        call_id,
        obligation,
        handlers.clone(),
        closing,
    )?;
    let entry = carrier.await?;
    let cancelled = discharged(&entry, call_id, &start_key)?;
    discharge_effects(
        call_id,
        obligation,
        None,
        handlers.get(),
        &process_id,
        cancelled,
    )
    .await?;
    Ok(entry)
}
