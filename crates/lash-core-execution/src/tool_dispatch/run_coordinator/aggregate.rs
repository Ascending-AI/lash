//! Aggregate selection and logical ownership over K3's recorded schedule.

use std::collections::BTreeSet;

use super::*;
use crate::tool_run::{AggregateConsumer, AggregateLeaf, AggregatePlan, RunLifecycle};

/// An aggregate exposes only the operands its consumer took. Already
/// settled values and timer fulfilments stay with the program as `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunAggregateOutcome {
    /// An empty race, or an aggregate waiting on a Deferred source.
    Pending,
    Selected {
        operand: u32,
        fulfilled: bool,
        reply: Option<SingletonTerminal>,
    },
    AllResults(Vec<Option<SingletonTerminal>>),
    ExhaustedRejections(Vec<Option<SingletonTerminal>>),
    /// Logical Run cancellation and abort are not operand rejections.
    HostControl {
        call_id: ToolCallId,
        decision: CallDecision,
    },
}

struct Settlement {
    fulfilled: bool,
    /// Immediate operands precede every newly dispatched settlement.
    order: (bool, u64),
    rank: Option<u64>,
}

enum Selection {
    Pending,
    Selected(usize),
    All,
    Exhausted,
    HostControl(ToolCallId, CallDecision),
}

impl<'a> RunCoordinator<'a> {
    /// Admit timers, immediate operands and handles of calls this Run already
    /// owns. This uses the same recorded terminal order as tool aggregates,
    /// without a tool callback registry or a group service.
    ///
    /// # Errors
    /// A typed mapping, owner, lifecycle or journal refusal. An unknown call
    /// refuses the entire plan before any timer starts.
    pub async fn admit_aggregate(
        &mut self,
        plan: &AggregatePlan,
        clock: &dyn crate::Clock,
    ) -> Result<(), SingletonRunError> {
        plan.validate()?;
        if let Some(cut) = self.cut {
            return Err(super::RunCutRefusal::AdmissionFrozen { reason: cut.reason }.into());
        }
        if plan.leaves.iter().any(|leaf| {
            matches!(leaf, AggregateLeaf::Call { call_id } if !self.journal.ledger.has_call(call_id))
        }) {
            return Err(RunEventRefusal::AggregateShape { key: plan.key.clone() }.into());
        }
        self.begin_frame()?;
        let record = self.journal.record(vec![RunEvent::AggregateAdmitted {
            plan: plan.clone(),
            admitted_at_ms: clock.timestamp_ms(),
        }]);
        let result = async {
            let mut ledger = self.journal.ledger.clone();
            ledger.append(self.journal.segment, &record)?;
            self.journal
                .append(
                    format!("lash:run:aggregate:{}:admit", plan.key),
                    Box::pin(async move {
                        Ok(RunJournalEntry {
                            record,
                            materials: Vec::new(),
                            state: Vec::new(),
                        })
                    }),
                )
                .await?;
            if self.aggregate_plan(&plan.key)? != *plan {
                return Err(RunEventRefusal::AggregateShape {
                    key: plan.key.clone(),
                }
                .into());
            }
            self.register_aggregate_timers(plan, clock)
        }
        .await;
        self.active_frame = false;
        self.note_fault(&result);
        result
    }

    /// Admit all pending siblings, aliases, immediate operands and timer
    /// deadlines before a consumer can leave with an immediate winner.
    /// Existing admitted calls may be referenced without another attempt.
    /// The logical owner supplies the clock used for recorded timer admission.
    ///
    /// # Errors
    /// A typed mapping, admission, material or journal refusal.
    pub async fn start_aggregate(
        &mut self,
        plan: &AggregatePlan,
        calls: &'a [SingletonToolCall],
        handlers: std::sync::Arc<dyn SingletonToolHandlers>,
        retry: crate::tool_run::RecordedRetryPolicy,
        clock: &dyn crate::Clock,
    ) -> Result<(), SingletonRunError> {
        plan.validate()?;
        let supplied: BTreeSet<_> = calls.iter().map(|call| &call.call_id).collect();
        let new: BTreeSet<_> = plan
            .leaves
            .iter()
            .filter_map(|leaf| match leaf {
                AggregateLeaf::Call { call_id } if !self.journal.ledger.has_call(call_id) => {
                    Some(call_id)
                }
                _ => None,
            })
            .collect();
        if supplied != new || supplied.len() != calls.len() {
            return Err(RunEventRefusal::AggregateShape {
                key: plan.key.clone(),
            }
            .into());
        }
        self.begin_frame()?;
        let result = self
            .start_round_inner(calls, handlers, retry, Some((plan, clock)))
            .await
            .map(|_| ());
        self.active_frame = false;
        self.note_fault(&result);
        result?;
        let recorded = self.aggregate_plan(&plan.key)?;
        if recorded != *plan {
            self.faulted = true;
            return Err(RunEventRefusal::AggregateShape {
                key: plan.key.clone(),
            }
            .into());
        }
        Ok(())
    }

