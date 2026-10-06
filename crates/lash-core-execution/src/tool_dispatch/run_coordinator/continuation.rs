//! Physical capture and adoption use the shared K6 codec.

use super::*;

impl RunCoordinator<'_> {
    pub(crate) fn with_generation_cuts(mut self, enabled: bool) -> Self {
        self.observe_generation_cuts = enabled;
        self
    }

    // Only a served frame result owns cut truth; a replay never reads the mark.
    pub(super) fn accept_cut_request(
        &mut self,
        record: &RunRecord,
    ) -> Result<(), SingletonRunError> {
        if let Some(reason) = record.events.iter().find_map(|event| match event {
            RunEvent::CutChecked { reason } => Some(*reason),
            _ => None,
        }) {
            let cut = self.request_cut(reason);
            return Err(RunCutRefusal::AdmissionFrozen { reason: cut.reason }.into());
        }
        Ok(())
    }

    /// Freeze admission at this boundary, retaining the first requested reason.
    /// Requesting a physical cut never closes or cancels the logical Run.
    pub fn request_cut(&mut self, reason: crate::BoundaryReason) -> crate::tool_run::Cut {
        let cut = *self
            .cut
            .get_or_insert_with(|| crate::tool_run::Cut::request(reason));
        let observed = cut.observe(
            self.pending
                .len()
                .saturating_add(usize::from(self.active_frame || self.faulted)),
        );
        self.cut = Some(observed);
        observed
    }

    /// The current phase, derived from the handles still owed durable acceptance.
    #[must_use]
    pub fn cut(&self) -> Option<crate::tool_run::Cut> {
        self.cut.map(|cut| {
            cut.observe(
                self.pending
                    .len()
                    .saturating_add(usize::from(self.active_frame || self.faulted)),
            )
        })
    }

    /// Poll issued work through durable acceptance, without draining protected
    /// declarations or awaiting Deferred sources. Registered retry work belongs
    /// to the already admitted calls and keeps its recorded schedule.
    ///
    /// # Errors
    /// A missing request or a typed execution refusal. An invocation fault
    /// exports nothing; its original engine journal owns recovery.
    pub async fn quiesce(&mut self) -> Result<crate::tool_run::RunTransfer, SingletonRunError> {
        if self.cut.is_none() {
            return Err(RunCutRefusal::NotRequested.into());
        }
        while !self.pending.is_empty() {
            self.progress().await?;
        }
        self.capture_cut().map_err(Into::into)
    }

    /// Capture only durable receipts. This does not publish successor ownership.
    ///
    /// # Errors
    /// A missing request or an issued handle still awaiting durable acceptance.
    pub fn capture_cut(&self) -> Result<crate::tool_run::RunTransfer, RunCutRefusal> {
        if self.faulted || self.active_frame {
            return Err(RunCutRefusal::InvocationFailed);
        }
        let cut = self.cut().ok_or(RunCutRefusal::NotRequested)?;
        if cut.phase != crate::tool_run::CutPhase::Capturable {
            return Err(RunCutRefusal::NotQuiescent);
        }
        Ok(crate::tool_run::RunTransfer {
            owner: self.journal.owner.clone(),
            from: self.journal.segment,
            entries: self.journal.entries.clone(),
            attempts: self.attempts.clone(),
            material_aliases: self
                .journal
                .materials
                .entries
                .iter()
                .filter_map(|(reference, payload)| payload.as_ref().map(|_| reference.clone()))
                .collect(),
            material: Vec::new(),
            sources: self.sources.values().cloned().collect(),
            subscriptions: self
                .waiting
                .keys()
                .map(|id| self.sources[id].source.clone())
                .collect(),
            environment: self.environment.clone(),
            plugin_state: self
                .handlers
                .values()
                .find_map(|handlers| handlers.get().plugin_session())
                .map(|plugins| plugins.export_state()),
        })
    }
}

