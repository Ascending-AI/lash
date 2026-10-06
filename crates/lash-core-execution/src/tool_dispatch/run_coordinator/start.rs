//! Binding, launch recovery and discharge of admitted process starts.
use super::*;
use crate::runtime::process::{DeclaredStartPhase, StartCancelDecision, StartKey};
use crate::tool_dispatch::{RunStartPrepared, RunStepHandle, StartLaunch};

/// The hold key of a call's declared start: the call's own id, so a call
/// holds at most one process.
fn start_hold_key(call_id: &ToolCallId) -> String {
    format!("{call_id}:start")
}

/// Bind a body's declared start to the Run: the Run's environment, when lash
/// executes the process, and a consumer hold, owned by the Run's opener, that
/// cancels the process with its call when `cancels` (see [`cancels_work`]).
pub(super) fn bind_start(
    call: &SingletonToolCall,
    cancels: bool,
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
        cancels,
    });
    DeclaredStartObligation::new(call.call_id.clone(), registration)
}

/// Whether cancelling a call cancels the process its start launched: the
/// call's recorded cancel policy, narrowed by the cancel hint of the wait a
/// pending call parks on. Under [`crate::CancelHint::Ignore`] a cancelled
/// wait drops only the wait, and the child runs on (ADR 0116 §3.4).
pub(super) fn cancels_work(policy: &RuntimeCallPolicy, hint: Option<crate::CancelHint>) -> bool {
    policy.cancel == ExternalCancelPolicy::CancelExternalWork
        && hint.is_none_or(|hint| hint == crate::CancelHint::CancelExternalWork)
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

/// What a deferred start's served or produced launch carrier recorded.
pub(super) enum ServedLaunch {
    /// The process the start registered and, for a start its call declared,
    /// the launch receipt the record owns.
    Launched {
        process_id: ProcessId,
        receipt: Option<MaterialRef>,
    },
    /// The registrar refused the start: the call's failure capture, which
    /// the record owns.
    Refused { output: MaterialRef },
}

/// The served or produced launch carrier, checked to be exactly this
/// call's `StartLaunched` or `StartRefused`.
pub(super) fn served_launch(
    entry: &RunJournalEntry,
    call_id: &ToolCallId,
    start_key: &StartKey,
) -> Result<ServedLaunch, SingletonRunError> {
    let (served_call, served_key, launch) = match entry.record.events.first() {
        Some(RunEvent::StartLaunched {
            call_id,
            start_key,
            process_id,
            receipt,
        }) => (
            call_id,
            start_key,
            ServedLaunch::Launched {
                process_id: process_id.clone(),
                receipt: receipt.clone(),
            },
        ),
        Some(RunEvent::StartRefused {
            call_id,
            start_key,
            output,
        }) => (
            call_id,
            start_key,
            ServedLaunch::Refused {
                output: output.clone(),
            },
        ),
        _ => {
            return Err(RunEventRefusal::StartOrder {
                call_id: call_id.clone(),
                start_key: start_key.clone(),
            }
            .into());
        }
    };
    if entry.record.events.len() != 1 || served_call != call_id || served_key != start_key {
        return Err(RunEventRefusal::StartOrder {
            call_id: served_call.clone(),
            start_key: served_key.clone(),
        }
        .into());
    }
    Ok(launch)
}

/// The deferred call whose start a launch registers: the attempt that
/// parked on the start, and the stream that attempt recorded.
pub(super) struct ParkedStart<'c> {
    pub(super) call_id: &'c ToolCallId,
    pub(super) attempt: AttemptOrdinal,
    pub(super) stream: Option<crate::runtime::effect::AttemptStream>,
}

/// Register a deferred start inside its VM run. A served launch does not
/// invoke the registrar; crash-before-ACK remains idempotent by StartKey.
/// The launch is a Run record wait, so it keeps the acknowledgement queue.
///
/// A start its call declared under `identity` records the call's
/// `StartProcess` intent outcome with the launch, naming the handle the
/// registrar answered, so every replay reads back the receipt the first
/// launch produced. A start the registrar refused records the call's
/// failure capture instead, reporting the refusal as that outcome; the
/// call's deferred completion resolves to it.
pub(super) async fn launch_start(
    journal: &mut RunJournal<'_>,
    parked: ParkedStart<'_>,
    obligation: &DeclaredStartObligation,
    identity: Option<crate::ToolIntentIdentity>,
    handlers: &dyn SingletonToolHandlers,
) -> Result<RunJournalEntry, SingletonRunError> {
    let ParkedStart {
        call_id,
        attempt,
        stream,
    } = parked;
    let template = journal.record(Vec::new());
    let owner = journal.materials.owner.clone();
    let start_key = obligation.start_key().clone();
    let launched_call = call_id.clone();
    let key = start_key.clone();
    let entry = journal
        .wait_record(
            record_name(call_id, "start:launch"),
            Box::pin(async move {
                let mut materials = Vec::new();
                let event = match handlers.launch_start(obligation).await? {
                    StartLaunch::Launched(handle) => {
                        let process_id = handle.process_id.clone();
                        let receipt = match identity {
                            Some(identity) => {
                                let (reference, entry) = mint(
                                    &owner,
                                    MaterialRole::RealizationReceipt,
                                    encode(&RealizationReceipt {
                                        outcomes: vec![
                                            crate::ToolIntentExecutionOutcome::Executed {
                                                identity,
                                                realized: crate::ToolIntentRealized::StartProcess(
                                                    handle,
                                                ),
                                            },
                                        ],
                                    })?,
                                )?;
                                materials.push(entry);
                                Some(reference)
                            }
                            None => None,
                        };
                        RunEvent::StartLaunched {
                            call_id: launched_call,
                            start_key: key,
                            process_id,
                            receipt,
                        }
                    }
                    StartLaunch::Refused(refusal) => {
                        let outcome = crate::ToolIntentExecutionOutcome::Refused {
                            intent_index: identity
                                .as_ref()
                                .map_or(0, |identity| identity.intent_index),
                            identity,
                            kind: ToolIntentKind::StartProcess,
                            refusal,
                        };
                        let mut capture = handlers
                            .refused_start(&launched_call, attempt, &outcome)
                            .await?;
                        if let (
                            Some(recorded),
                            SingletonCapture::Done { stream, .. }
                            | SingletonCapture::Failed { stream, .. }
                            | SingletonCapture::RetryableFailure { stream, .. },
                        ) = (stream, &mut capture)
                        {
                            *stream = recorded;
                        }
                        let (output, entry) =
                            mint(&owner, MaterialRole::AttemptOutput, encode(&capture)?)?;
                        materials.push(entry);
                        RunEvent::StartRefused {
                            call_id: launched_call,
                            start_key: key,
                            output,
                        }
                    }
                };
                Ok(RunJournalEntry {
                    state: Vec::new(),
                    record: RunRecord {
                        events: vec![event],
                        ..template
                    },
                    materials,
                })
            }),
        )
        .await?;
    served_launch(&entry, call_id, &start_key)?;
    Ok(entry)
}