    /// Select from the recorded schedule, returning as soon as this consumer
    /// has its answer. Issued losers remain owned and keep their policies.
    /// A Deferred descriptor carries no value and never completes a consumer.
    ///
    /// # Errors
    /// A typed execution, material or journal refusal.
    pub async fn consume_aggregate(
        &mut self,
        key: &str,
        consumer: AggregateConsumer,
    ) -> Result<RunAggregateOutcome, SingletonRunError> {
        self.begin_frame()?;
        let result = self.consume_aggregate_inner(key, consumer).await;
        self.active_frame = false;
        self.note_fault(&result);
        result
    }

    async fn consume_aggregate_inner(
        &mut self,
        key: &str,
        consumer: AggregateConsumer,
    ) -> Result<RunAggregateOutcome, SingletonRunError> {
        let plan = self.aggregate_plan(key)?;
        let (selection, settlements) = loop {
            let (selection, settlements) = self.select(&plan, consumer)?;
            if !matches!(selection, Selection::Pending)
                || plan.operands.is_empty()
                || (self.pending.is_empty() && self.timers.is_empty())
            {
                break (selection, settlements);
            }
            self.progress_inner().await?;
        };
        if let Selection::HostControl(call_id, decision) = selection {
            return Ok(RunAggregateOutcome::HostControl { call_id, decision });
        }
        if matches!(selection, Selection::Pending) {
            return Ok(RunAggregateOutcome::Pending);
        }
        let observed: BTreeSet<_> = match selection {
            Selection::Selected(operand) if consumer != AggregateConsumer::ListBatch => {
                [plan.operands[operand] as usize].into_iter().collect()
            }
            _ => (0..plan.leaves.len()).collect(),
        };
        let consumed: BTreeSet<_> = observed
            .iter()
            .filter_map(|index| match &plan.leaves[*index] {
                AggregateLeaf::Call { call_id } => Some(call_id.clone()),
                _ => None,
            })
            .collect();
        let through = observed
            .iter()
            .filter_map(|index| settlements[*index].as_ref()?.rank)
            .max();
        if let Some(through) = through {
            self.drain_through(through, &consumed).await?;
        }
        // A background drain may have presented a result before this consumer
        // asked for it. Taking its value is a separate recorded fact then.
        let events: Vec<_> = consumed
            .iter()
            .filter(|id| !self.journal.ledger.consumed(id))
            .map(|id| RunEvent::Consumed {
                call_id: id.clone(),
            })
            .collect();
        if !events.is_empty() {
            let record = self.journal.record(events);
            self.journal
                .append(
                    format!("lash:run:aggregate:{key}:consume:{}", record.first.0),
                    Box::pin(async move {
                        Ok(RunJournalEntry {
                            record,
                            materials: Vec::new(),
                            state: Vec::new(),
                        })
                    }),
                )
                .await?;
        }
        let reply = |operand: usize| match &plan.leaves[plan.operands[operand] as usize] {
            AggregateLeaf::Call { call_id } => self.terminal(call_id).map(Some),
            _ => Ok(None),
        };
        Ok(match selection {
            Selection::Selected(operand) => RunAggregateOutcome::Selected {
                operand: operand as u32,
                fulfilled: settlements[plan.operands[operand] as usize]
                    .as_ref()
                    .ok_or_else(|| RunEventRefusal::AggregateShape {
                        key: plan.key.clone(),
                    })?
                    .fulfilled,
                reply: reply(operand)?,
            },
            Selection::All => RunAggregateOutcome::AllResults(
                (0..plan.operands.len())
                    .map(reply)
                    .collect::<Result<_, _>>()?,
            ),
            Selection::Exhausted => RunAggregateOutcome::ExhaustedRejections(
                (0..plan.operands.len())
                    .map(reply)
                    .collect::<Result<_, _>>()?,
            ),
            Selection::Pending | Selection::HostControl(..) => {
                unreachable!("handled before consumption")
            }
        })
    }

