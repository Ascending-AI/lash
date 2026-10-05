//! Protected declarations, declared starts and presentation in rank order.

use super::*;
use crate::runtime::process::WorkerTerminationReceipt;
use crate::tool_dispatch::singleton_run::{IsolatedProcessDescriptor, SingletonPresentationError};
use futures_util::future::{BoxFuture, FutureExt};
use lash_sansio::sync::MutexExt;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

/// The compact outcomes one V's protected preparation produced beside its
/// schedule: the start events, the launched process and its receipt.
#[derive(Clone, Default)]
pub(super) struct Prepared {
    events: Vec<RunEvent>,
    launched: Option<ProcessId>,
    termination: Option<WorkerTerminationReceipt>,
}

/// One V's owner-driven protected preparation: launch, the discharge
/// decision with its effects, then realization. Every part of it runs
/// outside V's step so a served V still replays its journal commands.
enum Preparation<'a> {
    Running(BoxFuture<'a, Result<Prepared, SingletonRunError>>),
    Done(Prepared),
    Failed(Option<SingletonRunError>),
}

/// The parts V's body computed outside the schedule step; the schedule's
/// own record supplies the ordinal and the consumer's `consume` at
/// selection time.
pub(super) struct PresentedParts {
    prefix: Vec<RunEvent>,
    presentation: Option<MaterialRef>,
    failure: Option<crate::tool_run::HookCause>,
    projections: BTreeMap<ToolCallId, serde_json::Value>,
    materials: Vec<MaterialEntry>,
}

impl PresentedParts {
    /// Build V's record inside the schedule step: the prepared start events
    /// and the declarations' settlement precede the presentation events.
    pub(super) fn entry(
        &self,
        call_id: &ToolCallId,
        template: RunRecord,
        consume: bool,
        opener: &EffectOpener,
    ) -> RunJournalEntry {
        let mut events = self.prefix.clone();
        events.extend(presented(
            call_id,
            self.presentation.clone(),
            consume,
            self.failure.clone(),
        ));
        RunJournalEntry {
            state: Vec::new(),
            record: observation_record(
                RunRecord { events, ..template },
                opener,
                self.projections.clone(),
            ),
            materials: self.materials.clone(),
        }
    }
}

/// Protected work remains owned while the schedule accepts other results.
pub(super) struct PendingPresentation<'a> {
    pub call_id: ToolCallId,
    pub handle: parallel::Handle<'a>,
    pub consume: bool,
    /// The discharge decision a served V record supplies — the launched
    /// process and its `cancelled` — before the preparation decides.
    pub(super) recorded: std::sync::Arc<std::sync::Mutex<Option<(ProcessId, bool)>>>,
    owed: Owed<'a>,
    fresh: std::sync::Arc<AtomicBool>,
    preparation: Preparation<'a>,
}

impl<'a> PendingPresentation<'a> {
    /// Poll `future`, then poll a running preparation once — on every pass,
    /// including the pass `future` is already ready because the schedule
    /// was served. A ready output is shown to `inspect` before the
    /// preparation's own poll in that pass. A preparation fault is stored,
    /// not returned: V's body then faults through its channel exactly like
    /// the old in-body fault.
    pub(super) async fn beside<F: std::future::Future>(
        &mut self,
        future: F,
        mut inspect: impl FnMut(&F::Output),
    ) -> F::Output {
        tokio::pin!(future);
        std::future::poll_fn(|context| {
            let ready = future.as_mut().poll(context);
            if let std::task::Poll::Ready(output) = &ready {
                inspect(output);
            }
            if let Preparation::Running(preparation) = &mut self.preparation {
                match preparation.as_mut().poll(context) {
                    std::task::Poll::Ready(Ok(prepared)) => {
                        self.preparation = Preparation::Done(prepared);
                    }
                    std::task::Poll::Ready(Err(error)) => {
                        self.preparation = Preparation::Failed(Some(error));
                    }
                    std::task::Poll::Pending => {}
                }
            }
            ready
        })
        .await
    }

    /// Drive the preparation to its end: a running one is awaited alone, a
    /// failed one returns the stored error and a done one is its outcome.
    pub(super) async fn finish_preparation(&mut self) -> Result<&Prepared, SingletonRunError> {
        if let Preparation::Running(preparation) = &mut self.preparation {
            let result = preparation.await;
            self.preparation = match result {
                Ok(prepared) => Preparation::Done(prepared),
                Err(error) => Preparation::Failed(Some(error)),
            };
        }
        match &mut self.preparation {
            Preparation::Done(prepared) => Ok(prepared),
            Preparation::Failed(error) => {
                Err(error.take().unwrap_or_else(|| boundary(&self.call_id)))
            }
            Preparation::Running(_) => unreachable!("a running preparation was awaited"),
        }
    }
}

