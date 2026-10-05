//! Protected declarations, declared starts and presentation in rank order.

use super::*;
use crate::tool_dispatch::singleton_run::{IsolatedProcessDescriptor, SingletonPresentationError};
use futures_util::FutureExt;
use std::sync::atomic::{AtomicBool, Ordering};

/// Protected work remains owned while the schedule accepts other results.
pub(super) struct PendingPresentation<'a> {
    pub call_id: ToolCallId,
    pub handle: parallel::Handle<'a>,
    pub consume: bool,
    owed: Owed<'a>,
    fresh: std::sync::Arc<AtomicBool>,
    launched: Option<ProcessId>,
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
        self.drain_starts().await?;
        let owed = std::mem::take(&mut self.owed);
        let mut terminals = Vec::with_capacity(owed.len());
        for (rank, owed) in owed {
            let call_id = owed.call_id.clone();
            terminals.push((call_id, self.present(rank, owed, true).await?));
        }
        Ok(terminals)
    }

    pub(super) async fn present(
        &mut self,
        rank: u64,
        owed: Owed<'a>,
        consume: bool,
    ) -> Result<SingletonTerminal, SingletonRunError> {
        self.present_inner(rank, owed, consume).await
    }

    async fn present_inner(
        &mut self,
        rank: u64,
        owed: Owed<'a>,
        consume: bool,
    ) -> Result<SingletonTerminal, SingletonRunError> {
        let pending = self.begin_presentation(rank, owed, consume).await?;
        let handle = pending.handle.clone();
        self.journal.scoped.admit_journal_write()?;
        let entry = self
            .journal
            .scoped
            .controller()
            .record_run_record(
                record_name(&pending.call_id, "present"),
                Box::pin(async move {
                    match handle.await.map_err(|error| error.to_string())? {
                        parallel::Ready::Presentation(entry) => Ok(entry.as_ref().clone()),
                        _ => unreachable!("a presentation handle returns V"),
                    }
                }),
            )
            .await?;
        let record = self.journal.accept(entry)?;
        self.finish_presentation(pending, &record)
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
            let record = journal.record(presented(&call_id, None, consume, None));
            let fresh = std::sync::Arc::new(AtomicBool::new(false));
            let executed = std::sync::Arc::clone(&fresh);
            let handle = async move {
                executed.store(true, Ordering::Relaxed);
                Ok(parallel::Ready::Presentation(std::sync::Arc::new(
                    RunJournalEntry {
                        state: Vec::new(),
                        record,
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
                launched: None,
                consume,
            });
        };

        // A final's declarations are issued only after its decision is
        // durable and every lower committed final is seated, and settle
        // before its presentation.
        // Its declared start is admitted with them and drains before they
        // settle.
        let mut settle = Vec::new();
        let mut launched = None;
        let mut termination = None;
        if *declares {
            if !journal.ledger.drain_frontier_open(rank) {
                return Err(RunEventRefusal::DrainFrontier { call_id }.into());
            }
            let obligation = match capture.start() {
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
            if let Some(obligation) = &obligation {
                let isolated = match &capture {
                    SingletonCapture::Isolated { binding } => Some(binding.as_ref()),
                    _ => None,
                };
                let (process, receipt) =
                    drain_start(journal, &call_id, obligation, isolated, handlers).await?;
                launched = Some(process);
                termination = receipt;
            }
            settle.push(RunEvent::DeclarationsSettled {
                call_id: call_id.clone(),
            });
        }

        // V: presentation, owning only bytes distinct from the output, in one
        // record with its incorporation.
        let present_record = journal.record(Vec::new());
        let owner = journal.materials.owner.clone();
        let final_capture = capture.clone();
        let declares = *declares;
        let step_call = call_id.clone();
        let descriptor = match (&capture, &launched) {
            (SingletonCapture::Isolated { binding }, Some(process_id)) => {
                Some(IsolatedProcessDescriptor {
                    process_id: process_id.clone(),
                    start_key: binding.start.start_key.clone(),
                    boundary: binding.boundary,
                    termination,
                })
            }
            _ => None,
        };
        let observation_decision = decision.clone();
        let fresh = std::sync::Arc::new(AtomicBool::new(false));
        let executed = std::sync::Arc::clone(&fresh);
        let present: crate::RunRecordStep<'a> = Box::pin(async move {
            let handlers = callback.get();
            if declares && !final_capture.intents().is_empty() {
                handlers.realize_capture(&step_call, &final_capture).await?;
            }
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
            let mut events = settle;
            events.extend(presented(&step_call, presentation, consume, failure));
            executed.store(true, Ordering::Relaxed);
            let projections = projection
                .into_iter()
                .map(|value| (step_call.clone(), value))
                .collect();
            Ok(RunJournalEntry {
                state: Vec::new(),
                record: observation_record(
                    RunRecord {
                        events,
                        ..present_record
                    },
                    &match &owner {
                        MaterialOwner::Run { opener } => opener.clone(),
                        _ => unreachable!("a Run owns its presentation"),
                    },
                    projections,
                ),
                materials: owned,
            })
        });
        let handle = async move {
            let entry = present.await.map_err(|message| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::EngineEffectController,
                    message,
                )
            })?;
            Ok(parallel::Ready::Presentation(std::sync::Arc::new(entry)))
        }
        .boxed()
        .shared();
        Ok(PendingPresentation {
            call_id,
            handle,
            owed,
            fresh,
            launched,
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
            launched,
            ..
        } = pending;
        let Owed {
            call_id,
            handlers,
            decision,
            capture,
        } = owed;
        let handlers = handlers.get();
        let presentation_ref = record
            .events
            .iter()
            .find_map(|event| match event {
                RunEvent::Presented { presentation, .. } => Some(presentation.clone()),
                _ => None,
            })
            .flatten();
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
