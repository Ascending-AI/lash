//! The queued-work drain: what one drain claimed, what it refused, and why it
//! ran no turn.
//!
//! The automatic and exact drains share one implementation but publish
//! different answers, so both contracts live here next to the claim decisions
//! that produce them.

use super::*;

/// Why an exact host-selected queued-work set was refused before turn execution.
///
/// Missing durable rows are not a refusal: they idempotently satisfy their
/// requested IDs. Refusal means at least one still-present row could not be
/// executed under the requested atomic composition, or the execution lane was
/// unavailable; no selected turn was started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectedQueuedWorkDrainRefusalCause {
    UnclaimableTogether {
        unclaimed_batch_ids: Vec<crate::BatchId>,
    },
    InterruptedBatchRequiresFullComposition {
        required_batch_ids: Vec<crate::BatchId>,
    },
    ExecutionLaneBusy,
    /// One requested row alone renders larger than the whole model context
    /// window, so no drain policy can ever make it fit (FIG-1313).
    QueuedItemExceedsContextWindow {
        batch_id: crate::BatchId,
        batch_enqueue_seq: u64,
        required_context_tokens: usize,
        max_context_tokens: usize,
    },
}

/// Why an automatic queued-turn drain executed no turn.
///
/// An automatic drain names no batch ids, so there is nothing for a host to
/// inspect afterwards: this reason is the whole account of the empty drain.
/// Reading one variant as another is how queued work gets abandoned — a drain
/// that never reached its input is retryable, while an exhausted queue is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EmptyQueuedDrainReason {
    /// Another execution holds the session execution lane, so this drain never
    /// looked at the queue. Nothing was consumed and the work is retryable.
    ExecutionLaneBusy,
    /// The session has no durable store, so no queue exists to drain.
    NoDurableQueue,
    /// The queue was reachable and the claim state machine refused it.
    ClaimRefused(crate::QueuedWorkClaimRefusal),
}

impl EmptyQueuedDrainReason {
    /// The stable snake_case spelling, for host logs and metrics labels.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExecutionLaneBusy => "execution_lane_busy",
            Self::NoDurableQueue => "no_durable_queue",
            Self::ClaimRefused(refusal) => refusal.as_str(),
        }
    }
}

/// One automatic queued-turn drain: the turn it ran, or why it ran none.
#[derive(Clone, Debug)]
pub enum QueuedTurnDrain<T> {
    /// The drain claimed queued work and ran a turn.
    Ran(T),
    /// An explicit identity names an already settled run.
    Replayed(Box<crate::store::QueuedRunAdmission>),
    /// The drain ran no turn, for the named reason.
    Empty(EmptyQueuedDrainReason),
}

impl<T> QueuedTurnDrain<T> {
    /// The turn this drain ran, discarding the empty reason.
    pub fn ran(self) -> Option<T> {
        match self {
            Self::Ran(turn) => Some(turn),
            Self::Empty(_) | Self::Replayed(_) => None,
        }
    }

    /// Transforms the turn, preserving the empty reason.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> QueuedTurnDrain<U> {
        match self {
            Self::Ran(turn) => QueuedTurnDrain::Ran(f(turn)),
            Self::Replayed(receipt) => QueuedTurnDrain::Replayed(receipt),
            Self::Empty(reason) => QueuedTurnDrain::Empty(reason),
        }
    }

    #[track_caller]
    pub fn expect(self, message: &str) -> T {
        match self {
            Self::Ran(turn) => turn,
            Self::Replayed(_) => panic!("{message}: queued drain replayed its terminal receipt"),
            Self::Empty(reason) => {
                panic!("{message}: queued drain ran no turn ({})", reason.as_str())
            }
        }
    }
}

/// One drain's result before it is projected onto the caller's contract.
///
/// The automatic and exact drains share one implementation but publish different
/// answers: the exact drain reports per-id satisfaction, the automatic drain
/// reports the turn it ran or why it ran none. The variant is fixed by which
/// drain ran, so neither contract has to be reconstructed from the other's
/// evidence.
enum QueuedWorkDrainResult {
    /// An automatic drain: the turn it ran, or why it ran none.
    Automatic(QueuedTurnDrain<AssembledTurn>),
    /// An exact drain: per-requested-id satisfaction.
    Selected(SelectedQueuedWorkDrainOutcome<AssembledTurn>),
}

