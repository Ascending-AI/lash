//! Eager X handles and the recorded K9 selection/registration schedule.
use super::*;
use crate::tool_run::{RecordedRetryPolicy, RunAttemptEntry};
use futures_util::future::{BoxFuture, FutureExt, Shared, select_all};

type Handle<'a> = Shared<BoxFuture<'a, Result<Ready, RuntimeEffectControllerError>>>;

#[derive(Clone)]
enum Ready {
    Attempt(std::sync::Arc<RunAttemptEntry>),
    Timer,
}

struct Pending<'a> {
    index: usize,
    ordinal: AttemptOrdinal,
    timer: bool,
    handle: Handle<'a>,
}

fn captured(
    entry: &RunAttemptEntry,
    owner: &MaterialOwner,
    available: &[PluginRevision],
) -> Result<Option<SingletonCapture>, RuntimeEffectControllerError> {
    let output = match &entry.result {
        AttemptResult::Deferred { .. } => return Ok(None),
        AttemptResult::Done { output } | AttemptResult::Failed { output, .. } => output,
    };
    let mut materials = Materials {
        owner: owner.clone(),
        available: available.to_vec(),
        entries: BTreeMap::new(),
    };
    materials.admit(entry.materials.clone())?;
    materials.decode(output).map(Some)
}

