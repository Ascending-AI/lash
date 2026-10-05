//! Protected declarations, declared starts and presentation in rank order.

use super::*;
use crate::tool_dispatch::RunStartPrepared;
use crate::tool_dispatch::singleton_run::{IsolatedProcessDescriptor, SingletonPresentationError};
use futures_util::future::FutureExt;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

/// A final's deterministic drain point. Only its started preparation may be
/// selected alongside other work; V is issued after that result is accepted.
pub(super) struct PendingPresentation<'a> {
    pub call_id: ToolCallId,
    pub handle: Option<parallel::Handle<'a>>,
    pub consume: bool,
    pub prepared: RunStartPrepared,
    owed: Owed<'a>,
    fresh: std::sync::Arc<AtomicBool>,
    settle: Vec<RunEvent>,
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
            return Ok(PendingPresentation {
                call_id,
                handle: None,
                owed,
                consume,
                fresh: std::sync::Arc::new(AtomicBool::new(false)),
                prepared: RunStartPrepared::default(),
                settle: Vec::new(),
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

        let isolated = match &capture {
            SingletonCapture::Isolated { binding } => Some(binding.as_ref().clone()),
            _ => None,
        };
        let handle = if let Some(obligation) = obligation {
            let crate::tool_dispatch::RunStepHandle { body, result } = start::issue_prepare(
                journal.scoped,
                call_id.clone(),
                obligation,
                isolated,
                callback,
                journal.ledger.lifecycle() == crate::tool_run::RunLifecycle::Closing,
            )?;
            self.bodies.issue(body);
            Some(
                async move {
                    Ok(parallel::Ready::StartPrepared(std::sync::Arc::new(
                        result.await?,
                    )))
                }
                .boxed()
                .shared(),
            )
        } else {
            None
        };
        Ok(PendingPresentation {
            call_id,
            handle,
            owed,
            consume,
            settle,
            fresh: std::sync::Arc::new(AtomicBool::new(false)),
            prepared: RunStartPrepared::default(),
        })
    }

    /// V is issued once at the drain frontier, after preparation's durable
    /// window. Realization stays at this owner point until its receipt path
    /// joins the schedule (ADR 0130).
    pub(super) async fn present_pending(
        &mut self,
        pending: PendingPresentation<'a>,
    ) -> Result<(), SingletonRunError> {
        let handlers = pending.owed.handlers.clone();
        let decision = pending.owed.decision.clone();
        let capture = pending.owed.capture.clone();
        let call_id = pending.call_id.clone();
        let prepared = pending.prepared.clone();
        if matches!(decision, CallDecision::Final { declares: true, .. })
            && let Some(capture) = &capture
            && !capture.intents().is_empty()
        {
            handlers
                .get()
                .realize_capture(&call_id, capture)
                .await
                .map_err(|message| {
                    RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::EngineEffectController,
                        message,
                    )
                })?;
        }
        let template = self.journal.record(Vec::new());
        let owner = self.journal.materials.owner.clone();
        let opener = self.journal.owner.clone();
        let settle = pending.settle.clone();
        let consume = pending.consume;
        let executed = std::sync::Arc::clone(&pending.fresh);
        let record = self
            .journal
            .append(
                record_name(&pending.call_id, "present"),
                Box::pin(async move {
                    let handlers = handlers.get();
                    let mut materials = Vec::new();
                    let (presentation, failure, projections) =
                        if let (CallDecision::Final { .. }, Some(final_capture)) =
                            (&decision, &capture)
                        {
                            let launched = prepared.events.iter().find_map(|event| match event {
                                RunEvent::StartLaunched { process_id, .. } => Some(process_id),
                                _ => None,
                            });
                            let descriptor = match (final_capture, launched) {
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
                                None => match handlers.present(&call_id, final_capture).await {
                                    Ok(text) => (text, None),
                                    Err(SingletonPresentationError::Refused { cause }) => {
                                        let fallback = match final_capture.output() {
                                            Some(output) => output.to_owned(),
                                            None => encode(final_capture)?,
                                        };
                                        (fallback, Some(cause))
                                    }
                                    Err(SingletonPresentationError::Fault { message }) => {
                                        return Err(message);
                                    }
                                },
                            };
                            let projection = handlers.terminal_observation(
                                &call_id,
                                &decision,
                                None,
                                Some(final_capture),
                                Some(&text),
                            )?;
                            let presentation = if final_capture.output() == Some(text.as_str()) {
                                None
                            } else {
                                let (reference, entry) =
                                    mint(&owner, MaterialRole::Presentation, text)?;
                                materials.push(entry);
                                Some(reference)
                            };
                            (
                                presentation,
                                failure,
                                projection
                                    .into_iter()
                                    .map(|value| (call_id.clone(), value))
                                    .collect(),
                            )
                        } else {
                            (None, None, BTreeMap::new())
                        };
                    executed.store(true, Ordering::Relaxed);
                    let mut events = settle;
                    events.extend(presented(&call_id, presentation, consume, failure));
                    Ok(RunJournalEntry {
                        state: Vec::new(),
                        materials,
                        record: observation_record(
                            RunRecord { events, ..template },
                            &opener,
                            projections,
                        ),
                    })
                }),
            )
            .await?;
        self.finish_presentation(pending, &record)?;
        Ok(())
    }

    pub(super) fn finish_presentation(
        &mut self,
        pending: PendingPresentation<'a>,
        record: &RunRecord,
    ) -> Result<SingletonTerminal, SingletonRunError> {
        let PendingPresentation {
            owed,
            fresh,
            prepared,
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
        let launched = prepared.events.iter().find_map(|event| match event {
            RunEvent::StartLaunched { process_id, .. } => Some(process_id.clone()),
            _ => None,
        });
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