/// How one distinct requested batch ID satisfied a successful selected drain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectedQueuedWorkBatchSatisfaction {
    /// This invocation claimed and executed the durable row.
    ClaimedNow { batch_id: crate::BatchId },
    /// No durable row remained, so the idempotent request was already done.
    AlreadySatisfied { batch_id: crate::BatchId },
}

/// Successful result of an exact, host-selected queued-work drain.
///
/// Each distinct requested ID is either executed now or already absent from
/// durable storage. A present ID that cannot join the exact claim produces a
/// refusal instead of this type, before any selected turn executes.
#[derive(Clone, Debug)]
pub struct SelectedQueuedWorkDrainOutcome<T> {
    /// Executed turn, absent only for a fully satisfied drain that produced no
    /// selected turn (including an empty selection).
    pub turn: Option<T>,
    pub receipt: Option<Box<crate::store::QueuedRunAdmission>>,
    /// One entry per distinct requested ID, ordered by first occurrence.
    pub satisfied: Vec<SelectedQueuedWorkBatchSatisfaction>,
}

impl<T> SelectedQueuedWorkDrainOutcome<T> {
    fn new(turn: Option<T>, satisfied: Vec<SelectedQueuedWorkBatchSatisfaction>) -> Self {
        Self {
            turn,
            satisfied,
            receipt: None,
        }
    }

    /// Reports whether this successful drain settled every requested ID without
    /// executing a selected turn.
    ///
    /// Because refusals are returned as errors, `true` never means that
    /// selected work was busy or unclaimable. It means every distinct requested
    /// ID was satisfied without a selected turn, or the selection was empty.
    pub fn settled_without_selected_turn(&self) -> bool {
        self.turn.is_none()
    }

    /// `false` has the same fully-satisfied meaning as
    /// [`Self::settled_without_selected_turn`].
    pub fn executed_selected_turn(&self) -> bool {
        self.turn.is_some()
    }

    #[track_caller]
    #[expect(clippy::expect_used, reason = "the crate's own panicking accessor")]
    pub fn expect(self, message: &str) -> T {
        self.turn.expect(message)
    }
}

/// Error from an exact host-selected queued-work drain.
///
/// [`Self::Refused`] is a pre-execution atomicity result: absent rows count as
/// idempotently satisfied, while present rows that cannot form the requested
/// composition leave the selection unexecuted.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SelectedQueuedWorkDrainError {
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error("selected queued-work drain refused: {cause:?}")]
    Refused {
        cause: SelectedQueuedWorkDrainRefusalCause,
    },
}

impl LashRuntime {
    async fn session_commands_precede_pending_turn_input(
        &self,
        store: &dyn crate::RuntimePersistence,
    ) -> Result<bool, RuntimeError> {
        Ok(store
            .pending_session_work_ordering(&self.state.session_id)
            .await
            .map_err(super::runtime_error_from_store_commit)?
            .session_command_precedes_turn_input())
    }

