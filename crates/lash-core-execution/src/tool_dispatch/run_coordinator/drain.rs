//! Protected declarations, declared starts and presentation in rank order.

use super::*;
use crate::tool_dispatch::singleton_run::{IsolatedProcessDescriptor, SingletonPresentationError};
use std::sync::atomic::{AtomicBool, Ordering};

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
        let result = self.drain_inner().await;
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
        let handles = self
            .pending
            .iter()
            .map(parallel::Pending::handle)
            .collect::<Vec<_>>();
        parallel::poll_beside(&handles, self.present_inner(rank, owed, consume)).await?
    }

    async fn present_inner(
        &mut self,
        rank: u64,
        owed: Owed<'a>,
        consume: bool,
    ) -> Result<SingletonTerminal, SingletonRunError> {
        let Owed {
            call_id,
            handlers,
            decision,
            capture,
        } = owed;
        let handlers = handlers.get();
        restore_contributions(&self.journal, &call_id, handlers)?;
        let journal = &mut self.journal;
        let (CallDecision::Final { declares, source }, Some(capture)) =
            (&decision, capture.clone())
        else {
            // V: a withheld call is presented by its decision and
            // incorporated; the stream its body emitted is still the host's.
            let present = journal.record(presented(&call_id, None, consume, None));
            let fresh = AtomicBool::new(false);
            let executed = &fresh;
            journal
                .append(
                    record_name(&call_id, "present"),
                    Box::pin(async move {
                        executed.store(true, Ordering::Relaxed);
                        Ok(RunJournalEntry {
                            state: Vec::new(),
                            record: present,
                            materials: Vec::new(),
                        })
                    }),
                )
                .await?;
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
            self.presented.insert(
                call_id,
                PresentedCall {
                    decision: decision.clone(),
                    presentation: None,
                    launched: None,
                },
            );
            return Ok(SingletonTerminal::Withheld { decision });
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
        let fresh = AtomicBool::new(false);
        let executed = &fresh;
        let present = Box::pin(async move {
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
            Ok(RunJournalEntry {
                state: Vec::new(),
                record: RunRecord {
                    events,
                    ..present_record
                },
                materials: owned,
            })
        });
        let presented_record = journal
            .append(record_name(&call_id, "present"), present)
            .await?;
        let presentation = presented_record
            .events
            .iter()
            .find_map(|event| match event {
                RunEvent::Presented { presentation, .. } => Some(presentation.clone()),
                _ => None,
            });
        let presentation_ref = presentation.flatten();
        self.presented.insert(
            call_id.clone(),
            PresentedCall {
                decision: decision.clone(),
                presentation: presentation_ref.clone(),
                launched: launched.clone(),
            },
        );
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
