//! Root ownership and retained-result acceptance for Deferred calls.
use super::*;

impl<'a> RunCoordinator<'a> {
    pub(super) fn queue_deferred_start(
        &mut self,
        call: &SingletonToolCall,
        member: AdmittedCall,
        handlers: Handlers<'a>,
        attempt: AttemptOrdinal,
        source: AwaitEventKey,
        start: SingletonStart,
    ) -> DecidedCall {
        self.pending_starts.insert(
            call.call_id.clone(),
            (
                Waiting {
                    call: call.clone(),
                    member,
                    handlers,
                    attempt,
                    start: Some(start),
                },
                source.clone(),
            ),
        );
        DecidedCall::Deferred { source }
    }

    pub(super) async fn drain_starts(&mut self) -> Result<(), SingletonRunError> {
        for (_, (waiting, source)) in std::mem::take(&mut self.pending_starts) {
            let start = waiting
                .start
                .ok_or_else(|| boundary(&waiting.call.call_id))?;
            self.defer_start(
                &waiting.call,
                waiting.member,
                waiting.handlers,
                waiting.attempt,
                source,
                start,
            )
            .await?;
        }
        Ok(())
    }

    /// Admit and launch the start while leaving the call open on its terminal.
    pub(super) async fn defer_start(
        &mut self,
        call: &SingletonToolCall,
        member: AdmittedCall,
        handlers: Handlers<'a>,
        attempt: AttemptOrdinal,
        source: AwaitEventKey,
        start: SingletonStart,
    ) -> Result<DecidedCall, SingletonRunError> {
        let obligation = recorded_obligation(&self.journal, &call.call_id, &start)?;
        let rank = self.journal.ledger.next_rank();
        let record = self.journal.record(Vec::new());
        let id = call.call_id.clone();
        let key = start.start_key.clone();
        let binding = handlers.get();
        let closing = self.journal.ledger.lifecycle() != crate::tool_run::RunLifecycle::Live;
        let admitted = self
            .journal
            .append(
                record_name(&id, "start:admit"),
                Box::pin(async move {
                    let event = if closing || binding.run_cancel_requested() {
                        RunEvent::Decided {
                            call_id: id,
                            rank,
                            decision: CallDecision::Cancelled,
                            after: None,
                        }
                    } else {
                        RunEvent::StartAdmitted {
                            call_id: id,
                            start_key: key,
                        }
                    };
                    Ok(RunJournalEntry {
                        record: RunRecord {
                            events: vec![event],
                            ..record
                        },
                        materials: Vec::new(),
                        state: Vec::new(),
                    })
                }),
            )
            .await?;
        if matches!(admitted.events.first(), Some(RunEvent::Decided { .. })) {
            self.owed.insert(
                rank,
                Owed {
                    call_id: call.call_id.clone(),
                    handlers,
                    decision: CallDecision::Cancelled,
                    capture: None,
                },
            );
            return Ok(DecidedCall::Ranked {
                rank,
                decision: CallDecision::Cancelled,
            });
        }
        let process_id =
            launch_start(&mut self.journal, &call.call_id, &obligation, binding).await?;
        let descriptor = crate::tool_run::SourceDescriptor {
            source: source.clone(),
            call_id: call.call_id.clone(),
            owner: call.owner.clone(),
            resolver: member.binding.executable.owner.clone(),
            cancel: member.policy.cancel,
            authority: crate::tool_run::SourceAuthority::ProcessTerminal {
                process_id: process_id.clone(),
            },
        };
        self.journal
            .scoped
            .controller()
            .arm_run_source(descriptor.clone())
            .await?;
        binding
            .attach_start_terminal(&descriptor, &process_id)
            .await?;
        self.sources.insert(call.call_id.clone(), descriptor);
        self.waiting.insert(
            call.call_id.clone(),
            Waiting {
                call: call.clone(),
                member,
                handlers,
                attempt,
                start: Some(start),
            },
        );
        Ok(DecidedCall::Deferred { source })
    }