    pub async fn stream_next_queued_work<'a>(
        &mut self,
        opts: impl Into<QueuedTurnOptions<'a>>,
    ) -> Result<QueuedTurnDrain<AssembledTurn>, RuntimeError> {
        match Box::pin(self.stream_queued_work(opts.into(), None)).await {
            Ok(QueuedWorkDrainResult::Automatic(drain)) => Ok(drain),
            Ok(QueuedWorkDrainResult::Selected(_)) => {
                unreachable!("an automatic drain cannot produce a selected outcome")
            }
            Err(SelectedQueuedWorkDrainError::Runtime(error)) => Err(error),
            // Selected-drain refusals reason about requested batch ids, which an
            // automatic drain never has. Naming the cause beats panicking: this
            // API's whole contract is that it says why it ran no turn.
            Err(SelectedQueuedWorkDrainError::Refused { cause }) => Err(RuntimeError::new(
                RuntimeErrorCode::QueuedWork,
                format!("automatic queued-work drain refused: {cause:?}"),
            )),
        }
    }

    pub async fn stream_selected_queued_work<'a>(
        &mut self,
        opts: impl Into<QueuedTurnOptions<'a>>,
        batch_ids: &[crate::BatchId],
    ) -> Result<SelectedQueuedWorkDrainOutcome<AssembledTurn>, SelectedQueuedWorkDrainError> {
        Box::pin(self.stream_queued_work(opts.into(), Some(batch_ids)))
            .await
            .map(|result| match result {
                QueuedWorkDrainResult::Selected(outcome) => outcome,
                QueuedWorkDrainResult::Automatic(_) => {
                    unreachable!("a selected drain cannot produce an automatic outcome")
                }
            })
    }

    async fn stream_queued_work(
        &mut self,
        queued_opts: QueuedTurnOptions<'_>,
        selected_batch_ids: Option<&[crate::BatchId]>,
    ) -> Result<QueuedWorkDrainResult, SelectedQueuedWorkDrainError> {
        let selected = selected_batch_ids.map(|ids| {
            let mut seen = std::collections::BTreeSet::new();
            ids.iter()
                .filter(|id| seen.insert((*id).clone()))
                .cloned()
                .collect::<Vec<_>>()
        });
        let Some(lease) = self
            .claim_session_execution_lease_for_queued_work(&queued_opts)
            .await?
        else {
            return if selected.is_some() {
                Err(SelectedQueuedWorkDrainError::Refused {
                    cause: SelectedQueuedWorkDrainRefusalCause::ExecutionLaneBusy,
                })
            } else {
                Ok(QueuedWorkDrainResult::Automatic(QueuedTurnDrain::Empty(
                    if self
                        .session
                        .as_ref()
                        .and_then(|session| session.history_store())
                        .is_none()
                    {
                        EmptyQueuedDrainReason::NoDurableQueue
                    } else {
                        EmptyQueuedDrainReason::ExecutionLaneBusy
                    },
                )))
            };
        };
        let store = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::QueuedWork,
                    "queued admission requires persistence",
                )
            })?;
        let fence = lease.fence();
        if let Err(error) = self
            .reload_invalidated_resident_session_state_under_lease(Some(&lease))
            .await
        {
            let _ = lease.release_if_live().await;
            return Err(error.into());
        }
        if let Err(error) = self.refresh_session_graph_from_store().await {
            let _ = lease.release_if_live().await;
            return Err(session_head_refresh_error(error).into());
        }
        let anonymous_caller = queued_opts.source.identity().is_none();
        let request = crate::store::BeginQueuedRun {
            session_id: self.state.session_id.clone(),
            identity: queued_opts.source.identity(),
            request: selected
                .clone()
                .map_or(crate::store::QueuedRunRequest::Automatic, |batch_ids| {
                    crate::store::QueuedRunRequest::Selected { batch_ids }
                }),
            configuration: crate::store::persisted_session_config_from_state(&self.state),
            expected_head_revision: self.state.head_revision,
            initial_turn_index: crate::StoreError::checked_monotonic_increment(
                "queued_run_turn_index",
                self.state.turn_index as u64,
            )
            .map_err(super::runtime_error_from_store_commit)?,
        };
        let admission = match store.begin_or_resume_queued_run(&fence, request).await {
            Ok(run) => run,
            Err(error) => {
                let _ = lease.release_if_live().await;
                return Err(super::runtime_error_from_store_commit(error).into());
            }
        };
        if let Some(terminal) = &admission.terminal {
            // A retry of a settled run is how a drain that crashed after its
            // terminal settlement, before its end, reaches the epilogue again
            // (FIG-3419): the persisted scope is the same owner, and every
            // terminal settlement — a durable `Failed` included (FIG-3559) —
            // is an end.
            Box::pin(self.end_queue_drain(&admission.scope, &lease, &store, false)).await;
            if selected.is_some()
                && let crate::store::QueuedRunTerminal::Failed { message, .. } = terminal
            {
                let _ = lease.release_if_live().await;
                return Err(
                    RuntimeError::new(RuntimeErrorCode::QueuedRunFailed, message.clone()).into(),
                );
            }
            lease
                .release_if_live()
                .await
                .map_err(super::runtime_error_from_store_commit)?;
            return Ok(match selected {
                Some(ids) => QueuedWorkDrainResult::Selected(SelectedQueuedWorkDrainOutcome {
                    turn: None,
                    satisfied: ids
                        .into_iter()
                        .map(
                            |batch_id| SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied {
                                batch_id,
                            },
                        )
                        .collect(),
                    receipt: Some(Box::new(admission)),
                }),
                None => {
                    QueuedWorkDrainResult::Automatic(QueuedTurnDrain::Replayed(Box::new(admission)))
                }
            });
        }
        let preparation = async {
            let opts = queued_opts.bind(admission.scope.clone())?;
            self.defer_orphaned_turn_inputs_before_drain(
                &store,
                &fence,
                &TurnId::from(admission.scope.id()),
                &opts.scoped_effect_controller(),
            )
            .await?;
            if admission.members.is_none() {
                let commands_first = selected.is_some()
                    || self
                        .session_commands_precede_pending_turn_input(store.as_ref())
                        .await?;
                if commands_first {
                    let controller = opts.scoped_effect_controller();
                    while self
                        .drain_next_session_command_with_cancellation(
                            &fence,
                            opts.local_stop().immediate_token(),
                            controller.controller(),
                        )
                        .await?
                        .is_some()
                    {}
                }
            }
            Ok::<_, RuntimeError>(opts)
        }
        .await;
        let opts = match preparation {
            Ok(opts) => opts,
            Err(error) => {
                let error = self
                    .retain_or_settle_queued_error(
                        &store,
                        &lease,
                        &admission,
                        anonymous_caller,
                        error,
                    )
                    .await;
                let _ = lease.release_if_live().await;
                return Err(error.into());
            }
        };
        let selection = match store
            .select_queued_run(
                &fence,
                &admission.scope,
                &self.runtime_lease_owner,
                self.host
                    .core
                    .durability
                    .queued_work_batching
                    .max_turn_input_claim(),
                &crate::store::persisted_session_config_from_state(&self.state),
                self.host
                    .core
                    .durability
                    .queued_work_batching
                    .claim_policy(self.max_context_tokens()),
            )
            .await
        {
            Ok(selection) => selection,
            Err(error) => {
                if admission.members.is_none()
                    && matches!(
                        &error,
                        crate::StoreError::SelectedQueuedRunIncomplete { .. }
                            | crate::StoreError::SelectedQueuedWorkRequiresInterruptedComposition { .. }
                            | crate::StoreError::QueuedWorkRowExceedsContextWindow { .. }
                    )
                {
                    self.settle_failed_queued_run(
                        &store,
                        &lease,
                        failed_settlement(
                            &admission,
                            anonymous_caller,
                            RuntimeErrorCode::QueuedWork,
                            error.to_string(),
                        ),
                    )
                    .await
                    .map_err(super::runtime_error_from_store_commit)?;
                }
                let _ = lease.release_if_live().await;
                return Err(match error {
                    crate::StoreError::SelectedQueuedWorkRequiresInterruptedComposition { required_batch_ids } => SelectedQueuedWorkDrainError::Refused {
                        cause: SelectedQueuedWorkDrainRefusalCause::InterruptedBatchRequiresFullComposition { required_batch_ids: required_batch_ids.into_iter().map(crate::BatchId::new).collect() },
                    },
                    crate::StoreError::QueuedWorkRowExceedsContextWindow { batch_id, batch_enqueue_seq, rendered_tokens, max_context_tokens } if selected.is_some() => SelectedQueuedWorkDrainError::Refused {
                        cause: SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow { batch_id, batch_enqueue_seq, required_context_tokens: rendered_tokens, max_context_tokens },
                    },
                    crate::StoreError::SelectedQueuedRunIncomplete { unclaimed_batch_ids } => SelectedQueuedWorkDrainError::Refused {
                        cause: SelectedQueuedWorkDrainRefusalCause::UnclaimableTogether { unclaimed_batch_ids },
                    },
                    error @ crate::StoreError::QueuedWorkRowExceedsContextWindow { .. } => RuntimeError::new(RuntimeErrorCode::QueuedWorkRowExceedsContextWindow, error.to_string()).into(),
                    error => super::runtime_error_from_store_commit(error).into(),
                });
            }
        };
        let satisfaction = selected
            .as_ref()
            .map(|ids| {
                ids.iter()
                    .map(|batch_id| {
                        if selection.already_satisfied.contains(batch_id) {
                            SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied {
                                batch_id: batch_id.clone(),
                            }
                        } else {
                            SelectedQueuedWorkBatchSatisfaction::ClaimedNow {
                                batch_id: batch_id.clone(),
                            }
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        if selection.inputs.is_empty()
            && selection.queued.is_empty()
            && selection.admission.position.physical_ordinal == 0
        {
            if selection.admission.members.is_none() {
                lease
                    .release_if_live()
                    .await
                    .map_err(super::runtime_error_from_store_commit)?;
                return Err(super::runtime_error_from_store_commit(
                    crate::StoreError::QueuedRunConflict {
                        session_id: self.state.session_id.clone(),
                    },
                )
                .into());
            }
            store
                .settle_queued_run(
                    &fence,
                    crate::store::QueuedRunCommit {
                        scope: selection.admission.scope.clone(),
                        expected_revision: selection.admission.revision,
                        progress: if anonymous_caller && selection.admission.can_forget_unworked() {
                            crate::store::QueuedRunProgress::ForgetUnworked
                        } else {
                            crate::store::QueuedRunProgress::Settle {
                                terminal: crate::store::QueuedRunTerminal::Empty,
                            }
                        },
                    },
                )
                .await
                .map_err(super::runtime_error_from_store_commit)?;
            Box::pin(self.end_queue_drain(&selection.admission.scope, &lease, &store, false)).await;
            lease
                .release_if_live()
                .await
                .map_err(super::runtime_error_from_store_commit)?;
            return Ok(if selected.is_some() {
                QueuedWorkDrainResult::Selected(SelectedQueuedWorkDrainOutcome::new(
                    None,
                    satisfaction,
                ))
            } else {
                QueuedWorkDrainResult::Automatic(QueuedTurnDrain::Empty(
                    EmptyQueuedDrainReason::ClaimRefused(
                        selection
                            .refusal
                            .unwrap_or(crate::QueuedWorkClaimRefusal::Empty),
                    ),
                ))
            });
        }
        self.queued_run_reacquired = Default::default();
        let (mut input, claims) = self.queued_run_input(selection, true)?;
        if selected.is_some() {
            input.turn_context.mark_selected_queued_work_drain();
        }
        let mut lease = Some(lease);
        let mut result = self
            .drive_logical_turn(
                LogicalTurnStart::Input(input),
                opts.events_or_noop(),
                opts.turn_events_or_noop(),
                opts.scoped_effect_controller(),
                opts.local_stop().clone(),
                claims,
                &mut lease,
                TurnStopwatch::start(self.host.core.clock.as_ref()),
            )
            .await;
        if let Err(error) = result {
            result = Err(match (self.queued_run.take(), lease.as_ref()) {
                (Some(run), Some(held)) => {
                    self.retain_or_settle_queued_error(&store, held, &run, anonymous_caller, error)
                        .await
                }
                _ => error,
            });
        }
        self.queued_run = None;
        self.queued_run_reacquired = Default::default();
        if result.is_ok()
            && let Some(held) = lease.as_ref()
        {
            Box::pin(self.end_queue_drain(&admission.scope, held, &store, true)).await;
        }
        let result = self
            .settle_session_execution_lease(lease.as_ref(), result)
            .await?;
        let turn = result.into_final_turn();
        Ok(if selected.is_some() {
            QueuedWorkDrainResult::Selected(SelectedQueuedWorkDrainOutcome::new(turn, satisfaction))
        } else {
            match turn {
                Some(turn) => QueuedWorkDrainResult::Automatic(QueuedTurnDrain::Ran(turn)),
                None => QueuedWorkDrainResult::Automatic(QueuedTurnDrain::Empty(
                    EmptyQueuedDrainReason::ClaimRefused(crate::QueuedWorkClaimRefusal::Empty),
                )),
            }
        })
    }

    pub(in crate::runtime) fn queued_run_input(
        &mut self,
        selected: crate::store::SelectedQueuedRun,
        announce_claims: bool,
    ) -> Result<(TurnInput, LogicalTurnClaims), RuntimeError> {
        let mut input = TurnInput::items(Vec::new());
        for member in selected.admission.members.as_deref().unwrap_or_default() {
            match member {
                crate::store::QueuedRunMember::Input(id) => {
                    let pending = selected
                        .inputs
                        .iter()
                        .flat_map(|claim| &claim.inputs)
                        .find(|pending| pending.input_id == id)
                        .ok_or_else(|| {
                            RuntimeError::new(
                                RuntimeErrorCode::QueuedRunPending,
                                "Admitted input is missing from its claims",
                            )
                        })?;
                    input.items.extend(pending.input.items.clone());
                    if input.protocol_turn_options.is_none() {
                        input.protocol_turn_options = pending.input.protocol_turn_options.clone();
                    }
                }
                crate::store::QueuedRunMember::Batch(id) => {
                    let batch = selected
                        .queued
                        .iter()
                        .flat_map(|claim| &claim.batches)
                        .find(|batch| batch.batch_id == id)
                        .ok_or_else(|| {
                            RuntimeError::new(
                                RuntimeErrorCode::QueuedRunPending,
                                "Admitted batch is missing from its claims",
                            )
                        })?;
                    for item in &batch.items {
                        if let crate::QueuedWorkPayload::AgentFrameTask {
                            task,
                            protocol_turn_options,
                            ..
                        } = &item.payload
                        {
                            input.items.push(crate::InputItem::text(task.clone()));
                            input.protocol_turn_options = protocol_turn_options.clone();
                        }
                    }
                }
            }
        }
        if announce_claims {
            let turn_index =
                usize::try_from(selected.admission.position.turn_index).map_err(|_| {
                    RuntimeError::new(
                        RuntimeErrorCode::StoreCommitFailed,
                        "admitted turn index exceeds platform range",
                    )
                })?;
            let trace_context = lash_trace::TraceContext::default()
                .for_session(self.state.session_id.clone())
                .for_turn_index(turn_index)
                .for_turn(selected.admission.position.turn_id.clone());
            for claim in &selected.inputs {
                crate::trace::emit_trace(
                    &self.host.core.tracing.trace_sink,
                    &self.host.core.tracing.trace_context,
                    trace_context.clone(),
                    lash_trace::TraceEvent::Custom {
                        name: "turn_input.claimed".to_string(),
                        payload: serde_json::json!({
                            "claim_id": &claim.claim_id,
                            "input_ids": claim.inputs.iter().map(|input| &input.input_id).collect::<Vec<_>>(),
                        }),
                    },
                    self.host.core.clock.as_ref(),
                );
            }
            for claim in &selected.queued {
                let materialized = claim.materialize_queued_turn_work();
                crate::trace::emit_trace(
                    &self.host.core.tracing.trace_sink,
                    &self.host.core.tracing.trace_context,
                    trace_context.clone(),
                    lash_trace::TraceEvent::Custom {
                        name: "queued_work.claimed".to_string(),
                        payload: super::queued_work_trace_payload(
                            crate::QueuedWorkClaimBoundary::Idle,
                            claim,
                            &materialized.turn_causes,
                        ),
                    },
                    self.host.core.clock.as_ref(),
                );
            }
        }
        input.trace_turn_id = Some(selected.admission.position.turn_id.clone());
        self.queued_run_reacquired
            .queued
            .extend(selected.reacquired_queued);
        self.queued_run_reacquired
            .turn_inputs
            .extend(selected.reacquired_inputs);
        self.queued_run = Some(Box::new(selected.admission));
        Ok((
            input,
            LogicalTurnClaims::new(selected.queued, selected.inputs),
        ))
    }
}

impl LashRuntime {
    /// Dispose of a queued run's error by its cause (FIG-3575).
    ///
    /// A live fault keeps the run for its retry budget: a retryable one
    /// passes through, any other stays pending. A refusal that parks the turn
    /// (FIG-3586) is never retryable, so its run stays pending without
    /// spending the budget. An outcome settles the run failed once; a
    /// deterministic failure is never retried.
    async fn retain_or_settle_queued_error(
        &mut self,
        store: &Arc<dyn crate::store::RuntimePersistence>,
        lease: &SessionExecutionLeaseGuard,
        run: &crate::store::QueuedRunAdmission,
        anonymous_caller: bool,
        error: RuntimeError,
    ) -> RuntimeError {
        if error.turn_failure_cause().aborts_invocation() {
            if error.is_retryable() {
                return error;
            }
            return RuntimeError::new(
                RuntimeErrorCode::QueuedRunPending,
                format!("queued run remains recoverable: {error}"),
            );
        }
        if let Err(disposition) = self
            .settle_failed_queued_run(
                store,
                lease,
                failed_settlement(
                    run,
                    anonymous_caller,
                    error.code.clone(),
                    error.message.clone(),
                ),
            )
            .await
        {
            return RuntimeError::new(
                RuntimeErrorCode::QueuedRunPending,
                format!("queued terminal disposition remains pending: {disposition}"),
            );
        }
        error
    }

    /// Settle a queued run terminally failed and then end its drain.
    ///
    /// Every terminal failure of a queued run comes through here: the
    /// runtime's own terminal errors and a host's
    /// [`abandon_queued_run`](Self::abandon_queued_run). A durable `Failed` is
    /// terminal: nothing retries it, so no later drain would end it, and the
    /// settlement is the drain's end exactly as a successful run's commit is
    /// (FIG-3559, FIG-3560). The drain-end epilogue therefore runs here, under
    /// the lane the caller still holds, and decides as it does on every other
    /// end path — a drain that owns no children has no end to write, and one
    /// whose closing work is still owed elsewhere is withheld and written
    /// later by [`end_settled_queue_drain`](Self::end_settled_queue_drain).
    async fn settle_failed_queued_run(
        &mut self,
        store: &Arc<dyn crate::store::RuntimePersistence>,
        lease: &SessionExecutionLeaseGuard,
        settlement: crate::store::QueuedRunCommit,
    ) -> Result<crate::store::QueuedRunAdmission, crate::StoreError> {
        let settled = store.settle_queued_run(&lease.fence(), settlement).await?;
        Box::pin(self.end_queue_drain(&settled.scope, lease, store, false)).await;
        Ok(settled)
    }

    /// Abandon this session's unfinished queued run: settle it durably
    /// `Failed` with `reason` and end its drain.
    ///
    /// This is the host's disposition for a run it no longer wants recovered
    /// (ADR 0099): `scope` and `expected_revision` name the run exactly as
    /// [`pending_queued_run`](crate::store::QueuedWorkStore::pending_queued_run)
    /// reported it, and a stale revision is refused. The run's assigned work
    /// is cancelled and its failed-terminal evidence retained. Abandonment is
    /// a terminal settlement like any other, so it ends the drain through the
    /// same epilogue: the drain's closing groups settle first — a live tool
    /// child under one is waited out — then the end receipt and ledger row
    /// land and the sweep cancels the drain's `Cancel` children (FIG-3560).
    ///
    /// The runtime claims the session execution lane itself; a busy lane is
    /// the retryable [`RuntimeErrorCode::SessionExecutionLaneBusy`]. A retry
    /// of a completed abandonment replays the settlement and its end.
    pub async fn abandon_queued_run(
        &mut self,
        scope: crate::ExecutionScope,
        expected_revision: u64,
        reason: String,
    ) -> Result<crate::store::QueuedRunAdmission, RuntimeError> {
        let (store, lease) = self
            .claim_lane_for_settled_drain_work("abandon its queued run")
            .await?;
        let settled = self
            .settle_failed_queued_run(
                &store,
                &lease,
                crate::store::QueuedRunCommit {
                    scope,
                    expected_revision,
                    progress: crate::store::QueuedRunProgress::Settle {
                        terminal: crate::store::QueuedRunTerminal::Failed {
                            code: RuntimeErrorCode::QueuedWork,
                            message: reason,
                        },
                    },
                },
            )
            .await;
        let released = lease.release_if_live().await;
        let settled = settled.map_err(super::runtime_error_from_store_commit)?;
        released.map_err(super::runtime_error_from_store_commit)?;
        Ok(settled)
    }

    /// Write the end a settled drain still owes (FIG-3563).
    ///
    /// Every terminal settlement is a drain end, and its epilogue runs right
    /// after it — but the epilogue withholds the end while a closing group
    /// under the drain's scope owes work leased to another host, and nothing
    /// retries a settled run to ask again: a durable `Failed` is never
    /// retried, and a completed run's caller has its answer. This is that
    /// later ask. It claims the session execution lane, reads the drain's run
    /// by its scope, and — only when the run is terminally settled — runs the
    /// same epilogue a replayed settled drain runs: the closing groups resume
    /// (finishing any obligation whose lease has since settled or expired),
    /// and the receipt and ledger row land once nothing is owed. A run still
    /// pending is interrupted, not ended, and is left for its own retry; a
    /// drain whose end already landed replays the receipt as a no-op.
    ///
    /// The parent-end recovery sweep is the caller: it finds a drain whose
    /// registry-listed children name an owner with no end receipt, and calls
    /// this once the drain's run reads settled. No input is consumed and the
    /// drain id is not replayed through admission. A busy lane is the
    /// retryable [`RuntimeErrorCode::SessionExecutionLaneBusy`]; the sweep's
    /// next pass asks again.
    pub async fn end_settled_queue_drain(&mut self, drain_id: &str) -> Result<(), RuntimeError> {
        let (store, lease) = self
            .claim_lane_for_settled_drain_work("end a settled queue drain")
            .await?;
        let scope = crate::ExecutionScope::queue_drain(self.state.session_id.clone(), drain_id);
        let run = match store.queued_run(&scope).await {
            Ok(run) => run,
            Err(error) => {
                let _ = lease.release_if_live().await;
                return Err(super::runtime_error_from_store_commit(error));
            }
        };
        if run.is_some_and(|run| run.terminal.is_some()) {
            Box::pin(self.end_queue_drain(&scope, &lease, &store, false)).await;
        }
        lease
            .release_if_live()
            .await
            .map_err(super::runtime_error_from_store_commit)
    }

    /// Claim the session execution lane for a disposition of queued-run work
    /// outside a drain — an abandonment or an owed end — and bring the
    /// resident state up to the committed head under it.
    async fn claim_lane_for_settled_drain_work(
        &mut self,
        purpose: &str,
    ) -> Result<
        (
            Arc<dyn crate::store::RuntimePersistence>,
            SessionExecutionLeaseGuard,
        ),
        RuntimeError,
    > {
        let store = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::QueuedWork,
                    format!("a runtime needs persistence to {purpose}"),
                )
            })?;
        let Some(lease) = SessionExecutionLeaseGuard::try_acquire_for_executor(
            Arc::clone(&store),
            &self.state.session_id,
            &self.runtime_lease_owner,
            &self.runtime_lease_executor_id,
            self.host.core.control.lease_timings,
            Arc::clone(&self.host.core.clock),
        )
        .await
        .map_err(super::runtime_error_from_store_commit)?
        else {
            return Err(RuntimeError::new(
                RuntimeErrorCode::SessionExecutionLaneBusy,
                format!(
                    "session `{}` cannot {purpose} until it acquires the execution lane",
                    self.state.session_id
                ),
            ));
        };
        if let Err(error) = self
            .reload_invalidated_resident_session_state_under_lease(Some(&lease))
            .await
        {
            let _ = lease.release_if_live().await;
            return Err(error);
        }
        if let Err(error) = self.refresh_session_graph_from_store().await {
            let _ = lease.release_if_live().await;
            return Err(session_head_refresh_error(error));
        }
        Ok((store, lease))
    }
}

/// The terminal failure of `run`: durably `Failed`, or forgotten when an
/// anonymous caller's run never worked anything.
fn failed_settlement(
    run: &crate::store::QueuedRunAdmission,
    anonymous_caller: bool,
    code: RuntimeErrorCode,
    message: String,
) -> crate::store::QueuedRunCommit {
    crate::store::QueuedRunCommit {
        scope: run.scope.clone(),
        expected_revision: run.revision,
        progress: if anonymous_caller && run.can_forget_unworked() {
            crate::store::QueuedRunProgress::ForgetUnworked
        } else {
            crate::store::QueuedRunProgress::Settle {
                terminal: crate::store::QueuedRunTerminal::Failed { code, message },
            }
        },
    }
}