impl<'a> RunCoordinator<'a> {
    /// Admit the whole round, register first attempts in admission order,
    /// then record each selection together with its final decision or retry
    /// eligibility. Replay registers that dynamic prefix before awaiting an
    /// older unresolved handle. Every issued X settles before success returns;
    /// an invocation fault leaves its unrecorded work to recovery.
    ///
    /// # Errors
    /// A typed admission, journal, material or Run-fold refusal.
    pub async fn decide_round(
        &mut self,
        calls: &'a [SingletonToolCall],
        handlers: std::sync::Arc<dyn SingletonToolHandlers>,
        retry: RecordedRetryPolicy,
    ) -> Result<Vec<DecidedCall>, SingletonRunError> {
        let admitted = self.admit_round(calls, handlers.as_ref(), retry).await?;
        let mut decisions = vec![None; calls.len()];
        let mut captures = vec![None; calls.len()];
        let mut pending = Vec::new();
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
                pending.push(Pending {
                    index,
                    ordinal: AttemptOrdinal::FIRST,
                    timer: false,
                    handle,
                });
            }
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
            decisions[index] = Some(
                self.decide_candidate(
                    &calls[index],
                    Handlers::Owned(std::sync::Arc::clone(&handlers)),
                    member,
                    candidate,
                )
                .await?,
            );
        }
        while !pending.is_empty() {
            let record = self.journal.record(Vec::new());
            let rank = self.journal.ledger.next_rank();
            let aborted = self.journal.ledger.aborted();
            let choices: Vec<_> = pending
                .iter()
                .map(|entry| {
                    (
                        entry.index,
                        entry.ordinal,
                        entry.timer,
                        entry.handle.clone(),
                        backoff(
                            &admitted[entry.index].0.policy.retry,
                            entry.ordinal,
                            captures[entry.index].as_ref(),
                        ),
                    )
                })
                .collect();
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
            let publication = handlers
                .plugin_session()
                .map(|plugins| crate::plugin::EffectPublication::begin(plugins, address.clone()));
            let owner = self.journal.materials.owner.clone();
            let available = self.journal.materials.available.clone();
            let members = admitted.clone();
            let step_calls = calls.to_vec();
            let needs_selection = std::sync::Arc::new(tokio::sync::Notify::new());
            let needed = std::sync::Arc::clone(&needs_selection);
            let (send_choice, receive_choice) = tokio::sync::oneshot::channel();
            let selector = async move {
                needs_selection.notified().await;
                let (ready, chosen, _) =
                    select_all(choices.iter().map(|entry| entry.3.clone())).await;
                let (index, ordinal, timer, _, delay) = &choices[chosen];
                let _ = send_choice.send((ready, *index, *ordinal, *timer, *delay));
            };
            let step_handlers = std::sync::Arc::clone(&handlers);
            let step = Box::pin(async move {
                let handlers = step_handlers.as_ref();
                needed.notify_one();
                let (ready, index, ordinal, timer, delay) = receive_choice
                    .await
                    .map_err(|_| "the owning selection frame ended".to_owned())?;
                let call = &step_calls[index];
                let (member, _) = &members[index];
                let call_id = call.call_id.clone();
                let mut record = record;
                match ready.map_err(|error| error.to_string())? {
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
            let position = pending
                .iter()
                .position(|entry| match event {
                    RunEvent::AttemptRecorded {
                        call_id, attempt, ..
                    } => {
                        !entry.timer
                            && calls[entry.index].call_id == *call_id
                            && entry.ordinal == *attempt
                    }
                    RunEvent::RetryScheduled {
                        call_id, failed, ..
                    } => {
                        entry.timer
                            && calls[entry.index].call_id == *call_id
                            && entry.ordinal == *failed
                    }
                    RunEvent::Decided {
                        call_id,
                        decision: CallDecision::Cancelled,
                        ..
                    } => entry.timer && calls[entry.index].call_id == *call_id,
                    _ => false,
                })
                .ok_or_else(|| boundary(&calls[0].call_id))?;
            let pending_entry = pending.remove(position);
            let index = pending_entry.index;
            let ordinal = pending_entry.ordinal;
            let (member, request) = &admitted[index];
            let call = &calls[index];
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
                    capture
                }
                (RunEvent::RetryScheduled { .. } | RunEvent::Decided { .. }, Ready::Timer) => {
                    captures[index].take()
                }
                _ => return Err(boundary(&call.call_id)),
            };
            let state = selected.state.clone();
            let selected = self.journal.accept(selected)?;
            if let Some(publication) = publication {
                publication.publish_run(state)?;
            }
            match selected.events.as_slice() {
                [
                    RunEvent::AttemptRecorded {
                        result: AttemptResult::Deferred { source },
                        ..
                    },
                ] => {
                    decisions[index] = Some(DecidedCall::Deferred {
                        source: source.clone(),
                    });
                }
                [
                    RunEvent::AttemptRecorded { .. },
                    RunEvent::RetryTimerRegistered { backoff_ms, .. },
                ] => {
                    captures[index] = capture;
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
                    pending.push(Pending {
                        index,
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
                    pending.push(Pending {
                        index,
                        ordinal: *next,
                        timer: false,
                        handle,
                    });
                }
                [
                    RunEvent::AttemptRecorded { .. },
                    RunEvent::Decided { rank, decision, .. },
                ]
                | [RunEvent::Decided { rank, decision, .. }] => {
                    self.owed.insert(
                        *rank,
                        Owed {
                            call_id: call.call_id.clone(),
                            handlers: Handlers::Owned(std::sync::Arc::clone(&handlers)),
                            decision: decision.clone(),
                            capture,
                        },
                    );
                    decisions[index] = Some(DecidedCall::Ranked {
                        rank: *rank,
                        decision: decision.clone(),
                    });
                }
                _ => return Err(boundary(&call.call_id)),
            }
        }
        decisions
            .into_iter()
            .enumerate()
            .map(|(index, decision)| decision.ok_or_else(|| boundary(&calls[index].call_id)))
            .collect()
    }

    fn issue_attempt<'run>(
        &self,
        call: &SingletonToolCall,
        member: &AdmittedCall,
        request: &SingletonPreparedRequest,
        handlers: std::sync::Arc<dyn SingletonToolHandlers>,
        ordinal: AttemptOrdinal,
    ) -> Result<Handle<'run>, SingletonRunError>
    where
        'a: 'run,
    {
        self.journal.scoped.admit_journal_write()?;
        let owner = self.journal.materials.owner.clone();
        let name = record_name(&call.call_id, &format!("attempt:{ordinal}"));
        let (call, member, request) = (call.clone(), member.clone(), request.clone());
        let step = Box::pin(async move {
            capture_attempt(owner, &call, &member, &request, handlers.as_ref(), ordinal).await
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
