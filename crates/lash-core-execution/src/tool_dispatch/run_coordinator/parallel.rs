//! Eager X handles and the recorded K9 selection/registration schedule.
use super::*;
use crate::tool_dispatch::SelectKey;
use crate::tool_run::{RecordedRetryPolicy, RunAttemptEntry};
use futures_util::future::{BoxFuture, FutureExt, Shared};

pub(super) type KeyHandle<'a> =
    Shared<BoxFuture<'a, Result<SelectKey, RuntimeEffectControllerError>>>;

pub(super) type Handle<'a> = Shared<BoxFuture<'a, Result<Ready, RuntimeEffectControllerError>>>;

#[derive(Clone)]
pub(super) enum Ready {
    Attempt(std::sync::Arc<RunAttemptEntry>),
    /// The durable timer fired.
    Timer,
    StartPrepared(std::sync::Arc<crate::tool_dispatch::RunStartPrepared>),
    /// A realization invocation answered its receipt.
    Realization(std::sync::Arc<RealizationReceipt>),
    /// An open source's seal.
    Sealed(crate::tool_run::SourceSeal),
}

/// A final's issued realization whose receipt a schedule window selects and
/// records (ADR 0130).
pub(super) struct Realizing<'a> {
    /// The engine notification identity the controller returned with the
    /// attach; the schedule holds it beside the source and never polls it.
    pub key: Shared<crate::tool_dispatch::RunSelectKey<'a>>,
    /// The attach's receipt source, one of the schedule's selected works.
    pub handle: Handle<'a>,
}

pub(super) struct AggregateTimer<'a> {
    pub key: String,
    pub leaf: u32,
    pub handle: Handle<'a>,
    pub select_key: KeyHandle<'a>,
}

#[derive(Clone)]
enum SelectedWork<'a> {
    StartPrepared {
        call_id: ToolCallId,
    },
    Call {
        work: std::sync::Arc<Work<'a>>,
        ordinal: AttemptOrdinal,
        timer: bool,
        delay: u64,
    },
    AggregateTimer {
        key: String,
        leaf: u32,
    },
    Realization {
        call_id: ToolCallId,
    },
    /// An open source's seal, which the source race answered directly.
    Source {
        call_id: ToolCallId,
    },
}

impl SelectedWork<'_> {
    fn recorded_by(&self, event: &RunEvent) -> bool {
        match (self, event) {
            (
                Self::StartPrepared { call_id },
                RunEvent::StartLaunched {
                    call_id: recorded, ..
                },
            )
            | (
                Self::Realization { call_id },
                RunEvent::Realized {
                    call_id: recorded, ..
                },
            ) => call_id == recorded,
            (
                Self::AggregateTimer { key, leaf },
                RunEvent::TimerElapsed {
                    aggregate,
                    leaf: recorded,
                },
            ) => key == aggregate && leaf == recorded,
            (
                Self::Call {
                    work,
                    ordinal,
                    timer: false,
                    ..
                },
                RunEvent::AttemptRecorded {
                    call_id, attempt, ..
                },
            ) => work.call.call_id == *call_id && ordinal == attempt,
            (
                Self::Call {
                    work,
                    ordinal,
                    timer: true,
                    ..
                },
                RunEvent::RetryScheduled {
                    call_id, failed, ..
                },
            ) => work.call.call_id == *call_id && ordinal == failed,
            (
                Self::Call {
                    work, timer: true, ..
                },
                RunEvent::Decided {
                    call_id,
                    decision: CallDecision::Cancelled,
                    ..
                },
            ) => work.call.call_id == *call_id,
            _ => false,
        }
    }
}

struct Work<'a> {
    call: SingletonToolCall,
    member: AdmittedCall,
    request: SingletonPreparedRequest,
    handlers: std::sync::Arc<dyn SingletonToolHandlers + 'a>,
}

pub(super) struct Pending<'a> {
    work: std::sync::Arc<Work<'a>>,
    capture: Option<SingletonCapture>,
    ordinal: AttemptOrdinal,
    timer: bool,
    handle: Handle<'a>,
    select_key: KeyHandle<'a>,
}

fn captured(
    entry: &RunAttemptEntry,
    owner: &MaterialOwner,
    available: &[PluginRevision],
) -> Result<Option<SingletonCapture>, RuntimeEffectControllerError> {
    let output = match &entry.result {
        AttemptResult::Deferred { .. }
        | AttemptResult::DeferredStart { .. }
        | AttemptResult::Pending { .. } => return Ok(None),
        AttemptResult::Done { output } | AttemptResult::Failed { output, .. } => output,
    };
    let mut materials = Materials {
        owner: owner.clone(),
        available: available.to_vec(),
        entries: BTreeMap::new(),
        snapshots: BTreeMap::new(),
    };
    materials.admit(entry.materials.clone())?;
    materials.decode(output).map(Some)
}

