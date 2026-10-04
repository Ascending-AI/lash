//! Eager X handles and the recorded K9 selection/registration schedule.
use super::*;
use crate::tool_run::{RecordedRetryPolicy, RunAttemptEntry};
use futures_util::future::{BoxFuture, FutureExt, Shared, select_all};

pub(super) type Handle<'a> = Shared<BoxFuture<'a, Result<Ready, RuntimeEffectControllerError>>>;

#[derive(Clone)]
pub(super) enum Ready {
    Attempt(std::sync::Arc<RunAttemptEntry>),
    Timer,
}

pub(super) struct AggregateTimer<'a> {
    pub key: String,
    pub leaf: u32,
    pub handle: Handle<'a>,
}

#[derive(Clone)]
enum SelectedWork<'a> {
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
}

impl<'a> Pending<'a> {
    pub(super) fn handle(&self) -> Handle<'a> {
        self.handle.clone()
    }
}

/// Poll the already issued X handles alongside a program effect or protected
/// drain. Readiness never decides a call: only its recorded schedule does.
pub(super) async fn poll_beside<F: std::future::Future>(
    handles: &[Handle<'_>],
    future: F,
) -> Result<F::Output, RuntimeEffectControllerError> {
    tokio::pin!(future);
    let mut handles: Vec<_> = handles.iter().cloned().map(Some).collect();
    std::future::poll_fn(|context| {
        // Issue the program command first on every replay. Once that future
        // returns, its context may have failed; do not poll another SDK wait.
        if let std::task::Poll::Ready(output) = future.as_mut().poll(context) {
            return std::task::Poll::Ready(Ok(output));
        }
        for handle in &mut handles {
            let Some(active) = handle else {
                continue;
            };
            match std::pin::Pin::new(active).poll(context) {
                std::task::Poll::Ready(Err(error)) => {
                    return std::task::Poll::Ready(Err(error));
                }
                // This polling clone is done. The coordinator still owns
                // its unpolled clone and accepts the receipt in the schedule.
                std::task::Poll::Ready(Ok(_)) => *handle = None,
                std::task::Poll::Pending => {}
            }
        }
        std::task::Poll::Pending
    })
    .await
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
        handlers: std::sync::Arc<dyn SingletonToolHandlers + 'a>,
        retry: RecordedRetryPolicy,
    ) -> Result<Vec<(ToolCallId, DecidedCall)>, SingletonRunError> {
        self.begin_frame()?;
        let result = self.start_round_inner(calls, handlers, retry, None).await;
        self.active_frame = false;
        self.note_fault(&result);
        result
    }

    pub(super) async fn start_round_inner(
        &mut self,
        calls: &[SingletonToolCall],
        handlers: std::sync::Arc<dyn SingletonToolHandlers + 'a>,
        retry: RecordedRetryPolicy,
        aggregate: Option<(&crate::tool_run::AggregatePlan, &dyn crate::Clock)>,
    ) -> Result<Vec<(ToolCallId, DecidedCall)>, SingletonRunError> {
        let admitted = self
            .admit_round(calls, handlers.as_ref(), retry, aggregate)
            .await?;
        let mut decisions = Vec::new();
        for call in calls {
            self.handlers.insert(
                call.call_id.clone(),
                Handlers::Owned(std::sync::Arc::clone(&handlers)),
            );
        }
        // Even an immediate/cached winner cannot bypass registration of a
        // sibling whose admission already owns an executable attempt.
        for (index, (member, request)) in admitted.iter().enumerate() {
            if member.selection() == BeforeSelection::Execute {
                let handle = self.issue_attempt(
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
                });
            }
        }
        if let Some((plan, clock)) = aggregate {
            self.register_aggregate_timers(plan, clock)?;
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
                    Handlers::Owned(std::sync::Arc::clone(&handlers)),
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
            if let crate::tool_run::AggregateLeaf::Timer { duration_ms } = leaf {
                let deadline = admitted_at_ms.saturating_add(*duration_ms);
                self.journal.scoped.admit_journal_write()?;
                let timer = self
                    .journal
                    .scoped
                    .controller()
                    .start_run_retry(deadline.saturating_sub(clock.timestamp_ms()));
                let handle = async move {
                    timer.await?;
                    Ok(Ready::Timer)
                }
                .boxed()
                .shared();
                self.timers.push(AggregateTimer {
                    key: plan.key.clone(),
                    leaf: index as u32,
                    handle,
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
        let result = self.progress_inner().await;
        self.active_frame = false;
        self.note_fault(&result);
        result
    }

    pub(super) async fn progress_inner(
        &mut self,
    ) -> Result<Option<(ToolCallId, DecidedCall)>, SingletonRunError> {
        if self.pending.is_empty() && self.timers.is_empty() {
            return Ok(None);
        }
        let mut decision = None;
        {
            let record = self.journal.record(Vec::new());
            let rank = self.journal.ledger.next_rank();
            let aborted = self.journal.ledger.aborted()
                || self.journal.ledger.lifecycle() != crate::tool_run::RunLifecycle::Live;
            let mut choices: Vec<_> = self
                .pending
                .iter()
                .map(|entry| {
                    (
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
                    timer.handle.clone(),
                    SelectedWork::AggregateTimer {
                        key: timer.key.clone(),
                        leaf: timer.leaf,
                    },
                )
            }));
            let name = format!("lash:run:schedule:{}", record.first.0);
            let address = crate::EffectAddress::new(
                self.journal.scoped.execution_scope().clone(),
                name.clone(),
            )
            .map_err(|error| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
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

            let needs_selection = std::sync::Arc::new(tokio::sync::Notify::new());
            let needed = std::sync::Arc::clone(&needs_selection);
            let (send_choice, receive_choice) = tokio::sync::oneshot::channel();
            let selector = async move {
                needs_selection.notified().await;
                let (ready, chosen, _) =
                    select_all(choices.iter().map(|entry| entry.0.clone())).await;
                let _ = send_choice.send((ready, choices[chosen].1.clone()));
            };
            let step = Box::pin(async move {
                needed.notify_one();
                let (ready, selected_work) = receive_choice
                    .await
                    .map_err(|_| "the owning selection frame ended".to_owned())?;
                let ready = ready.map_err(|error| error.to_string())?;
                let (work, ordinal, timer, delay) = match selected_work {
                    SelectedWork::AggregateTimer { key, leaf } => {
                        if !matches!(ready, Ready::Timer) {
                            return Err("aggregate timer returned an X receipt".to_owned());
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
                            && !handlers.run_cancel_requested()
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
                                    rank,
                                    address,
                                    aborted,
                                },
                            )
                            .await
                        }
                    }
                    Ready::Timer => {
                        if !timer {
                            return Err("an X handle returned a timer wake".to_owned());
                        }
                        let event = if aborted || handlers.run_cancel_requested() {
                            RunEvent::Decided {
                                call_id,
                                rank,
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
            self.journal.scoped.admit_journal_write()?;
            let selection = self
                .journal
                .scoped
                .controller()
                .record_run_schedule(name, step);
            tokio::pin!(selection);
            tokio::pin!(selector);
            let selected = tokio::select! {
                result = &mut selection => result?,
                () = &mut selector => selection.await?,
            };
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
                .ok_or_else(|| boundary(&self.pending[0].work.call.call_id))?;
            let pending_entry = self.pending.remove(position);
            let ordinal = pending_entry.ordinal;
            let work = pending_entry.work;
            let (call, member, request) = (&work.call, &work.member, &work.request);
            let handlers = std::sync::Arc::clone(&work.handlers);
            let ready = pending_entry.handle.await?;
            let capture = match (event, &ready) {
                (RunEvent::AttemptRecorded { result, .. }, Ready::Attempt(entry)) => {
                    if entry.call_id != call.call_id
                        || entry.attempt != ordinal
                        || entry.result != *result
                    {
                        return Err(boundary(&call.call_id));
                    }
                    let capture = captured(
                        entry,
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
                (RunEvent::RetryScheduled { .. } | RunEvent::Decided { .. }, Ready::Timer) => {
                    pending_entry.capture
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
                            Handlers::Owned(std::sync::Arc::clone(&handlers)),
                            ordinal,
                            source.clone(),
                            metadata,
                            start.as_ref(),
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
                        Handlers::Owned(std::sync::Arc::clone(&handlers)),
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
                            handlers: Handlers::Owned(std::sync::Arc::clone(&handlers)),
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
                    let handle = async move {
                        timer.await?;
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
                    });
                }
                [RunEvent::RetryScheduled { next, .. }] => {
                    let handle = self.issue_attempt(
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
                    });
                }
                [
                    RunEvent::AttemptRecorded { .. },
                    RunEvent::Decided {
                        rank,
                        decision: recorded_decision,
                        ..
                    },
                ]
                | [
                    RunEvent::Decided {
                        rank,
                        decision: recorded_decision,
                        ..
                    },
                ] => {
                    self.owed.insert(
                        *rank,
                        Owed {
                            call_id: call.call_id.clone(),
                            handlers: Handlers::Owned(std::sync::Arc::clone(&handlers)),
                            decision: recorded_decision.clone(),
                            capture,
                        },
                    );
                    decision = Some((
                        call.call_id.clone(),
                        DecidedCall::Ranked {
                            rank: *rank,
                            decision: recorded_decision.clone(),
                        },
                    ));
                }
                _ => return Err(boundary(&call.call_id)),
            }
        }
        Ok(decision)
    }

    /// Admit and decide every call of this round in the recorded K9 schedule.
    /// Existing pending calls also progress; their decisions remain in the Run.
    ///
    /// # Errors
    /// A typed admission, journal, material or Run-fold refusal.
    pub async fn decide_round(
        &mut self,
        calls: &[SingletonToolCall],
        handlers: std::sync::Arc<dyn SingletonToolHandlers + 'a>,
        retry: RecordedRetryPolicy,
    ) -> Result<Vec<DecidedCall>, SingletonRunError> {
        let mut decisions: BTreeMap<_, _> = self
            .start_round(calls, handlers, retry)
            .await?
            .into_iter()
            .collect();
        while !self.pending.is_empty() {
            if let Some((call_id, decision)) = self.progress().await? {
                decisions.insert(call_id, decision);
            }
        }
        calls
            .iter()
            .map(|call| {
                decisions
                    .remove(&call.call_id)
                    .ok_or_else(|| boundary(&call.call_id))
            })
            .collect()
    }

    fn issue_attempt<'run>(
        &self,
        call: &SingletonToolCall,
        member: &AdmittedCall,
        request: &SingletonPreparedRequest,
        handlers: std::sync::Arc<dyn SingletonToolHandlers + 'a>,
        ordinal: AttemptOrdinal,
    ) -> Result<Handle<'run>, SingletonRunError>
    where
        'a: 'run,
    {
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
        let attempt = controller.start_run_attempt(name, step);
        let handle = async move {
            let entry = attempt.await?;
            Ok(Ready::Attempt(std::sync::Arc::new(entry)))
        }
        .boxed()
        .shared();
        Ok(handle)
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