    /// Wait at the Run for the real seals of every open Deferred call.
    /// Dispatch descriptors take no rank and never reach presentation.
    /// A resolution that won before cancellation remains protected.
    ///
    /// # Errors
    /// Journal, source authority and typed retained-material refusals. A
    /// handover leaves every call open for the successor's same source.
    pub async fn await_deferred(&mut self) -> Result<(), SingletonRunError> {
        use crate::tool_run::SourceSubscription;
        self.drain_starts().await?;
        while !self.waiting.is_empty() {
            let ids: Vec<_> = self.waiting.keys().cloned().collect();
            let cancelling = ids
                .iter()
                .any(|id| self.waiting[id].handlers.get().run_cancel_requested());
            let selected = if cancelling {
                let id = &ids[0];
                (
                    0,
                    self.journal
                        .scoped
                        .controller()
                        .cancel_run_source(self.sources[id].clone())
                        .await?,
                )
            } else {
                let subscriptions = ids
                    .iter()
                    .map(|id| SourceSubscription {
                        source: self.sources[id].source.clone(),
                        owner: self.sources[id].owner.clone(),
                        segment: self.journal.segment,
                    })
                    .collect();
                let cancel = self
                    .journal
                    .scoped
                    .turn_cancel_wait(tokio_util::sync::CancellationToken::new());
                match self
                    .journal
                    .scoped
                    .controller()
                    .await_run_sources(subscriptions, cancel)
                    .await
                {
                    Ok(selected) => selected,
                    Err(error)
                        if error.code
                            == crate::RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled =>
                    {
                        // The gate is a request. Each source's reply decides
                        // whether its real result already won.
                        for id in &ids {
                            let seal = self
                                .journal
                                .scoped
                                .controller()
                                .cancel_run_source(self.sources[id].clone())
                                .await?;
                            self.accept_source(id, seal).await?;
                        }
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            let id = ids.get(selected.0).ok_or_else(|| boundary(&ids[0]))?;
            self.accept_source(id, selected.1).await?;
        }
        Ok(())
    }

    pub(super) async fn accept_source(
        &mut self,
        id: &ToolCallId,
        seal: crate::tool_run::SourceSeal,
    ) -> Result<(), SingletonRunError> {
        use crate::tool_run::{MaterialHolder, SourceSeal};
        let waiting = self.waiting.remove(id).ok_or_else(|| boundary(id))?;
        if let Some(start) = &waiting.start {
            let obligation = recorded_obligation(&self.journal, id, start)?;
            let process_id = match &self.sources[id].authority {
                crate::tool_run::SourceAuthority::ProcessTerminal { process_id } => {
                    process_id.clone()
                }
                _ => return Err(boundary(id)),
            };
            discharge_start(
                &mut self.journal,
                id,
                &obligation,
                None,
                waiting.handlers.get(),
                process_id,
            )
            .await?;
        }
        match seal {
            SourceSeal::Cancelled => {
                let rank = self.journal.ledger.next_rank();
                let record = self.journal.record(vec![RunEvent::Decided {
                    call_id: id.clone(),
                    rank,
                    decision: CallDecision::Cancelled,
                    after: None,
                }]);
                self.journal
                    .append(
                        record_name(id, "decide"),
                        Box::pin(async move {
                            Ok(RunJournalEntry {
                                record,
                                materials: Vec::new(),
                                state: Vec::new(),
                            })
                        }),
                    )
                    .await?;
                self.owed.insert(
                    rank,
                    Owed {
                        call_id: id.clone(),
                        handlers: waiting.handlers,
                        decision: CallDecision::Cancelled,
                        capture: None,
                    },
                );
            }
            SourceSeal::Resolved { result } => {
                let source = &self.sources[id];
                let store = waiting
                    .handlers
                    .get()
                    .tool_material_store()
                    .ok_or_else(|| {
                        RuntimeEffectControllerError::from(MaterialRefusal::Missing {
                            reference: result.clone(),
                        })
                    })?;
                let payload = store
                    .read_material(
                        &MaterialHolder::Source {
                            source: source.source.clone(),
                        },
                        &result,
                        &MaterialOwner::Source {
                            source: source.source.clone(),
                        },
                        &self.journal.materials.available,
                    )
                    .await?;
                let capture: SingletonCapture =
                    serde_json::from_str(&payload.text).map_err(|error| {
                        RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RecordEncodingFailed,
                            format!("the source's retained result does not decode: {error}"),
                        )
                    })?;
                let shape = OutcomeShape::Done {
                    intents: capture.intents(),
                };
                let capture = match waiting.member.declaration.admits(shape) {
                    Ok(()) => capture,
                    Err(refusal) => SingletonCapture::Refused { refusal },
                };
                // Source bytes stay canonical at the retained source; the
                // decision carries its reference, never another payload copy.
                self.journal
                    .materials
                    .entries
                    .insert(*result.clone(), Some(payload));
                self.decide_candidate(
                    &waiting.call,
                    waiting.handlers,
                    &waiting.member,
                    Some((
                        ResultSource::DeferredCompletion {
                            attempt: waiting.attempt,
                            resolved: result,
                        },
                        capture,
                    )),
                )
                .await?;
            }
        }
        Ok(())
    }
}