    /// Poll issued attempts beside a program effect without consuming any
    /// result or selecting by future readiness. A dropped frame is a fault.
    ///
    /// # Errors
    /// A typed fault from an issued attempt.
    pub async fn beside<F: std::future::Future>(
        &mut self,
        effect: F,
    ) -> Result<F::Output, SingletonRunError> {
        self.begin_frame()?;
        let handles = self
            .pending
            .iter()
            .map(parallel::Pending::handle)
            .collect::<Vec<_>>();
        let result = parallel::poll_beside(&handles, effect)
            .await
            .map_err(Into::into);
        self.active_frame = false;
        self.note_fault(&result);
        result
    }

    /// Finish committed protected work without giving a consumer any value
    /// or possession. Bodies of outstanding losers progress alongside it.
    ///
    /// # Errors
    /// A typed protected-drain or journal refusal.
    pub async fn drain_protected(&mut self) -> Result<(), SingletonRunError> {
        self.begin_frame()?;
        let result = self.drain_through(u64::MAX, &BTreeSet::new()).await;
        self.active_frame = false;
        self.note_fault(&result);
        result
    }

    async fn drain_through(
        &mut self,
        through: u64,
        consumed: &BTreeSet<ToolCallId>,
    ) -> Result<(), SingletonRunError> {
        self.drain_starts().await?;
        while self
            .owed
            .first_key_value()
            .is_some_and(|(rank, _)| *rank <= through)
        {
            let Some((rank, owed)) = self.owed.pop_first() else {
                break;
            };
            let consume = consumed.contains(&owed.call_id);
            self.present(rank, owed, consume).await?;
        }
        Ok(())
    }

    #[must_use]
    pub fn lifecycle(&self) -> RunLifecycle {
        self.journal.ledger.lifecycle()
    }

    /// End the logical owner. Closing freezes admission, discharges only
    /// eligible cancellations, waits for issued X through durable ACK, and
    /// drains finals already accepted without consuming losing values.
    /// A physical cut and worker loss never call this method.
    ///
    /// # Errors
    /// A typed execution, cancellation or protected-drain refusal.
    pub async fn close(&mut self) -> Result<(), SingletonRunError> {
        self.begin_frame()?;
        let result = self.close_inner().await;
        self.active_frame = false;
        self.note_fault(&result);
        result
    }

    async fn close_inner(&mut self) -> Result<(), SingletonRunError> {
        self.lifecycle_record(RunLifecycle::Closing).await?;
        self.timers.clear();
        for call_id in self.journal.ledger.eligible_cancellations() {
            let handlers = self
                .handlers
                .get(&call_id)
                .cloned()
                .ok_or_else(|| boundary(&call_id))?;
            let source = self
                .sources
                .get(&call_id)
                .map(|source| source.source.clone());
            self.cancel(&call_id, handlers.get(), source.as_ref())
                .await?;
        }
        while !self.pending.is_empty() {
            self.progress_inner().await?;
        }
        self.drain_starts().await?;
        let waiting: Vec<_> = self.waiting.keys().cloned().collect();
        for call_id in waiting {
            let source = self
                .sources
                .get(&call_id)
                .ok_or_else(|| boundary(&call_id))?;
            let seal = self
                .journal
                .scoped
                .controller()
                .cancel_run_source(source.clone())
                .await?;
            self.accept_source(&call_id, seal).await?;
        }
        self.drain_through(u64::MAX, &BTreeSet::new()).await?;
        self.lifecycle_record(RunLifecycle::Settled).await
    }