impl<'a> RunCoordinator<'a> {
    /// Drain every decided call in rank order: a final's declarations once
    /// every lower committed final is seated, then its presentation with its
    /// incorporation (V).
    ///
    /// # Errors
    ///
    /// A typed [`SingletonRunError`]. A drain asked of a final whose lower
    /// ranks are not seated refuses with [`RunEventRefusal::DrainFrontier`]
    /// before it issues anything.
    pub async fn drain(
        &mut self,
    ) -> Result<Vec<(ToolCallId, SingletonTerminal)>, SingletonRunError> {
        self.begin_frame()?;
        let result = self.bodies.clone().beside(self.drain_inner()).await;
        self.active_frame = false;
        self.note_fault(&result);
        result
    }

    async fn drain_inner(
        &mut self,
    ) -> Result<Vec<(ToolCallId, SingletonTerminal)>, SingletonRunError> {
        let ids: Vec<ToolCallId> = self
            .owed
            .values()
            .map(|owed| owed.call_id.clone())
            .collect();
        let consumed: BTreeSet<ToolCallId> = ids.iter().cloned().collect();
        self.drain_through(u64::MAX, &consumed).await?;
        ids.into_iter()
            .map(|id| {
                let terminal = self.terminal(&id)?;
                Ok((id, terminal))
            })
            .collect()
    }

    pub(super) async fn begin_presentation(
        &mut self,
        rank: u64,
        owed: Owed<'a>,
        consume: bool,
    ) -> Result<PendingPresentation<'a>, SingletonRunError> {
        let call_id = owed.call_id.clone();
        let callback = owed.handlers.clone();
        let handlers = callback.get();
        let decision = owed.decision.clone();
        let capture = owed.capture.clone();
        restore_contributions(&self.journal, &call_id, handlers)?;
        let journal = &mut self.journal;
        let (CallDecision::Final { declares, .. }, Some(capture)) = (&decision, capture.clone())
        else {
            let fresh = std::sync::Arc::new(AtomicBool::new(false));
            let executed = std::sync::Arc::clone(&fresh);
            let handle = async move {
                executed.store(true, Ordering::Relaxed);
                Ok(parallel::Ready::Presentation(std::sync::Arc::new(
                    PresentedParts {
                        prefix: Vec::new(),
                        presentation: None,
                        failure: None,
                        projections: BTreeMap::new(),
                        materials: Vec::new(),
                    },
                )))
            }
            .boxed()
            .shared();
            return Ok(PendingPresentation {
                call_id,
                handle,
                owed,
                fresh,
                recorded: std::sync::Arc::new(std::sync::Mutex::new(None)),
                preparation: Preparation::Done(Prepared::default()),
                consume,
            });
        };

        // A final's declarations are issued only after its decision is
        // durable and every lower committed final is seated, and settle
        // before its presentation.
        // Its declared start is admitted with them and drains before they
        // settle.
        let mut settle = Vec::new();
        let mut obligation = None;
        if *declares {
            if !journal.ledger.drain_frontier_open(rank) {
                return Err(RunEventRefusal::DrainFrontier { call_id }.into());
            }
            obligation = match capture.start() {
                Some(start) => Some(recorded_obligation(journal, &call_id, start)?),
                None => None,
            };
            let mut issue = vec![RunEvent::DeclarationsIssued {
                call_id: call_id.clone(),
            }];
            if let Some(obligation) = &obligation {
                issue.push(RunEvent::StartAdmitted {
                    call_id: call_id.clone(),
                    start_key: obligation.start_key().clone(),
                });
            }
            let issued = journal.record(issue);
            journal
                .append(
                    record_name(&call_id, "declare"),
                    Box::pin(async move {
                        Ok(RunJournalEntry {
                            state: Vec::new(),
                            record: issued,
                            materials: Vec::new(),
                        })
                    }),
                )
                .await?;
            settle.push(RunEvent::DeclarationsSettled {
                call_id: call_id.clone(),
            });
        }