/// Issue once in the declare frame, through the same body/result bridge as X.
/// Launch, the gate read and discharge effects all belong to this VM run.
pub(super) fn issue_prepare<'a>(
    scoped: &'a ScopedEffectController<'a>,
    call_id: ToolCallId,
    obligation: DeclaredStartObligation,
    handlers: Handlers<'a>,
    closing: bool,
) -> Result<RunStepHandle<'a, RunStartPrepared>, SingletonRunError> {
    scoped.admit_journal_write()?;
    let name = record_name(&call_id, "start:prepare");
    Ok(scoped.controller().start_run_prepare(
        name,
        Box::pin(async move {
            let handlers = handlers.get();
            // A start a final declares is admitted with its declarations;
            // nothing settles its call after them, so a refusal is a fault.
            let process_id = match handlers.launch_start(&obligation).await? {
                StartLaunch::Launched(handle) => handle.process_id,
                StartLaunch::Refused(refusal) => return Err(refusal.describe()),
            };
            let cancelled = decide_discharge(&obligation, handlers, closing).await?;
            discharge_effects(&obligation, handlers, &process_id, cancelled)
                .await
                .map_err(|error| error.to_string())?;
            Ok(RunStartPrepared {
                events: vec![
                    RunEvent::StartLaunched {
                        call_id: call_id.clone(),
                        start_key: obligation.start_key().clone(),
                        process_id,
                        receipt: None,
                    },
                    RunEvent::StartDischarged {
                        call_id,
                        start_key: obligation.start_key().clone(),
                        cancelled,
                    },
                ],
            })
        }),
    ))
}

/// The live discharge decision, asked of the authoritative gate: closing
/// or a requested cancel discharges only a launch whose policy recovers.
/// Shared by deferred discharge and the declared-start preparation body.
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

/// Follow the journaled discharge decision — release the hold, and on a
/// cancel ask the process to stop cooperatively — outside the carrier, on
/// every replay.
pub(super) async fn discharge_effects(
    obligation: &DeclaredStartObligation,
    handlers: &dyn SingletonToolHandlers,
    process_id: &ProcessId,
    cancelled: bool,
) -> Result<(), SingletonRunError> {
    handlers
        .discharge_start(obligation, process_id, cancelled)
        .await
        .map_err(|message| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::EngineEffectController,
                message,
            )
        })?;
    Ok(())
}

/// Journal a deferred start's cancel decision as its `start:discharge`
/// carrier, then follow the recorded decision — release the hold —
/// outside the carrier, on every replay. The carrier's step asks the gate;
/// a replay serves the decision. Only the deferred path owns one; a declared
/// start records its decision together with launch in start:prepare.
pub(super) async fn discharge_start<'a>(
    journal: &mut RunJournal<'a>,
    call_id: &ToolCallId,
    obligation: &DeclaredStartObligation,
    handlers: Handlers<'a>,
    process_id: ProcessId,
    closing: bool,
) -> Result<RunJournalEntry, SingletonRunError> {
    let template = journal.record(Vec::new());
    let start_key = obligation.start_key().clone();
    let discharged_call = call_id.clone();
    let discharged_key = start_key.clone();
    let on_cancel = obligation.on_cancel(DeclaredStartPhase::Launched);
    let gate = handlers.clone();
    let entry = journal
        .wait_record(
            record_name(call_id, "start:discharge"),
            Box::pin(async move {
                let cancel = decide_on_policy(on_cancel, gate.get(), closing).await?;
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
        )
        .await?;
    let cancelled = discharged(&entry, call_id, &start_key)?;
    discharge_effects(obligation, handlers.get(), &process_id, cancelled).await?;
    Ok(entry)
}
