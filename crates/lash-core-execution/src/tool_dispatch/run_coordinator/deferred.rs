//! Root ownership and retained-result acceptance for Deferred calls.
use super::*;
use crate::tool_run::CompletionSource;

impl<'a> RunCoordinator<'a> {
    pub(super) async fn accept_pending(
        &mut self,
        call: &SingletonToolCall,
        member: AdmittedCall,
        handlers: Handlers<'a>,
        attempt: AttemptOrdinal,
        pending: PendingAttempt<'_>,
    ) -> Result<DecidedCall, SingletonRunError> {
        let PendingAttempt {
            source,
            metadata,
            start,
        } = pending;
        let pending: RecordedPending = self.journal.materials.decode(metadata)?;
        if let Some(start) = start {
            return Ok(self.queue_deferred_start(
                call,
                member,
                handlers,
                attempt,
                source,
                SingletonStart {
                    start_key: start.start_key.clone(),
                    obligation: start.obligation.clone(),
                },
            ));
        }
        let authority = match &pending.completion.resolved_by {
            Some(crate::PendingResolver::ProcessTerminal { process_id }) => {
                crate::tool_run::SourceAuthority::ProcessTerminal {
                    process_id: process_id.clone(),
                }
            }
            None => crate::tool_run::SourceAuthority::ExternalCompletion,
            Some(crate::PendingResolver::DeclaredStart(_)) => return Err(boundary(&call.call_id)),
        };
        let descriptor = crate::tool_run::SourceDescriptor {
            source: source.clone(),
            call_id: call.call_id.clone(),
            owner: call.owner.clone(),
            resolver: member.binding.executable.owner.clone(),
            cancel: member.policy.cancel,
            authority,
        };
        self.journal
            .scoped
            .arm_run_source(descriptor.clone())
            .await?;
        handlers
            .get()
            .arm_pending(&descriptor, &pending.completion)
            .await?;
        self.sources.insert(call.call_id.clone(), descriptor);
        self.waiting.insert(
            call.call_id.clone(),
            Waiting {
                call: call.clone(),
                member,
                handlers,
                attempt,
                start: None,
            },
        );
        Ok(DecidedCall::Deferred { source })
    }