impl<'a> RunCoordinator<'a> {
    /// Admit the whole round, register first attempts in admission order,
    /// handing the issued handles to this coordinator. The consumer calls
    /// `progress` to accept recorded selections, or requests a cut and calls
    /// `quiesce`. Returning from admission is not a physical return: every
    /// issued X still belongs to this invocation until durable acceptance.
    /// An invocation fault leaves its unrecorded work to engine recovery.
    ///
    /// # Errors
    /// A typed admission, journal, material or Run-fold refusal.
    pub async fn start_round(
        &mut self,
        calls: &[SingletonToolCall],
        capacity: crate::tool_run::CapacityScope,
        handlers: std::sync::Arc<dyn SingletonToolHandlers + 'a>,
        retry: RecordedRetryPolicy,
    ) -> Result<Vec<(ToolCallId, DecidedCall)>, SingletonRunError> {
        self.begin_frame()?;
        let result = self
            .bodies
            .clone()
            .beside(self.start_round_inner(calls, capacity, handlers, retry, None))
            .await;
        self.active_frame = false;
        self.note_fault(&result);
        result
    }

    pub(super) async fn start_round_inner(
        &mut self,
        calls: &[SingletonToolCall],
        capacity: crate::tool_run::CapacityScope,
        handlers: std::sync::Arc<dyn SingletonToolHandlers + 'a>,
        retry: RecordedRetryPolicy,
        aggregate: Option<(&crate::tool_run::AggregatePlan, &dyn crate::Clock)>,
    ) -> Result<Vec<(ToolCallId, DecidedCall)>, SingletonRunError> {
        let admitted = self
            .admit_round(calls, handlers.as_ref(), retry, aggregate, capacity)
            .await?;
        let mut decisions = Vec::new();
        for call in calls {
            self.handlers.insert(
                call.call_id.clone(),
                Handlers(std::sync::Arc::clone(&handlers)),
            );
        }
        // Even an immediate/cached winner cannot bypass registration of a
        // sibling whose admission already owns an executable attempt.
        for (index, (member, request)) in admitted.iter().enumerate() {
            if member.selection() == BeforeSelection::Execute {
                let (select_key, handle) = self.issue_attempt(
                    &calls[index],
                    member,
                    request,
                    std::sync::Arc::clone(&handlers),
                    AttemptOrdinal::FIRST,
                )?;
                self.pending.push(Pending {
                    work: std::sync::Arc::new(Work {
                        call: calls[index].clone(),
                        member: member.clone(),
                        request: request.clone(),
                        handlers: std::sync::Arc::clone(&handlers),
                    }),
                    capture: None,
                    ordinal: AttemptOrdinal::FIRST,
                    timer: false,
                    handle,
                    select_key,
                });
            }
        }
        if let Some((plan, clock)) = aggregate {
            self.register_aggregate_timers(plan, clock, &std::collections::BTreeSet::new())?;
        }
        for (index, (member, _)) in admitted.iter().enumerate() {
            if member.selection() == BeforeSelection::Execute {
                continue;
            }
            let candidate = if member.selection() == BeforeSelection::Cached {
                let Some(BeforeCheckVerdict::Cached { result }) =
                    member.checks.winner().map(|reply| &reply.verdict)
                else {
                    return Err(boundary(&calls[index].call_id));
                };
                let capture = if member.declaration.isolated {
                    SingletonCapture::Refused {
                        refusal: DeclarationRefusal::InlineOutcomeFromIsolated,
                    }
                } else {
                    self.journal.materials.decode(result)?
                };
                Some((ResultSource::Cached, capture))
            } else {
                None
            };
            let decision = self
                .decide_candidate(
                    &calls[index],
                    Handlers(std::sync::Arc::clone(&handlers)),
                    member,
                    candidate,
                )
                .await?;
            decisions.push((calls[index].call_id.clone(), decision));
        }
        Ok(decisions)
    }

    pub(super) fn register_aggregate_timers(
        &mut self,
        plan: &crate::tool_run::AggregatePlan,
        clock: &dyn crate::Clock,
        elapsed: &std::collections::BTreeSet<(String, u32)>,
    ) -> Result<(), SingletonRunError> {
        let admitted_at_ms = self
            .journal
            .records
            .iter()
            .flat_map(|record| &record.events)
            .find_map(|event| match event {
                RunEvent::AggregateAdmitted {
                    plan: recorded,
                    admitted_at_ms,
                } if recorded.key == plan.key => Some(*admitted_at_ms),
                _ => None,
            })
            .ok_or_else(|| RunEventRefusal::UnknownAggregate {
                key: plan.key.clone(),
            })?;
        for (index, leaf) in plan.leaves.iter().enumerate() {
            if let crate::tool_run::AggregateLeaf::Timer { duration_ms } = leaf
                && !elapsed.contains(&(plan.key.clone(), index as u32))
            {
                let deadline = admitted_at_ms.saturating_add(*duration_ms);
                self.journal.scoped.admit_journal_write()?;
                let timer = self
                    .journal
                    .scoped
                    .controller()
                    .start_run_retry(deadline.saturating_sub(clock.timestamp_ms()));
                let select_key = timer.key.shared();
                self.journal.selection.pending.push(select_key.clone());
                let handle = async move {
                    timer.value.await?;
                    Ok(Ready::Timer)
                }
                .boxed()
                .shared();
                self.timers.push(AggregateTimer {
                    key: plan.key.clone(),
                    leaf: index as u32,
                    handle,
                    select_key,
                });
            }
        }
        Ok(())
    }