impl<'a> RunCoordinator<'a> {
    /// Retain canonical payloads before publishing a continuation. The
    /// predecessor keeps its lease until successor ownership commits. The old
    /// invocation replays the receipt and its own journaled payloads, never a
    /// retention under the now-ended predecessor holder.
    ///
    /// # Errors
    /// A store refusal leaves the continuation unpublished.
    pub async fn retain_cut(
        &mut self,
        transfer: &mut crate::tool_run::RunTransfer,
        store: &dyn crate::store::ToolMaterialStore,
    ) -> Result<(), SingletonRunError> {
        // The extra command belongs only to a real, quiescent physical cut.
        let capture = self.capture_cut()?;
        if transfer.owner != capture.owner || transfer.from != capture.from {
            return Err(crate::tool_run::ContinuationRefusal::ForeignOwner.into());
        }
        let retained = self
            .journal
            .records
            .iter()
            .filter(|record| record.segment == self.journal.segment)
            .flat_map(|record| &record.events)
            .find_map(|event| match event {
                RunEvent::CutRetained { material } => Some(material.clone()),
                _ => None,
            });
        let material = match retained {
            Some(material) => material,
            None => {
                let payloads: Vec<_> = self
                    .journal
                    .materials
                    .entries
                    .values()
                    .filter_map(Clone::clone)
                    .collect();
                let holder = transfer.holder();
                let record = self.journal.record(Vec::new());
                let name = format!(
                    "lash:run:cut:retain:{}:{}",
                    record.segment.0, record.first.0
                );
                let recorded = self
                    .journal
                    .append(
                        name,
                        Box::pin(async move {
                            let material = match crate::tool_run::MaterialBundle::of(payloads)
                                .map_err(|error| error.to_string())?
                            {
                                Some(bundle) => vec![
                                    store
                                        .retain_material(&holder, &bundle)
                                        .await
                                        .map_err(|error| error.to_string())?,
                                ],
                                None => Vec::new(),
                            };
                            Ok(RunJournalEntry {
                                record: RunRecord {
                                    events: vec![RunEvent::CutRetained { material }],
                                    ..record
                                },
                                materials: Vec::new(),
                                state: Vec::new(),
                            })
                        }),
                    )
                    .await?;
                match recorded.events.into_iter().next() {
                    Some(RunEvent::CutRetained { material }) => material,
                    _ => {
                        return Err(crate::tool_run::ContinuationRefusal::UnretainedMaterial.into());
                    }
                }
            }
        };
        transfer.material = material.into_iter().map(Into::into).collect();
        transfer.entries.clone_from(&self.journal.entries);
        for entry in &mut transfer.entries {
            entry
                .materials
                .retain(|entry| matches!(entry, MaterialEntry::Retired { .. }));
        }
        for entry in &mut transfer.attempts {
            entry.materials.clear();
        }
        transfer.check_capture(crate::tool_run::CutPhase::Capturable)?;
        Ok(())
    }