    async fn cancel(
        &mut self,
        call_id: &ToolCallId,
        handlers: &dyn SingletonToolHandlers,
        source: Option<&AwaitEventKey>,
    ) -> Result<(), SingletonRunError> {
        let record = self.journal.record(vec![RunEvent::CancelDischarged {
            call_id: call_id.clone(),
        }]);
        let handles = self
            .pending
            .iter()
            .map(parallel::Pending::handle)
            .collect::<Vec<_>>();
        parallel::poll_beside(
            &handles,
            self.journal.append(
                record_name(call_id, "cancel"),
                Box::pin(async move {
                    handlers.cancel_call(call_id, source).await?;
                    Ok(RunJournalEntry {
                        record,
                        materials: Vec::new(),
                        state: Vec::new(),
                    })
                }),
            ),
        )
        .await??;
        Ok(())
    }

    async fn lifecycle_record(&mut self, state: RunLifecycle) -> Result<(), SingletonRunError> {
        let record = self.journal.record(vec![RunEvent::Lifecycle { state }]);
        self.journal
            .append(
                format!("lash:run:lifecycle:{state:?}"),
                Box::pin(async move {
                    Ok(RunJournalEntry {
                        record,
                        materials: Vec::new(),
                        state: Vec::new(),
                    })
                }),
            )
            .await?;
        Ok(())
    }