    /// Progress one recorded selection, including retry registration, through
    /// durable acceptance. A retry step has no final decision yet.
    /// Dropping this future is an invocation fault, never a successful cut.
    ///
    /// # Errors
    /// A typed journal, material or Run-fold refusal.
    pub async fn progress(
        &mut self,
    ) -> Result<Option<(ToolCallId, DecidedCall)>, SingletonRunError> {
        self.begin_frame()?;
        let result = self.bodies.clone().beside(self.progress_inner()).await;
        self.active_frame = false;
        self.note_fault(&result);
        result
    }

    pub(super) async fn progress_inner(
        &mut self,
    ) -> Result<Option<(ToolCallId, DecidedCall)>, SingletonRunError> {
        self.progress_with_presentation(&mut None).await
    }

    pub(super) async fn progress_with_presentation(
        &mut self,
        presentation: &mut Option<drain::PendingPresentation<'a>>,
    ) -> Result<Option<(ToolCallId, DecidedCall)>, SingletonRunError> {
        self.schedule_window(presentation, false).await
    }

    /// Whether this window races the Run's open sources: always when they
    /// are all it waits for, and beside other work only while the Run is
    /// live and no cut is requested, since Closing and a cut's quiesce
    /// decide sources themselves.
    fn races_sources(&self, sources_alone: bool) -> bool {
        !self.waiting.is_empty()
            && (sources_alone
                || (self.cut.is_none()
                    && self.journal.ledger.lifecycle() == crate::tool_run::RunLifecycle::Live))
    }

    /// One recorded selection over every issued body, timer, realization,
    /// presentation and, per [`Self::races_sources`], open source.
    pub(super) async fn schedule_window(
        &mut self,
        presentation: &mut Option<drain::PendingPresentation<'a>>,
        sources_alone: bool,
    ) -> Result<Option<(ToolCallId, DecidedCall)>, SingletonRunError> {
        let sources = self.races_sources(sources_alone);
        if presentation.as_ref().is_some_and(|pending| {
            pending.handle.is_none() && !self.realizing.contains_key(&pending.call_id)
        }) {
            let pending = presentation.take().ok_or(RunEventRefusal::EmptyRecord)?;
            self.present_pending(pending).await?;
            return Ok(None);
        }
        if self.pending.is_empty()
            && self.timers.is_empty()
            && self.realizing.is_empty()
            && presentation.is_none()
            && !sources
        {
            return Ok(None);
        }
        let mut decision = None;
        {
            let record = self.journal.record(Vec::new());
            let aborted = self.journal.ledger.aborted()
                || self.journal.ledger.lifecycle() != crate::tool_run::RunLifecycle::Live;
            let mut choices: Vec<_> = self
                .pending
                .iter()
                .map(|entry| {
                    (
                        entry.select_key.clone(),
                        entry.handle.clone(),
                        SelectedWork::Call {
                            work: std::sync::Arc::clone(&entry.work),
                            ordinal: entry.ordinal,
                            timer: entry.timer,
                            delay: backoff(
                                &entry.work.member.policy.retry,
                                entry.ordinal,
                                entry.capture.as_ref(),
                            ),
                        },
                    )
                })
                .collect();
            choices.extend(self.timers.iter().map(|timer| {
                (
                    timer.select_key.clone(),
                    timer.handle.clone(),
                    SelectedWork::AggregateTimer {
                        key: timer.key.clone(),
                        leaf: timer.leaf,
                    },
                )
            }));
            choices.extend(self.realizing.iter().map(|(call_id, realizing)| {
                (
                    realizing.key.clone(),
                    realizing.handle.clone(),
                    SelectedWork::Realization {
                        call_id: call_id.clone(),
                    },
                )
            }));
            if let Some(pending) = presentation.as_ref()
                && let (Some(key), Some(handle)) = (&pending.select_key, &pending.handle)
            {
                choices.push((
                    key.clone(),
                    handle.clone(),
                    SelectedWork::StartPrepared {
                        call_id: pending.call_id.clone(),
                    },
                ));
            }
            let name = format!("lash:run:schedule:{}", record.first.0);
            let address = crate::EffectAddress::new(
                self.journal.scoped.execution_scope().clone(),
                name.clone(),
            )
            .map_err(|error| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeToolRunShape,
                    error.to_string(),
                )
            })?;
            let mut publications = Vec::new();
            for entry in &self.pending {
                if let Some(plugins) = entry.work.handlers.plugin_session()
                    && !publications
                        .iter()
                        .any(|(held, _)| std::sync::Arc::ptr_eq(held, &plugins))
                {
                    let publication = crate::plugin::EffectPublication::begin(
                        std::sync::Arc::clone(&plugins),
                        address.clone(),
                    );
                    publications.push((plugins, publication));
                }
            }
            let owner = self.journal.materials.owner.clone();
            let available = self.journal.materials.available.clone();