        // P, the one protected preparation this V owns: the launch, the
        // discharge decision and its effects, then realization. None of it
        // owns an SDK run: the external calls run beside V's schedule on
        // every replay, and the only journal the preparation leaves is V's
        // own record, which carries both start events. A served V supplies
        // its recorded decision through `recorded` before P's next poll.
        let isolated = match &capture {
            SingletonCapture::Isolated { binding } => Some(binding.as_ref().clone()),
            _ => None,
        };
        let closing = journal.ledger.lifecycle() == crate::tool_run::RunLifecycle::Closing;
        let realize = *declares && !capture.intents().is_empty();
        let (send, receive) = tokio::sync::oneshot::channel();
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        let prepare_recorded = std::sync::Arc::clone(&recorded);
        let prepare_call = call_id.clone();
        let prepare_capture = capture.clone();
        let prepare_handlers = callback.clone();
        let preparation = async move {
            let prepared = async {
                let mut prepared = Prepared::default();
                if let Some(obligation) = &obligation {
                    let handlers = prepare_handlers.get();
                    let process_id = handlers.launch_start(obligation).await.map_err(
                        |message| {
                            RuntimeEffectControllerError::new(
                                crate::RuntimeErrorCode::EngineEffectController,
                                message,
                            )
                        },
                    )?;
                    prepared.launched = Some(process_id.clone());
                    prepared.events.push(RunEvent::StartLaunched {
                        call_id: prepare_call.clone(),
                        start_key: obligation.start_key().clone(),
                        process_id: process_id.clone(),
                    });
                    let recorded_decision = prepare_recorded.lock_recover().clone();
                    let cancelled = match recorded_decision {
                        Some((recorded_id, cancelled)) => {
                            if recorded_id != process_id {
                                return Err(RuntimeEffectControllerError::new(
                                    crate::RuntimeErrorCode::EffectReplayDivergence,
                                    format!(
                                        "call {prepare_call}'s recorded launch names another process"
                                    ),
                                )
                                .into());
                            }
                            cancelled
                        }
                        None => start::decide_discharge(obligation, handlers, closing)
                            .await
                            .map_err(|message| {
                                RuntimeEffectControllerError::new(
                                    crate::RuntimeErrorCode::EngineEffectController,
                                    message,
                                )
                            })?,
                    };
                    prepared.events.push(RunEvent::StartDischarged {
                        call_id: prepare_call.clone(),
                        start_key: obligation.start_key().clone(),
                        cancelled,
                    });
                    prepared.termination = start::discharge_effects(
                        &prepare_call,
                        obligation,
                        isolated.as_ref(),
                        handlers,
                        &process_id,
                        cancelled,
                    )
                    .await?;
                }
                if realize {
                    prepare_handlers
                        .get()
                        .realize_capture(&prepare_call, &prepare_capture)
                        .await
                        .map_err(|message| {
                            SingletonRunError::from(RuntimeEffectControllerError::new(
                                crate::RuntimeErrorCode::EngineEffectController,
                                message,
                            ))
                        })?;
                }
                Ok::<_, SingletonRunError>(prepared)
            }
            .await;
            let _ = send.send(match &prepared {
                Ok(prepared) => Ok(prepared.clone()),
                Err(error) => Err(error.to_string()),
            });
            prepared
        }
        .boxed();