    fn aggregate_plan(&self, key: &str) -> Result<AggregatePlan, SingletonRunError> {
        self.journal
            .records
            .iter()
            .flat_map(|record| &record.events)
            .find_map(|event| match event {
                RunEvent::AggregateAdmitted { plan, .. } if plan.key == key => Some(plan.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                RunEventRefusal::UnknownAggregate {
                    key: key.to_owned(),
                }
                .into()
            })
    }

    fn call_capture(
        &self,
        call_id: &ToolCallId,
        source: &ResultSource,
    ) -> Result<SingletonCapture, SingletonRunError> {
        let reference = self
            .journal
            .records
            .iter()
            .flat_map(|record| &record.events)
            .find_map(|event| match (source, event) {
                (
                    ResultSource::Attempt { attempt },
                    RunEvent::AttemptRecorded {
                        call_id: id,
                        attempt: ordinal,
                        result:
                            AttemptResult::Done { output } | AttemptResult::Failed { output, .. },
                    },
                ) if id == call_id && ordinal == attempt => Some(output),
                (ResultSource::Cached, RunEvent::Admitted { round }) => round
                    .members
                    .iter()
                    .find(|member| member.call_id == *call_id)
                    .and_then(|member| member.checks.winner())
                    .and_then(|reply| match &reply.verdict {
                        BeforeCheckVerdict::Cached { result } => Some(result),
                        _ => None,
                    }),
                _ => None,
            });
        let reference = match source {
            ResultSource::DeferredCompletion { resolved, .. } => Some(resolved.as_ref()),
            _ => reference,
        }
        .ok_or_else(|| boundary(call_id))?;
        self.journal.materials.decode(reference).map_err(Into::into)
    }

    fn terminal(&self, call_id: &ToolCallId) -> Result<SingletonTerminal, SingletonRunError> {
        let presented = self
            .presented
            .get(call_id)
            .ok_or_else(|| boundary(call_id))?;
        if let CallDecision::Final { source, .. } = &presented.decision {
            let capture = self.call_capture(call_id, source)?;
            let presentation = match &presented.presentation {
                Some(reference) => self.journal.materials.read(reference)?.to_owned(),
                None => capture
                    .output()
                    .ok_or_else(|| boundary(call_id))?
                    .to_owned(),
            };
            Ok(SingletonTerminal::Final {
                source: source.clone(),
                capture,
                presentation,
                launched: presented.launched.clone(),
            })
        } else {
            Ok(SingletonTerminal::Withheld {
                decision: presented.decision.clone(),
            })
        }
    }

    fn select(
        &self,
        plan: &AggregatePlan,
        consumer: AggregateConsumer,
    ) -> Result<(Selection, Vec<Option<Settlement>>), SingletonRunError> {
        let mut settlements: Vec<Option<Settlement>> =
            (0..plan.leaves.len()).map(|_| None).collect();
        let admitted = self.journal.records.iter().flat_map(|record| record.events.iter().enumerate().map(move |(index, event)| (record.first.0 + index as u64, event)))
            .find_map(|(ordinal, event)| matches!(event, RunEvent::AggregateAdmitted { plan: recorded, .. } if recorded.key == plan.key).then_some(ordinal)).unwrap_or_default();
        for (index, leaf) in plan.leaves.iter().enumerate() {
            let position = plan
                .operands
                .iter()
                .position(|operand| *operand as usize == index)
                .ok_or_else(|| RunEventRefusal::AggregateShape {
                    key: plan.key.clone(),
                })?;
            if let AggregateLeaf::Settled { fulfilled } = leaf {
                settlements[index] = Some(Settlement {
                    fulfilled: *fulfilled,
                    order: (false, position as u64),
                    rank: None,
                });
            }
        }
        for record in &self.journal.records {
            for (index, event) in record.events.iter().enumerate() {
                let ordinal = record.first.0 + index as u64;
                match event {
                    RunEvent::TimerElapsed { aggregate, leaf } if *aggregate == plan.key => {
                        settlements[*leaf as usize] = Some(Settlement {
                            fulfilled: true,
                            order: (true, ordinal),
                            rank: None,
                        });
                    }
                    RunEvent::Decided {
                        call_id,
                        rank,
                        decision,
                        ..
                    } => {
                        let Some(leaf) = plan.leaves.iter().position(|leaf| matches!(leaf, AggregateLeaf::Call { call_id: id } if id == call_id)) else { continue; };
                        if matches!(decision, CallDecision::Cancelled | CallDecision::Aborted) {
                            return Ok((
                                Selection::HostControl(call_id.clone(), decision.clone()),
                                settlements,
                            ));
                        }
                        let fulfilled = match decision {
                            CallDecision::Final { source, .. } => matches!(
                                self.call_capture(call_id, source)?,
                                SingletonCapture::Done { .. } | SingletonCapture::Isolated { .. }
                            ),
                            _ => false,
                        };
                        let immediate = ordinal < admitted
                            || self
                                .journal
                                .records
                                .iter()
                                .flat_map(|record| &record.events)
                                .any(|event| match event {
                                    RunEvent::Admitted { round } => {
                                        round.members.iter().any(|member| {
                                            member.call_id == *call_id
                                                && member.selection() != BeforeSelection::Execute
                                        })
                                    }
                                    _ => false,
                                });
                        let position = plan
                            .operands
                            .iter()
                            .position(|operand| *operand as usize == leaf)
                            .ok_or_else(|| RunEventRefusal::AggregateShape {
                                key: plan.key.clone(),
                            })?;
                        settlements[leaf] = Some(Settlement {
                            fulfilled,
                            order: if immediate {
                                (false, position as u64)
                            } else {
                                (true, ordinal)
                            },
                            rank: Some(*rank),
                        });
                    }
                    _ => {}
                }
            }
        }
        let all = settlements.iter().all(Option::is_some);
        let decides = |settled: &Settlement| match consumer {
            AggregateConsumer::Race => true,
            AggregateConsumer::Any => settled.fulfilled,
            AggregateConsumer::All => !settled.fulfilled,
            AggregateConsumer::AllSettled | AggregateConsumer::ListBatch => false,
        };
        let selected = plan
            .operands
            .iter()
            .enumerate()
            .filter_map(|(position, leaf)| {
                let settled = settlements[*leaf as usize].as_ref()?;
                decides(settled).then_some((settled.order, position))
            })
            .min();
        let selection = if let Some((_, position)) = selected {
            Selection::Selected(position)
        } else if all {
            match consumer {
                AggregateConsumer::Race => Selection::Pending,
                AggregateConsumer::Any => Selection::Exhausted,
                AggregateConsumer::ListBatch => plan
                    .operands
                    .iter()
                    .position(|leaf| {
                        settlements[*leaf as usize]
                            .as_ref()
                            .is_some_and(|settled| !settled.fulfilled)
                    })
                    .map_or(Selection::All, Selection::Selected),
                AggregateConsumer::All | AggregateConsumer::AllSettled => Selection::All,
            }
        } else {
            Selection::Pending
        };
        Ok((selection, settlements))
    }
}