            // Every recorded choice has a completed VM notification. Replay
            // can resolve this single combinator before registering D; an
            // unfinished sibling never gets an individual await.
            let mut keys = Vec::with_capacity(choices.len());
            for (key, _, _) in &choices {
                keys.push(key.clone().await?);
            }
            self.journal.selection.retain(&keys);
            let index = |chosen: usize| {
                keys.get(chosen)
                    .copied()
                    .ok_or_else(|| selection_boundary("selection index exceeds its sources"))
            };
            // A source that sealed first has no issued notification: its
            // seal is the selection, and D records it before acceptance.
            let won = match self.journal.selection.acknowledged.pop_front() {
                Some(key) => Ok(key),
                None if sources => match Box::pin(self.race_sources(keys.clone())).await? {
                    deferred::SourceRace::Selected(chosen) => Ok(index(chosen)?),
                    deferred::SourceRace::Sealed(call_id, seal) => Err((call_id, seal)),
                    deferred::SourceRace::Cancelled => return Ok(None),
                },
                None => Ok(index(
                    self.journal
                        .scoped
                        .controller()
                        .select_run_sources(keys.clone())
                        .await?,
                )?),
            };
            let (chosen, ready, selected_work) = match won {
                Ok(chosen_key) => {
                    let chosen = keys
                        .iter()
                        .position(|key| *key == chosen_key)
                        .ok_or_else(|| selection_boundary("queued acknowledgment has no source"))?;
                    (
                        Some((chosen, chosen_key)),
                        choices[chosen].1.clone().await?,
                        choices[chosen].2.clone(),
                    )
                }
                Err((call_id, seal)) => {
                    (None, Ready::Sealed(seal), SelectedWork::Source { call_id })
                }
            };
            let step = Box::pin(async move {
                let (work, ordinal, timer, delay) = match selected_work {
                    SelectedWork::Source { call_id } => {
                        let Ready::Sealed(seal) = ready else {
                            return Err(format!("a source returned {}", describe(&ready)));
                        };
                        return Ok(RunJournalEntry {
                            record: RunRecord {
                                events: vec![RunEvent::SourceSealed { call_id, seal }],
                                ..record
                            },
                            materials: Vec::new(),
                            state: Vec::new(),
                        });
                    }
                    SelectedWork::StartPrepared { call_id } => {
                        let Ready::StartPrepared(parts) = ready else {
                            return Err("a preparation returned an X receipt".to_owned());
                        };
                        if !matches!(parts.events.as_slice(), [RunEvent::StartLaunched { call_id: id, .. }, RunEvent::StartDischarged { call_id: discharged, .. }] if *id == call_id && *discharged == call_id)
                        {
                            return Err("a preparation returned another call".to_owned());
                        }
                        return Ok(RunJournalEntry {
                            record: RunRecord {
                                events: parts.events.clone(),
                                ..record
                            },
                            materials: Vec::new(),
                            state: Vec::new(),
                        });
                    }
                    SelectedWork::Realization { call_id } => {
                        let Ready::Realization(receipt) = ready else {
                            return Err(format!("a realization returned {}", describe(&ready)));
                        };
                        let (reference, material) = mint(
                            &owner,
                            MaterialRole::RealizationReceipt,
                            encode(receipt.as_ref())?,
                        )?;
                        return Ok(RunJournalEntry {
                            record: RunRecord {
                                events: vec![RunEvent::Realized {
                                    call_id,
                                    receipt: reference,
                                }],
                                ..record
                            },
                            materials: vec![material],
                            state: Vec::new(),
                        });
                    }
                    SelectedWork::AggregateTimer { key, leaf } => {
                        if !matches!(ready, Ready::Timer) {
                            return Err(format!("aggregate timer returned {}", describe(&ready)));
                        }
                        return Ok(RunJournalEntry {
                            record: RunRecord {
                                events: vec![RunEvent::TimerElapsed {
                                    aggregate: key,
                                    leaf,
                                }],
                                ..record
                            },
                            materials: Vec::new(),
                            state: Vec::new(),
                        });
                    }
                    SelectedWork::Call {
                        work,
                        ordinal,
                        timer,
                        delay,
                    } => (work, ordinal, timer, delay),
                };
                let call = &work.call;
                let member = &work.member;
                let handlers = work.handlers.as_ref();
                let call_id = call.call_id.clone();
                let mut record = record;
                match ready {
                    Ready::Attempt(entry) => {
                        if timer || entry.call_id != call_id || entry.attempt != ordinal {
                            return Err("an X handle returned a different attempt".to_owned());
                        }
                        let capture = captured(&entry, &owner, &available)
                            .map_err(|error| error.to_string())?;
                        let retryable = matches!(
                            &entry.result,
                            AttemptResult::Failed {
                                retryable: true,
                                ..
                            }
                        );
                        record.events.push(RunEvent::AttemptRecorded {
                            call_id: call_id.clone(),
                            attempt: ordinal,
                            result: entry.result.clone(),
                        });
                        let Some(capture) = capture else {
                            return Ok(RunJournalEntry {
                                record,
                                materials: Vec::new(),
                                state: Vec::new(),
                            });
                        };
                        if retryable
                            && eligible(&member.policy.retry, ordinal)
                            && !aborted
                            && !handlers.run_cancel_requested().await?
                        {
                            record.events.push(RunEvent::RetryTimerRegistered {
                                call_id,
                                failed: ordinal,
                                next: ordinal.next().ok_or("attempt ordinal exhausted")?,
                                backoff_ms: backoff(&member.policy.retry, ordinal, Some(&capture)),
                            });
                            Ok(RunJournalEntry {
                                record,
                                materials: Vec::new(),
                                state: Vec::new(),
                            })
                        } else {
                            decision_entry(
                                call,
                                handlers,
                                member,
                                Some((ResultSource::Attempt { attempt: ordinal }, capture)),
                                DecisionSlot {
                                    record,
                                    address,
                                    aborted,
                                },
                            )
                            .await
                        }
                    }
                    Ready::StartPrepared(_) => Err("an X returned a preparation".to_owned()),
                    Ready::Realization(_) => Err("an X returned a realization receipt".to_owned()),
                    Ready::Sealed(_) => Err("an X returned a source seal".to_owned()),
                    Ready::Timer => {
                        if !timer {
                            return Err("an X handle returned a timer wake".to_owned());
                        }
                        // A cancellation is observed at the durable timer's
                        // wake; Closing records pending cancellations itself.
                        let event = if aborted || handlers.run_cancel_requested().await? {
                            RunEvent::Decided {
                                call_id,
                                decision: CallDecision::Cancelled,
                                after: None,
                            }
                        } else {
                            RunEvent::RetryScheduled {
                                call_id,
                                failed: ordinal,
                                next: ordinal.next().ok_or("attempt ordinal exhausted")?,
                                backoff_ms: delay,
                            }
                        };
                        record.events.push(event);
                        Ok(RunJournalEntry {
                            record,
                            materials: Vec::new(),
                            state: Vec::new(),
                        })
                    }
                }
            });
            if let Some((_, chosen_key)) = chosen {
                self.journal.selection.forget(chosen_key);
            }
            let selected = self.journal.wait_record(name, step).await?;
            let event = selected
                .record
                .events
                .first()
                .ok_or(RunEventRefusal::EmptyRecord)?;
            let recorded = match event {
                RunEvent::SourceSealed { .. } => None,
                event => Some(
                    choices
                        .iter()
                        .position(|(_, _, work)| work.recorded_by(event))
                        .ok_or_else(|| {
                            selection_boundary("recorded selection has no issued source")
                        })?,
                ),
            };
            if let Some((chosen, chosen_key)) = chosen
                && recorded != Some(chosen)
            {
                // A non-Run await can leave a fresh unrecorded choice. The
                // served D is authoritative; retain that other popped source
                // for its next window, ahead of pops made while waiting on D.
                self.journal
                    .selection
                    .pending
                    .push(choices[chosen].0.clone());
                self.journal.selection.acknowledged.push_front(chosen_key);
            }
            // D recorded a source's seal: accept it in place, exactly as a
            // terminal body. A seal the live race took that D did not
            // record stays sealed at its authority for a later window.
            let Some(recorded) = recorded else {
                let Some(RunEvent::SourceSealed { call_id, seal }) =
                    selected.record.events.first().cloned()
                else {
                    return Err(RunEventRefusal::EmptyRecord.into());
                };
                self.journal.accept(selected)?;
                Box::pin(self.accept_source(&call_id, seal)).await?;
                return Ok(None);
            };
            self.journal.selection.forget(keys[recorded]);
            if let Some(pending) = presentation.as_mut()
                && selected.record.events.iter().any(|event| matches!(event, RunEvent::StartLaunched { call_id, .. } if *call_id == pending.call_id))
            {
                let handle = pending.handle.take().ok_or_else(|| boundary(&pending.call_id))?;
                let Ready::StartPrepared(prepared) = handle.await? else {
                    return Err(boundary(&pending.call_id));
                };
                if prepared.events != selected.record.events {
                    return Err(boundary(&pending.call_id));
                }
                self.journal.accept(selected)?;
                pending.select_key = None;
                pending.prepared = prepared.as_ref().clone();
                return Ok(None);
            }
            let event = selected
                .record
                .events
                .first()
                .ok_or(RunEventRefusal::EmptyRecord)?;
            if let RunEvent::TimerElapsed { aggregate, leaf } = event {
                let position = self
                    .timers
                    .iter()
                    .position(|timer| timer.key == *aggregate && timer.leaf == *leaf)
                    .ok_or_else(|| RunEventRefusal::TimerOrder {
                        key: aggregate.clone(),
                        leaf: *leaf,
                    })?;
                self.timers.remove(position).handle.await?;
                self.journal.accept(selected)?;
                return Ok(None);
            }
            if let RunEvent::Realized { call_id, receipt } = event {
                let call_id = call_id.clone();
                let receipt = receipt.clone();
                // A served record must not poll the attach: the issued
                // handle and its engine notification identity are dropped,
                // never awaited.
                let entry = self.realizing.remove(&call_id).ok_or_else(|| {
                    RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::EffectReplayDivergence,
                        format!("the schedule selected no issued realization for {call_id}"),
                    )
                })?;
                let Realizing { key, handle } = entry;
                drop(handle);
                drop(key);
                self.journal.accept(selected)?;
                let receipt: RealizationReceipt = self.journal.materials.decode(&receipt)?;
                let handlers = self
                    .handlers
                    .get(&call_id)
                    .cloned()
                    .ok_or_else(|| boundary(&call_id))?;
                handlers.get().adopt_realization(&call_id, &receipt)?;
                return Ok(None);
            }
            let position = self
                .pending
                .iter()
                .position(|entry| match event {
                    RunEvent::AttemptRecorded {
                        call_id, attempt, ..
                    } => {
                        !entry.timer
                            && entry.work.call.call_id == *call_id
                            && entry.ordinal == *attempt
                    }
                    RunEvent::RetryScheduled {
                        call_id, failed, ..
                    } => {
                        entry.timer
                            && entry.work.call.call_id == *call_id
                            && entry.ordinal == *failed
                    }
                    RunEvent::Decided {
                        call_id,
                        decision: CallDecision::Cancelled,
                        ..
                    } => entry.timer && entry.work.call.call_id == *call_id,
                    _ => false,
                })
                .ok_or_else(|| match event {
                    RunEvent::AttemptRecorded { call_id, .. }
                    | RunEvent::RetryScheduled { call_id, .. }
                    | RunEvent::Decided { call_id, .. }
                    | RunEvent::Presented { call_id, .. } => boundary(call_id),
                    _ => RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::EffectReplayDivergence,
                        "the schedule selected no issued attempt",
                    )
                    .into(),
                })?;
            let pending_entry = self.pending.remove(position);
            let ordinal = pending_entry.ordinal;
            let work = pending_entry.work;
            let (call, member, request) = (&work.call, &work.member, &work.request);
            let handlers = std::sync::Arc::clone(&work.handlers);
            let capture = match event {
                // A served timer selection drops its handle: replay never
                // awaits an SDK sleep a stop cut, nor before later commands.
                RunEvent::RetryScheduled { .. } | RunEvent::Decided { .. }
                    if pending_entry.timer =>
                {
                    pending_entry.capture
                }
                RunEvent::AttemptRecorded { result, .. } if !pending_entry.timer => {
                    let Ready::Attempt(entry) = pending_entry.handle.await? else {
                        return Err(boundary(&call.call_id));
                    };
                    if entry.call_id != call.call_id
                        || entry.attempt != ordinal
                        || entry.result != *result
                    {
                        return Err(boundary(&call.call_id));
                    }
                    let capture = captured(
                        &entry,
                        &self.journal.materials.owner,
                        &self.journal.materials.available,
                    )?;
                    let recorded_isolation = match &capture {
                        Some(SingletonCapture::Isolated { binding }) => Some(binding.as_ref()),
                        _ => None,
                    };
                    if recorded_isolation != request.isolation.as_ref() {
                        return Err(SingletonRunError::Drift {
                            call_id: call.call_id.clone(),
                            drift: SingletonDrift::IsolationBinding,
                        });
                    }
                    self.journal.materials.admit(entry.materials.clone())?;
                    self.attempts.push(entry.as_ref().clone());
                    capture
                }
                _ => return Err(boundary(&call.call_id)),
            };
            let state = selected.state.clone();
            let selected = self.journal.accept(selected)?;
            if let Some(plugins) = handlers.plugin_session()
                && let Some(index) = publications
                    .iter()
                    .position(|(held, _)| std::sync::Arc::ptr_eq(held, &plugins))
            {
                publications.swap_remove(index).1.publish_run(state)?;
            }
            match selected.events.as_slice() {
                [
                    RunEvent::AttemptRecorded {
                        result:
                            AttemptResult::Pending {
                                source,
                                metadata,
                                start,
                            },
                        ..
                    },
                ] => {
                    let terminal = self
                        .accept_pending(
                            call,
                            member.clone(),
                            Handlers(std::sync::Arc::clone(&handlers)),
                            ordinal,
                            PendingAttempt {
                                source: source.clone(),
                                metadata,
                                start: start.as_deref(),
                            },
                        )
                        .await?;
                    decision = Some((call.call_id.clone(), terminal));
                }
                [
                    RunEvent::AttemptRecorded {
                        result:
                            AttemptResult::DeferredStart {
                                source,
                                start_key,
                                obligation,
                            },
                        ..
                    },
                ] => {
                    let terminal = self.queue_deferred_start(
                        call,
                        member.clone(),
                        Handlers(std::sync::Arc::clone(&handlers)),
                        ordinal,
                        source.clone(),
                        SingletonStart {
                            start_key: start_key.clone(),
                            obligation: obligation.clone(),
                        },
                    );
                    decision = Some((call.call_id.clone(), terminal));
                }
                [
                    RunEvent::AttemptRecorded {
                        result: AttemptResult::Deferred { source },
                        ..
                    },
                ] => {
                    self.waiting.insert(
                        call.call_id.clone(),
                        Waiting {
                            call: call.clone(),
                            member: member.clone(),
                            handlers: Handlers(std::sync::Arc::clone(&handlers)),
                            attempt: ordinal,
                            start: None,
                        },
                    );
                    decision = Some((
                        call.call_id.clone(),
                        DecidedCall::Deferred {
                            source: source.clone(),
                        },
                    ));
                }
                [
                    RunEvent::AttemptRecorded { .. },
                    RunEvent::RetryTimerRegistered { backoff_ms, .. },
                ] => {
                    let delay = *backoff_ms;
                    let controller = self.journal.scoped.controller();
                    self.journal.scoped.admit_journal_write()?;
                    let timer = controller.start_run_retry(delay);
                    let select_key = timer.key.shared();
                    self.journal.selection.pending.push(select_key.clone());
                    let handle = async move {
                        timer.value.await?;
                        Ok(Ready::Timer)
                    }
                    .boxed()
                    .shared();
                    self.pending.push(Pending {
                        work: std::sync::Arc::clone(&work),
                        capture,
                        ordinal,
                        timer: true,
                        handle,
                        select_key,
                    });
                }
                [RunEvent::RetryScheduled { next, .. }] => {
                    let (select_key, handle) = self.issue_attempt(
                        call,
                        member,
                        request,
                        std::sync::Arc::clone(&handlers),
                        *next,
                    )?;
                    self.pending.push(Pending {
                        work: std::sync::Arc::clone(&work),
                        capture: None,
                        ordinal: *next,
                        timer: false,
                        handle,
                        select_key,
                    });
                }
                events => {
                    let (rank, recorded_decision) =
                        recorded_decision(&self.journal.ledger, events, &call.call_id)?;
                    self.owed.insert(
                        rank,
                        Owed {
                            call_id: call.call_id.clone(),
                            handlers: Handlers(std::sync::Arc::clone(&handlers)),
                            decision: recorded_decision.clone(),
                            capture,
                        },
                    );
                    decision = Some((
                        call.call_id.clone(),
                        DecidedCall::Ranked {
                            rank,
                            decision: recorded_decision.clone(),
                        },
                    ));
                }
            }
        }
        Ok(decision)
    }

    /// Closing owns the decision for a call whose failed X is already
    /// durable and whose next attempt is waiting on a backoff.
    pub(super) fn backoff_cancellations(&self) -> Vec<RunEvent> {
        self.pending
            .iter()
            .filter(|pending| pending.timer)
            .map(|pending| RunEvent::Decided {
                call_id: pending.work.call.call_id.clone(),
                decision: CallDecision::Cancelled,
                after: None,
            })
            .collect()
    }

    pub(super) fn accept_backoff_cancellations(
        &mut self,
        events: &[RunEvent],
    ) -> Result<(), SingletonRunError> {
        for event in events {
            if let RunEvent::Decided {
                call_id,
                decision: CallDecision::Cancelled,
                ..
            } = event
                && let Some(position) = self
                    .pending
                    .iter()
                    .position(|pending| pending.timer && pending.work.call.call_id == *call_id)
            {
                let pending = self.pending.remove(position);
                self.journal.selection.disown(&pending.select_key);
                let rank = self
                    .journal
                    .ledger
                    .decision_rank(call_id)
                    .ok_or_else(|| boundary(call_id))?;
                self.owed.insert(
                    rank,
                    Owed {
                        call_id: call_id.clone(),
                        handlers: Handlers(std::sync::Arc::clone(&pending.work.handlers)),
                        decision: CallDecision::Cancelled,
                        capture: pending.capture,
                    },
                );
            }
        }
        Ok(())
    }

    fn issue_attempt(
        &mut self,
        call: &SingletonToolCall,
        member: &AdmittedCall,
        request: &SingletonPreparedRequest,
        handlers: std::sync::Arc<dyn SingletonToolHandlers + 'a>,
        ordinal: AttemptOrdinal,
    ) -> Result<(KeyHandle<'a>, Handle<'a>), SingletonRunError> {
        self.journal.scoped.admit_journal_write()?;
        let owner = self.journal.materials.owner.clone();
        let name = record_name(&call.call_id, &format!("attempt:{ordinal}"));
        let completion_key = self
            .sources
            .get(&call.call_id)
            .map(|source| source.source.clone());
        let process_source = self.process_sources.get(&call.call_id).cloned();
        let (call, member, request) = (call.clone(), member.clone(), request.clone());
        let step = Box::pin(async move {
            capture_attempt(
                owner,
                &call,
                &member,
                &request,
                handlers.as_ref(),
                ordinal,
                AttemptSources {
                    completion: completion_key.as_ref(),
                    process: process_source.as_ref(),
                },
            )
            .await
        });
        let controller = self.journal.scoped.controller();
        let crate::tool_dispatch::RunAttemptHandle { body, result } =
            controller.start_run_attempt(name, step);
        self.bodies.issue(body);
        let select_key = result.key.shared();
        self.journal.selection.pending.push(select_key.clone());
        let handle = async move {
            let entry = result.value.await?;
            Ok(Ready::Attempt(std::sync::Arc::new(entry)))
        }
        .boxed()
        .shared();
        Ok((select_key, handle))
    }
}

