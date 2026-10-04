//! Physical capture and adoption use the shared K6 codec.

use super::*;

impl RunCoordinator<'_> {
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
            reason: cut.reason,
            events: self.journal.ledger.next_ordinal(),
            entries: self.journal.entries.clone(),
            attempts: self.attempts.clone(),
            material_aliases: self.journal.materials.entries.keys().cloned().collect(),
            material: Vec::new(),
            sources: self.sources.values().cloned().collect(),
            subscriptions: self
                .waiting
                .keys()
                .map(|id| crate::tool_run::SourceSubscription {
                    source: self.sources[id].source.clone(),
                    owner: self.journal.owner.clone(),
                    segment: self.journal.segment,
                })
                .collect(),
            owed_starts: self.journal.ledger.owed_starts(),
            owed_cancels: self.journal.ledger.owed_cancels(),
            state: crate::tool_run::StateFrontier {
                owner_segment: self.journal.segment,
                applied: self
                    .journal
                    .entries
                    .iter()
                    .flat_map(|entry| &entry.state)
                    .map(|state| state.ordinal)
                    .max(),
            },
            reserved_calls: self.journal.ledger.reserved_calls(),
            vm_continuation: false,
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
    /// predecessor keeps its lease until successor ownership commits.
    ///
    /// # Errors
    /// A store refusal leaves the continuation unpublished.
    pub async fn retain_cut(
        &self,
        transfer: &mut crate::tool_run::RunTransfer,
        store: &dyn crate::store::ToolMaterialStore,
    ) -> Result<(), SingletonRunError> {
        let payloads = self
            .journal
            .materials
            .entries
            .values()
            .filter_map(Clone::clone);
        if let Some(bundle) = crate::tool_run::MaterialBundle::of(payloads)? {
            transfer.material = vec![store.retain_material(&transfer.holder(), &bundle).await?];
        }
        for entry in &mut transfer.entries {
            entry
                .materials
                .retain(|entry| matches!(entry, MaterialEntry::Retired { .. }));
        }
        for entry in &mut transfer.attempts {
            entry.materials.clear();
        }
        transfer.check_capture(&crate::tool_run::Cut::request(transfer.reason).observe(0))?;
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
        handlers: std::sync::Arc<dyn SingletonToolHandlers>,
        clock: &dyn crate::Clock,
    ) -> Result<Self, SingletonRunError> {
        use crate::tool_run::{Cut, MaterialHolder, RunLifecycle};
        use futures_util::FutureExt;
        transfer.check_capture(&Cut::request(transfer.reason).observe(0))?;
        let ledger = transfer.ledger()?;
        let transfer = transfer.adopt(&owner, ledger.lifecycle(), successor)?;
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
            for bundle in &transfer.material {
                let acquired = store.acquire_material(&holder, bundle).await?;
                for alias in &transfer.material_aliases {
                    let Some(reference) = acquired.references.iter().find(|reference| {
                        reference.owner == alias.owner
                            && reference.role == alias.role
                            && reference.digest == alias.digest
                    }) else {
                        continue;
                    };
                    let payload = store
                        .read_material(
                            &holder,
                            reference,
                            &alias.owner,
                            &run.journal.materials.available,
                        )
                        .await?;
                    run.journal
                        .materials
                        .entries
                        .insert(alias.clone(), Some(payload));
                }
            }
        }
        for entry in &transfer.entries {
            run.journal
                .ledger
                .append(entry.record.segment, &entry.record)?;
            run.journal.materials.admit(entry.materials.clone())?;
            run.journal.records.push(entry.record.clone());
            run.journal.entries.push(entry.clone());
        }
        run.journal.ledger.admit_successor(successor);
        run.attempts.clone_from(&transfer.attempts);
        run.sources = transfer
            .sources
            .into_iter()
            .map(|source| (source.call_id.clone(), source))
            .collect();
        if let Some(plugins) = handlers.plugin_session() {
            if let Some(state) = &transfer.plugin_state {
                plugins
                    .hydrate_state(state)
                    .map_err(RuntimeEffectControllerError::from)?;
            }
            plugins.adopt_state_segment(successor);
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
                            Handlers::Owned(std::sync::Arc::clone(&handlers)),
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
                    call_id,
                    rank,
                    decision,
                    ..
                } => {
                    decisions.insert(call_id.clone(), (*rank, decision.clone()));
                }
                RunEvent::Presented {
                    call_id,
                    presentation,
                } => {
                    presented.insert(call_id.clone(), presentation.clone());
                }
                RunEvent::StartLaunched {
                    call_id,
                    process_id,
                    ..
                } => {
                    launched.insert(call_id.clone(), process_id.clone());
                }
                RunEvent::TimerElapsed { aggregate, leaf } => {
                    elapsed.insert((aggregate.clone(), *leaf));
                }
                _ => {}
            }
        }
        for (id, member) in members {
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
                                AttemptResult::Deferred { .. } => None,
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
                            handlers: Handlers::Owned(std::sync::Arc::clone(&handlers)),
                            decision: decision.clone(),
                            capture,
                        },
                    );
                }
            } else if let Some((attempt, AttemptResult::Deferred { .. })) = attempts.get(&id) {
                let request: SingletonPreparedRequest =
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
                    environment: run.environment.clone(),
                };
                run.waiting.insert(
                    id,
                    Waiting {
                        call,
                        member,
                        handlers: Handlers::Owned(std::sync::Arc::clone(&handlers)),
                        attempt: *attempt,
                    },
                );
            }
        }
        if run.journal.ledger.lifecycle() == RunLifecycle::Live {
            for event in events {
                if let RunEvent::AggregateAdmitted {
                    plan,
                    admitted_at_ms,
                } = event
                {
                    for (leaf, operand) in plan.leaves.iter().enumerate() {
                        let leaf = leaf as u32;
                        if let crate::tool_run::AggregateLeaf::Timer { duration_ms } = operand
                            && !elapsed.contains(&(plan.key.clone(), leaf))
                        {
                            scoped.admit_journal_write()?;
                            let timer = scoped.controller().start_run_retry(
                                admitted_at_ms
                                    .saturating_add(*duration_ms)
                                    .saturating_sub(clock.timestamp_ms()),
                            );
                            let handle = async move {
                                timer.await?;
                                Ok(parallel::Ready::Timer)
                            }
                            .boxed()
                            .shared();
                            run.timers.push(parallel::AggregateTimer {
                                key: plan.key.clone(),
                                leaf,
                                handle,
                            });
                        }
                    }
                }
            }
        }
        Ok(run)
    }
}