    /// Rebuild a transferred Run under its admitted successor's controller.
    /// No preparation, body, check, reducer or completed presentation runs.
    /// The owner publishes successor ownership before releasing the old lease.
    ///
    /// # Errors
    /// A typed owner, segment, receipt, binding or retained-material refusal.
    pub async fn adopt(
        scoped: &'a ScopedEffectController<'a>,
        owner: EffectOpener,
        successor: SegmentOrdinal,
        available: Vec<PluginRevision>,
        transfer: crate::tool_run::RunTransfer,
        handlers: std::sync::Arc<dyn SingletonToolHandlers + 'a>,
        clock: &dyn crate::Clock,
    ) -> Result<Self, SingletonRunError> {
        use crate::tool_run::{CutPhase, MaterialHolder, RunLifecycle};
        transfer.check_capture(CutPhase::Capturable)?;
        let ledger = transfer.ledger()?;
        let adopted = transfer.adopt(&owner, ledger.lifecycle(), successor)?;
        let ledger = adopted.ledger()?;
        let transfer = adopted.transfer;
        let mut run = Self::open(scoped, owner, successor, available);
        run.environment.clone_from(&transfer.environment);
        if !transfer.material.is_empty() {
            let store = handlers
                .tool_material_store()
                .ok_or(crate::tool_run::ContinuationRefusal::UnretainedMaterial)?;
            let holder = MaterialHolder::Segment {
                opener: run.journal.owner.clone(),
                segment: successor,
            };
            let name = format!("run:restore-material:{}", successor.0);
            let invocation = crate::RuntimeEffectInvocation::new(
                crate::EffectAddress::new(scoped.execution_scope().clone(), &name)
                    .map_err(crate::RuntimeEffectControllerError::from)?,
                crate::RuntimeAttribution::default(),
                &name,
            );
            let outcome = scoped
                .execute_effect(
                    crate::RuntimeEffectEnvelope::new(
                        invocation,
                        crate::RuntimeEffectCommand::RestoreRunMaterial {
                            holder,
                            bundles: transfer
                                .material
                                .iter()
                                .map(|bundle| bundle.held_by(transfer.holder()))
                                .collect(),
                            aliases: transfer.material_aliases.clone(),
                            available: run.journal.materials.available.clone(),
                        },
                    ),
                    crate::RuntimeEffectLocalExecutor::restore_run_material(store),
                )
                .await?;
            let crate::RuntimeEffectOutcome::RestoreRunMaterial { materials } = outcome else {
                return Err(crate::RuntimeEffectControllerError::wrong_outcome(
                    crate::RuntimeEffectKind::RestoreRunMaterial,
                    outcome.kind(),
                )
                .into());
            };
            run.journal.materials.admit(materials)?;
        }
        for entry in &transfer.entries {
            run.journal.materials.admit(entry.materials.clone())?;
            run.journal.records.push(entry.record.clone());
            run.journal.entries.push(entry.clone());
        }
        run.journal.ledger = ledger;
        run.attempts.clone_from(&transfer.attempts);
        run.sources = transfer
            .sources
            .into_iter()
            .map(|source| (source.call_id.clone(), source))
            .collect();
        if let Some(plugins) = handlers.plugin_session() {
            // Turn preparation already admitted publication ownership. The
            // transferred snapshot supplies values and receipts, but cannot
            // move that ownership back to its predecessor's physical turn.
            let publication_segment = plugins
                .export_state()
                .plugins
                .values()
                .map(|namespace| namespace.publication.owner_segment)
                .max()
                .unwrap_or_default()
                .max(successor);
            if let Some(state) = &transfer.plugin_state {
                plugins
                    .hydrate_state(state)
                    .map_err(RuntimeEffectControllerError::from)?;
            }
            plugins.adopt_state_segment(publication_segment);
        }
        let events: Vec<_> = run
            .journal
            .records
            .iter()
            .flat_map(|record| &record.events)
            .cloned()
            .collect();
        let mut members = BTreeMap::new();
        let mut decisions = BTreeMap::new();
        let mut presented = BTreeMap::new();
        let mut launched = BTreeMap::new();
        let mut receipts = BTreeMap::new();
        let mut attempts = BTreeMap::new();
        let mut elapsed = std::collections::BTreeSet::new();
        for event in &events {
            match event {
                RunEvent::Admitted { round } => {
                    for member in &round.members {
                        member
                            .binding
                            .require_available(&run.journal.materials.available)
                            .map_err(|cause| AdmissionRefusal::BindingUnavailable {
                                member: 0,
                                cause: Box::new(cause),
                            })?;
                        members.insert(member.call_id.clone(), member.clone());
                        run.handlers.insert(
                            member.call_id.clone(),
                            Handlers(std::sync::Arc::clone(&handlers)),
                        );
                    }
                }
                RunEvent::AttemptRecorded {
                    call_id,
                    attempt,
                    result,
                } => {
                    attempts.insert(call_id.clone(), (*attempt, result.clone()));
                }
                RunEvent::Decided {
                    call_id, decision, ..
                } => {
                    let rank = run
                        .journal
                        .ledger
                        .decision_rank(call_id)
                        .ok_or_else(|| boundary(call_id))?;
                    decisions.insert(call_id.clone(), (rank, decision.clone()));
                }
                RunEvent::Presented {
                    call_id,
                    presentation,
                    ..
                } => {
                    presented.insert(call_id.clone(), presentation.clone());
                }
                RunEvent::StartLaunched {
                    call_id,
                    process_id,
                    receipt,
                    ..
                } => {
                    launched.insert(call_id.clone(), process_id.clone());
                    if let Some(receipt) = receipt {
                        receipts.insert(call_id.clone(), receipt.clone());
                    }
                }
                RunEvent::TimerElapsed { aggregate, leaf } => {
                    elapsed.insert((aggregate.clone(), *leaf));
                }
                _ => {}
            }
        }
        for (id, member) in members {
            let recorded: RecordedPreparedRequest =
                run.journal.materials.decode(&member.request)?;
            let request = SingletonPreparedRequest {
                arguments: recorded.arguments,
                environment: recorded.environment,
                prepared: recorded.prepared,
                state_snapshot: recorded
                    .state_snapshot
                    .as_ref()
                    .map(|reference| run.journal.materials.snapshot(reference))
                    .transpose()?,
                isolation: recorded.isolation,
            };
            handlers.restore_request(&id, &member.binding, &request)?;
            // A launched declared start's receipt is the call's until a
            // presentation records it.
            let presented_final = presented.contains_key(&id)
                && matches!(decisions.get(&id), Some((_, CallDecision::Final { .. })));
            if !presented_final && let Some(receipt) = receipts.get(&id) {
                let receipt: RealizationReceipt = run.journal.materials.decode(receipt)?;
                handlers.adopt_realization(&id, &receipt)?;
            }
            if let Some((rank, decision)) = decisions.get(&id) {
                if let Some(presentation) = presented.get(&id) {
                    run.presented.insert(
                        id.clone(),
                        PresentedCall {
                            decision: decision.clone(),
                            presentation: presentation.clone(),
                            launched: launched.get(&id).cloned(),
                        },
                    );
                    super::drain::restore_contributions(&run.journal, &id, handlers.as_ref())?;
                    match run.terminal(&id)? {
                        SingletonTerminal::Final {
                            capture,
                            presentation,
                            ..
                        } => {
                            handlers.incorporate(&id, Some(&capture), Some(&presentation), false)?
                        }
                        SingletonTerminal::Withheld { .. } => {
                            handlers.incorporate(&id, None, None, false)?
                        }
                        SingletonTerminal::Deferred { .. } => return Err(boundary(&id)),
                    }
                } else {
                    let reference = match decision {
                        CallDecision::Final {
                            source: ResultSource::Attempt { attempt },
                            ..
                        } => run
                            .attempts
                            .iter()
                            .find(|entry| entry.call_id == id && entry.attempt == *attempt)
                            .and_then(|entry| match &entry.result {
                                AttemptResult::Done { output }
                                | AttemptResult::Failed { output, .. } => Some(output.clone()),
                                AttemptResult::Deferred { .. }
                                | AttemptResult::DeferredStart { .. }
                                | AttemptResult::Pending { .. } => None,
                            })
                            .or_else(|| {
                                attempts.get(&id).and_then(|(_, result)| match result {
                                    AttemptResult::Done { output }
                                    | AttemptResult::Failed { output, .. } => Some(output.clone()),
                                    _ => None,
                                })
                            }),
                        CallDecision::Final {
                            source: ResultSource::Cached,
                            ..
                        } => member
                            .checks
                            .winner()
                            .and_then(|reply| match &reply.verdict {
                                BeforeCheckVerdict::Cached { result } => Some(result.clone()),
                                _ => None,
                            }),
                        CallDecision::Final {
                            source: ResultSource::DeferredCompletion { resolved, .. },
                            ..
                        } => Some(*resolved.clone()),
                        _ => None,
                    };
                    let capture = reference
                        .as_ref()
                        .map(|reference| run.journal.materials.decode(reference))
                        .transpose()?;
                    run.owed.insert(
                        *rank,
                        Owed {
                            call_id: id.clone(),
                            handlers: Handlers(std::sync::Arc::clone(&handlers)),
                            decision: decision.clone(),
                            capture,
                        },
                    );
                }
            } else if let Some((
                attempt,
                result @ (AttemptResult::Deferred { .. }
                | AttemptResult::DeferredStart { .. }
                | AttemptResult::Pending { .. }),
            )) = attempts.get(&id)
            {
                let request: RecordedPreparedRequest =
                    run.journal.materials.decode(&member.request)?;
                let call = SingletonToolCall {
                    owner: run.journal.owner.clone(),
                    segment: successor,
                    call_id: id.clone(),
                    tool_name: member.tool_name.clone(),
                    arguments: request.arguments,
                    declaration: member.declaration.clone(),
                    binding: member.binding.clone(),
                    available: run.journal.materials.available.clone(),
                    cancel: member.policy.cancel,
                    environment: request.environment,
                };
                let start = match result {
                    AttemptResult::Pending {
                        start: Some(start), ..
                    } => Some(SingletonStart {
                        start_key: start.start_key.clone(),
                        obligation: start.obligation.clone(),
                    }),
                    AttemptResult::DeferredStart {
                        start_key,
                        obligation,
                        ..
                    } => Some(SingletonStart {
                        start_key: start_key.clone(),
                        obligation: obligation.clone(),
                    }),
                    _ => None,
                };
                let pending_start = start.is_some() && !launched.contains_key(&id);
                let waiting = Waiting {
                    call,
                    member,
                    handlers: Handlers(std::sync::Arc::clone(&handlers)),
                    attempt: *attempt,
                    start,
                };
                if pending_start {
                    let source = match result {
                        AttemptResult::DeferredStart { source, .. }
                        | AttemptResult::Pending { source, .. } => source,
                        _ => return Err(crate::tool_run::ContinuationRefusal::ForeignSource.into()),
                    };
                    run.pending_starts.insert(id, (waiting, source.clone()));
                } else {
                    if let AttemptResult::Pending { metadata, .. } = result {
                        let pending: RecordedPending = run.journal.materials.decode(metadata)?;
                        waiting
                            .handlers
                            .get()
                            .arm_pending(&run.sources[&id], &pending.completion)
                            .await?;
                    }
                    run.waiting.insert(id, waiting);
                }
            }
        }
        if run.journal.ledger.lifecycle() == RunLifecycle::Live {
            for event in &events {
                if let RunEvent::AggregateAdmitted { plan, .. } = event {
                    run.register_aggregate_timers(plan, clock, &elapsed)?;
                }
            }
        }
        Ok(run)
    }
}

// Admission and consumption borrow their existing SDK record. Only a fresh
// callback reads authority, and only an accepted mark replaces the frame result.
pub(super) async fn generation_cut_entry(
    controller: &dyn crate::RuntimeEffectController,
    enabled: bool,
    mut record: RunRecord,
) -> Result<Option<RunJournalEntry>, String> {
    if enabled
        && let Some(reason) = controller
            .peek_run_cut()
            .await
            .map_err(|error| error.to_string())?
    {
        record.events = vec![RunEvent::CutChecked { reason }];
        record.trace = None;
        return Ok(Some(RunJournalEntry {
            record,
            materials: Vec::new(),
            state: Vec::new(),
        }));
    }
    Ok(None)
}