fn describe(ready: &Ready) -> &'static str {
    match ready {
        Ready::Attempt(_) => "an X receipt",
        Ready::Timer => "a timer wake",
        Ready::Realization(_) => "a realization receipt",
        Ready::StartPrepared(_) => "a preparation",
        Ready::Sealed(_) => "a source seal",
    }
}

fn eligible(policy: &RecordedRetryPolicy, failed: AttemptOrdinal) -> bool {
    matches!(policy, RecordedRetryPolicy::Reported { max_attempts, .. } if failed.get() < max_attempts.get())
}

fn backoff(
    policy: &RecordedRetryPolicy,
    failed: AttemptOrdinal,
    capture: Option<&SingletonCapture>,
) -> u64 {
    let RecordedRetryPolicy::Reported {
        base_delay_ms,
        max_delay_ms,
        ..
    } = policy
    else {
        return 0;
    };
    let hint = match capture {
        Some(SingletonCapture::RetryableFailure { after_ms, .. }) => *after_ms,
        _ => None,
    };
    hint.unwrap_or_else(|| {
        base_delay_ms.saturating_mul(
            1_u64
                .checked_shl(failed.get().saturating_sub(1))
                .unwrap_or(u64::MAX),
        )
    })
    .min(*max_delay_ms)
}