        // V: presentation, owning only bytes distinct from the output. Its
        // body only awaits P's outcome over the channel; the schedule's own
        // record supplies the ordinal and `consume` at selection time.
        let owner = journal.materials.owner.clone();
        let final_capture = capture.clone();
        let step_call = call_id.clone();
        let observation_decision = decision.clone();
        let fresh = std::sync::Arc::new(AtomicBool::new(false));
        let executed = std::sync::Arc::clone(&fresh);
        let present = async move {
            let prepared = receive
                .await
                .map_err(|_| "the protected preparation ended before V".to_owned())??;
            let handlers = callback.get();
            let descriptor = match (&final_capture, &prepared.launched) {
                (SingletonCapture::Isolated { binding }, Some(process_id)) => {
                    Some(IsolatedProcessDescriptor {
                        process_id: process_id.clone(),
                        start_key: binding.start.start_key.clone(),
                        boundary: binding.boundary,
                        termination: prepared.termination.clone(),
                    })
                }
                _ => None,
            };
            let (text, failure) = match descriptor {
                Some(descriptor) => (encode(&descriptor)?, None),
                None => match handlers.present(&step_call, &final_capture).await {
                    Ok(text) => (text, None),
                    Err(SingletonPresentationError::Refused { cause }) => {
                        let fallback = match final_capture.output() {
                            Some(output) => output.to_owned(),
                            None => encode(&final_capture)?,
                        };
                        (fallback, Some(cause))
                    }
                    Err(SingletonPresentationError::Fault { message }) => return Err(message),
                },
            };
            let projection = handlers.terminal_observation(
                &step_call,
                &observation_decision,
                None,
                Some(&final_capture),
                Some(&text),
            )?;
            let mut owned = Vec::new();
            let presentation = if final_capture.output() == Some(text.as_str()) {
                None
            } else {
                let (reference, entry) = mint(&owner, MaterialRole::Presentation, text)?;
                owned.push(entry);
                Some(reference)
            };
            executed.store(true, Ordering::Relaxed);
            let mut prefix = prepared.events;
            prefix.extend(settle);
            Ok::<_, String>(PresentedParts {
                prefix,
                presentation,
                failure,
                projections: projection
                    .into_iter()
                    .map(|value| (step_call.clone(), value))
                    .collect(),
                materials: owned,
            })
        };
        let handle = async move {
            let parts = present.await.map_err(|message| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::EngineEffectController,
                    message,
                )
            })?;
            Ok(parallel::Ready::Presentation(std::sync::Arc::new(parts)))
        }
        .boxed()
        .shared();
        Ok(PendingPresentation {
            call_id,
            handle,
            owed,
            fresh,
            recorded,
            preparation: Preparation::Running(preparation),
            consume,
        })
    }

    pub(super) fn finish_presentation(
        &mut self,
        pending: PendingPresentation<'a>,
        record: &RunRecord,
    ) -> Result<SingletonTerminal, SingletonRunError> {
        let PendingPresentation {
            owed,
            fresh,
            preparation,
            ..
        } = pending;
        let Owed {
            call_id,
            handlers,
            decision,
            capture,
        } = owed;
        let handlers = handlers.get();
        let prepared = match preparation {
            Preparation::Done(prepared) => prepared,
            Preparation::Running(_) | Preparation::Failed(_) => {
                return Err(boundary(&call_id));
            }
        };
        let presentation_ref = record
            .events
            .iter()
            .find_map(|event| match event {
                RunEvent::Presented { presentation, .. } => Some(presentation.clone()),
                _ => None,
            })
            .flatten();
        let launched = record.events.iter().find_map(|event| match event {
            RunEvent::StartLaunched { process_id, .. } => Some(process_id.clone()),
            _ => None,
        });
        if launched != prepared.launched {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::EffectReplayDivergence,
                format!("call {call_id}'s presented start is not its launched one"),
            )
            .into());
        }
        self.presented.insert(
            call_id.clone(),
            PresentedCall {
                decision: decision.clone(),
                presentation: presentation_ref.clone(),
                launched: launched.clone(),
            },
        );
        let (CallDecision::Final { source, .. }, Some(capture)) = (&decision, capture.clone())
        else {
            if fresh.load(Ordering::Relaxed)
                && let Some(stream) = capture.as_ref().and_then(SingletonCapture::stream)
            {
                handlers.emit_stream(&call_id, stream);
            }
            handlers.incorporate(
                &call_id,
                capture.as_ref(),
                None,
                fresh.load(Ordering::Relaxed),
            )?;
            let observed_cause = self
                .withheld_verdict(&call_id)
                .map(|(callback, verdict)| AttributedVerdict { callback, verdict });
            handlers.observe_terminal(
                &call_id,
                &decision,
                observed_cause.as_ref(),
                capture.as_ref(),
                None,
            )?;
            return Ok(SingletonTerminal::Withheld { decision });
        };
        let journal = &self.journal;
        let presentation = match presentation_ref {
            Some(reference) => journal.materials.read(&reference)?.to_owned(),
            None => capture
                .output()
                .map(str::to_owned)
                .ok_or_else(|| boundary(&call_id))?,
        };
        if fresh.load(Ordering::Relaxed)
            && let Some(stream) = capture.stream()
        {
            handlers.emit_stream(&call_id, stream);
        }
        handlers.incorporate(
            &call_id,
            Some(&capture),
            Some(&presentation),
            fresh.load(Ordering::Relaxed),
        )?;
        handlers.observe_terminal(
            &call_id,
            &decision,
            None,
            Some(&capture),
            Some(&presentation),
        )?;
        Ok(SingletonTerminal::Final {
            source: source.clone(),
            capture,
            presentation,
            launched,
        })
    }
}

fn presented(
    call_id: &ToolCallId,
    presentation: Option<MaterialRef>,
    consume: bool,
    failure: Option<crate::tool_run::HookCause>,
) -> Vec<RunEvent> {
    let mut events = vec![RunEvent::Presented {
        call_id: call_id.clone(),
        presentation,
        failure,
    }];
    if consume {
        events.push(RunEvent::Consumed {
            call_id: call_id.clone(),
        });
    }
    events.push(RunEvent::Incorporated {
        call_id: call_id.clone(),
    });
    events
}

pub(super) fn restore_contributions(
    journal: &RunJournal<'_>,
    call_id: &ToolCallId,
    handlers: &dyn SingletonToolHandlers,
) -> Result<(), SingletonRunError> {
    if let Some(material) = journal
        .records
        .iter()
        .flat_map(|record| &record.events)
        .find_map(|event| match event {
            RunEvent::CheckContributions {
                call_id: id,
                material,
            } if id == call_id => Some(material),
            _ => None,
        })
    {
        handlers.restore_decision_contributions(call_id, journal.materials.read(material)?)?;
    }
    Ok(())
}