    fn pending_metadata(
        &self,
        id: &ToolCallId,
    ) -> Result<Option<RecordedPending>, SingletonRunError> {
        self.attempts
            .iter()
            .rev()
            .find_map(|entry| match &entry.result {
                AttemptOutcome::Waiting(CompletionSource::Pending { metadata, .. })
                    if &entry.call_id == id =>
                {
                    Some(self.journal.materials.decode(metadata))
                }
                _ => None,
            })
            .transpose()
            .map_err(Into::into)
    }

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
        for (_, (waiting, output)) in std::mem::take(&mut self.refused_starts) {
            Box::pin(self.settle_refused_start(
                &waiting.call,
                &waiting.member,
                waiting.handlers,
                waiting.attempt,
                output,
            ))
            .await?;
        }
        for (_, (waiting, source)) in std::mem::take(&mut self.pending_starts) {
            let start = waiting
                .start
                .ok_or_else(|| boundary(&waiting.call.call_id))?;
            Box::pin(self.defer_start(
                &waiting.call,
                waiting.member,
                waiting.handlers,
                waiting.attempt,
                source,
                start,
            ))
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
                    let event = if closing || binding.run_cancel_requested().await? {
                        RunEvent::Decided {
                            call_id: id,
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
            let rank = self
                .journal
                .ledger
                .decision_rank(&call.call_id)
                .ok_or_else(|| boundary(&call.call_id))?;
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
        // A start the call declared as its pending resolver is the call's
        // intent; its launch records the call's receipt. A refused launch
        // settles the call, which emits the stream its parked attempt
        // recorded.
        let pending = self.pending_metadata(&call.call_id)?;
        let identity = pending
            .as_ref()
            .and_then(|pending| match &pending.completion.resolved_by {
                Some(crate::PendingResolver::DeclaredStart(start)) => {
                    Some(start.identity().clone())
                }
                _ => None,
            });
        let launch = Box::pin(launch_start(
            &mut self.journal,
            ParkedStart {
                call_id: &call.call_id,
                attempt,
                stream: pending.map(|pending| pending.stream),
            },
            &obligation,
            identity,
            binding,
        ))
        .await?;
        let served = served_launch(&launch, &call.call_id, &start.start_key)?;
        self.journal.accept(launch)?;
        let (process_id, receipt) = match served {
            ServedLaunch::Launched {
                process_id,
                receipt,
            } => (process_id, receipt),
            ServedLaunch::Refused { output } => {
                return Box::pin(
                    self.settle_refused_start(call, &member, handlers, attempt, output),
                )
                .await;
            }
        };
        if let Some(receipt) = receipt {
            let receipt: RealizationReceipt = self.journal.materials.decode(&receipt)?;
            binding.adopt_realization(&call.call_id, &receipt)?;
        }
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
            .arm_run_source(descriptor.clone())
            .await?;
        if let Some(pending) = self.pending_metadata(&call.call_id)? {
            binding
                .arm_pending(&descriptor, &pending.completion)
                .await?;
        } else {
            binding
                .attach_start_terminal(&descriptor, &process_id)
                .await?;
        }
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

    /// Settle a deferred call whose admitted start the registrar refused: no
    /// source was armed, and the call's deferred completion is the failure
    /// capture its `StartRefused` record owns.
    async fn settle_refused_start(
        &mut self,
        call: &SingletonToolCall,
        member: &AdmittedCall,
        handlers: Handlers<'a>,
        attempt: AttemptOrdinal,
        output: MaterialRef,
    ) -> Result<DecidedCall, SingletonRunError> {
        let capture: SingletonCapture = self.journal.materials.decode(&output)?;
        self.decide_candidate(
            call,
            handlers,
            member,
            Some((
                ResultSource::DeferredCompletion {
                    attempt,
                    resolved: Box::new(output),
                },
                capture,
            )),
        )
        .await
    }

    /// Wait at the Run for the real seals of every open Deferred call.
    /// Dispatch descriptors take no rank and never reach presentation.
    /// A resolution that won before cancellation remains protected.
    ///
    /// # Errors
    /// Journal, source authority and typed retained-material refusals. A
    /// handover leaves every call open for the successor's same source.
    pub async fn await_deferred(&mut self) -> Result<(), SingletonRunError> {
        while !self.waiting.is_empty()
            || !self.pending_starts.is_empty()
            || !self.refused_starts.is_empty()
        {
            self.await_one_deferred().await?;
        }
        Ok(())
    }

    /// Accept one source seal so an aggregate can observe its winner while
    /// the owning Run keeps every other source live.
    pub(crate) async fn await_empty_aggregate(&self) -> Result<(), SingletonRunError> {
        let cancel = self
            .journal
            .scoped
            .turn_cancel_wait(tokio_util::sync::CancellationToken::new());
        self.journal
            .scoped
            .await_run_sources(Vec::new(), Vec::new(), cancel)
            .await?;
        Err(crate::RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
            "an empty aggregate has no source terminal",
        )
        .into())
    }

    /// Wait for one recorded selection with every open source in it, through
    /// its acceptance: a source's seal, or a sibling body, timer or receipt
    /// that completed first.
    ///
    /// # Errors
    /// Journal, source authority and typed retained-material refusals. A
    /// handover leaves every call open for the successor's same source.
    pub async fn await_one_deferred(&mut self) -> Result<(), SingletonRunError> {
        self.drain_starts().await?;
        if self.waiting.is_empty() {
            return Ok(());
        }
        self.bodies
            .clone()
            .beside(self.schedule_window(&mut None, true))
            .await
            .map(|_| ())
    }

    /// Race every open source's seal and the turn's cancel gate beside the
    /// schedule's `selectable` notifications, in one engine wait. A cancelled
    /// wait is a request: each source's reply decides whether its real
    /// result already won, and those seals are accepted here.
    pub(super) async fn race_sources(
        &mut self,
        selectable: Vec<crate::tool_dispatch::SelectKey>,
    ) -> Result<SourceRace, SingletonRunError> {
        use crate::tool_dispatch::RunSourceWake;
        use crate::tool_run::SourceSubscription;
        let ids: Vec<_> = self.waiting.keys().cloned().collect();
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
            .await_run_sources(subscriptions, selectable, cancel)
            .await
        {
            Ok(RunSourceWake::Selected(index)) => Ok(SourceRace::Selected(index)),
            Ok(RunSourceWake::Sealed { index, seal }) => {
                let id = ids.get(index).ok_or_else(|| boundary(&ids[0]))?;
                Ok(SourceRace::Sealed(id.clone(), seal))
            }
            Err(error) if error.code == crate::RuntimeErrorCode::RuntimeToolRunAwaitCancelled => {
                for id in &ids {
                    let seal = self
                        .journal
                        .scoped
                        .cancel_run_source(self.sources[id].clone())
                        .await?;
                    self.accept_source(id, seal).await?;
                }
                Ok(SourceRace::Cancelled)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) async fn accept_source(
        &mut self,
        id: &ToolCallId,
        seal: crate::tool_run::SourceSeal,
    ) -> Result<(), SingletonRunError> {
        use crate::tool_run::SourceSeal;
        let waiting = self.waiting.remove(id).ok_or_else(|| boundary(id))?;
        if let Some(start) = &waiting.start {
            let obligation = recorded_obligation(&self.journal, id, start)?;
            let process_id = match &self.sources[id].authority {
                crate::tool_run::SourceAuthority::ProcessTerminal { process_id } => {
                    process_id.clone()
                }
                _ => return Err(boundary(id)),
            };
            let closing = self.journal.ledger.lifecycle() == crate::tool_run::RunLifecycle::Closing;
            let discharge = discharge_start(
                &mut self.journal,
                id,
                &obligation,
                waiting.handlers.clone(),
                process_id,
                closing,
            )
            .await?;
            self.journal.accept(discharge)?;
        }
        match seal {
            SourceSeal::Cancelled => {
                let record = self.journal.record(vec![RunEvent::Decided {
                    call_id: id.clone(),
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
                let rank = self
                    .journal
                    .ledger
                    .decision_rank(id)
                    .ok_or_else(|| boundary(id))?;
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
                let payload =
                    read_source_result(&self.journal, id, &source.source, &result, store).await?;
                let source_result = result.clone();
                let capture: SingletonCapture =
                    serde_json::from_str(&payload.text).map_err(|error| {
                        RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RecordEncodingFailed,
                            format!("the source's retained result does not decode: {error}"),
                        )
                    })?;
                let (result, capture) = if waiting.handlers.get().finalizes_source() {
                    let pending = self.pending_metadata(id)?;
                    let binding = waiting.handlers.get();
                    let record = self.journal.record(Vec::new());
                    let owner = self.journal.materials.owner.clone();
                    let original = capture;
                    let call_id = id.clone();
                    let ordinal = waiting.attempt;
                    let entry = self
                        .journal
                        .append(
                            record_name(id, "source:finalize"),
                            Box::pin(async move {
                                let mut capture = binding
                                    .finalize_source(
                                        &call_id,
                                        ordinal,
                                        &original,
                                        pending.as_ref().map(|pending| &pending.completion),
                                    )
                                    .await?;
                                if let Some(pending) = pending {
                                    match &mut capture {
                                        SingletonCapture::Done { stream, .. }
                                        | SingletonCapture::Failed { stream, .. } => {
                                            *stream = pending.stream
                                        }
                                        _ => {}
                                    }
                                }
                                let (output, material) =
                                    mint(&owner, MaterialRole::AttemptOutput, encode(&capture)?)?;
                                Ok(RunJournalEntry {
                                    record: RunRecord {
                                        events: vec![RunEvent::SourceCaptured { call_id, output }],
                                        ..record
                                    },
                                    materials: vec![material],
                                    state: Vec::new(),
                                })
                            }),
                        )
                        .await?;
                    let Some(RunEvent::SourceCaptured { output, .. }) = entry.events.first() else {
                        return Err(boundary(id));
                    };
                    (
                        Box::new(output.clone()),
                        self.journal.materials.decode(output)?,
                    )
                } else {
                    (result, capture)
                };
                let shape = OutcomeShape::Done {
                    intents: capture.intents(),
                };
                let capture = match waiting.member.declaration.admits(shape) {
                    Ok(()) => capture,
                    Err(refusal) => SingletonCapture::Refused { refusal },
                };
                // The decision carries the source's reference, never another
                // payload copy; only the read step journals the bytes.
                self.journal
                    .materials
                    .entries
                    .insert(*source_result, Some(payload));
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

/// How one race of the Run's open sources ended.
pub(super) enum SourceRace {
    /// The schedule's selectable at this index completed first.
    Selected(usize),
    /// This source sealed first; the schedule records it before acceptance.
    Sealed(ToolCallId, crate::tool_run::SourceSeal),
    /// The turn's cancellation decided every open source.
    Cancelled,
}

/// Read a Resolved seal's retained result once, in a recorded step. Run
/// close releases the source's lease while this journal can still replay,
/// so a replay serves the canonical payload from the step, never the store.
async fn read_source_result(
    journal: &RunJournal<'_>,
    call_id: &ToolCallId,
    source: &AwaitEventKey,
    result: &MaterialRef,
    store: &dyn crate::store::ToolMaterialStore,
) -> Result<MaterialPayload, SingletonRunError> {
    use crate::tool_run::{MaterialHolder, RetainedBundle};
    let MaterialLocation::RetainedArtifact { artifact } = &result.location else {
        return Err(
            RuntimeEffectControllerError::from(MaterialRefusal::Missing {
                reference: Box::new(result.clone()),
            })
            .into(),
        );
    };
    let holder = MaterialHolder::Source {
        source: source.clone(),
    };
    let name = format!("run:source-material:{call_id}");
    let invocation = crate::RuntimeEffectInvocation::new(
        crate::EffectAddress::new(journal.scoped.execution_scope().clone(), &name)
            .map_err(RuntimeEffectControllerError::from)?,
        crate::RuntimeAttribution::default(),
        &name,
    );
    let outcome = journal
        .scoped
        .tool_effect(
            crate::RuntimeEffectEnvelope::new(
                invocation,
                crate::RuntimeEffectCommand::RestoreRunMaterial {
                    holder: holder.clone(),
                    bundles: vec![RetainedBundle {
                        holder,
                        artifact: artifact.clone(),
                        references: vec![result.clone()],
                        copy_bytes: 0,
                    }],
                    aliases: vec![result.clone()],
                    available: journal.materials.available.clone(),
                },
            ),
            crate::RuntimeEffectLocalExecutor::restore_run_material(store),
        )
        .await?;
    let crate::RuntimeEffectOutcome::RestoreRunMaterial { materials } = outcome else {
        return Err(RuntimeEffectControllerError::wrong_outcome(
            crate::RuntimeEffectKind::RestoreRunMaterial,
            outcome.kind(),
        )
        .into());
    };
    materials
        .into_iter()
        .find_map(|entry| match entry {
            MaterialEntry::Available { reference, payload } if reference == *result => {
                Some(*payload)
            }
            _ => None,
        })
        .ok_or_else(|| {
            RuntimeEffectControllerError::from(MaterialRefusal::Missing {
                reference: Box::new(result.clone()),
            })
            .into()
        })
}