fn selection_boundary(message: &str) -> SingletonRunError {
    RuntimeEffectControllerError::new(crate::RuntimeErrorCode::EffectReplayDivergence, message)
        .into()
}

/// Only the owner pops VM notifications. The queue is rebuilt in the same
/// order on replay, including pops during non-selection record waits.
#[derive(Default)]
pub(super) struct Selection<'a> {
    pub pending: Vec<KeyHandle<'a>>,
    acknowledged: std::collections::VecDeque<SelectKey>,
}

impl<'a> Selection<'a> {
    /// Drop a source the Run disowned without selecting it, so later record
    /// waits no longer arm on it and its acknowledgement leaves the queue.
    pub(super) fn disown(&mut self, source: &KeyHandle<'a>) {
        if let Some(Ok(key)) = source.peek() {
            self.acknowledged.retain(|found| found != key);
        }
        self.pending.retain(|held| !held.ptr_eq(source));
    }

    fn forget(&mut self, key: SelectKey) {
        self.pending
            .retain(|source| !matches!(source.peek(), Some(Ok(found)) if *found == key));
        self.acknowledged.retain(|found| *found != key);
    }

    fn retain(&mut self, keys: &[SelectKey]) {
        self.pending
            .retain(|source| matches!(source.peek(), Some(Ok(key)) if keys.contains(key)));
        self.acknowledged.retain(|key| keys.contains(key));
    }
}

impl RunJournal<'_> {
    /// Await a short record with every outstanding source in one combinator.
    /// A source that becomes ready stays queued until a schedule consumes it.
    pub(super) async fn wait_record(
        &mut self,
        name: String,
        step: crate::RunRecordStep<'_>,
    ) -> Result<RunJournalEntry, SingletonRunError> {
        self.scoped.admit_journal_write()?;
        let crate::tool_dispatch::RunStepHandle { body, result } = self
            .scoped
            .controller()
            .start_run_record(name.clone(), step);
        // This body belongs only to this record wait. A served record never
        // starts it, so returning the value can drop its unstarted body.
        let bodies = RunBodies::new();
        bodies.issue(body);
        let scoped = self.scoped;
        Ok(Box::pin(scoped.await_owner_step(
            name,
            bodies.beside(async {
                let decision_key = result.key.await?;
                let mut keys = Vec::with_capacity(self.selection.pending.len());
                for source in &self.selection.pending {
                    keys.push(source.clone().await?);
                }
                loop {
                    let remaining: Vec<_> = keys
                        .iter()
                        .copied()
                        .filter(|key| !self.selection.acknowledged.contains(key))
                        .collect();
                    let mut awaited = vec![decision_key];
                    awaited.extend(remaining.iter().copied());
                    let chosen = self.scoped.controller().select_run_sources(awaited).await?;
                    if chosen == 0 {
                        break;
                    }
                    let key = remaining.get(chosen - 1).ok_or_else(|| {
                        RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::EffectReplayDivergence,
                            "record wait selected no source",
                        )
                    })?;
                    self.selection.acknowledged.push_back(*key);
                }
                result.value.await
            }),
        ))
        .await?)
    }
}
