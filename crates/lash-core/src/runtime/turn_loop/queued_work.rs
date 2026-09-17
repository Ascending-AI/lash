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
    /// The drain ran no turn, for the named reason.
    Empty(EmptyQueuedDrainReason),
}

impl<T> QueuedTurnDrain<T> {
    /// The turn this drain ran, discarding the empty reason.
    pub fn ran(self) -> Option<T> {
        match self {
            Self::Ran(turn) => Some(turn),
            Self::Empty(_) => None,
        }
    }

    /// Transforms the turn, preserving the empty reason.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> QueuedTurnDrain<U> {
        match self {
            Self::Ran(turn) => QueuedTurnDrain::Ran(f(turn)),
            Self::Empty(reason) => QueuedTurnDrain::Empty(reason),
        }
    }

    /// Returns the turn this drain ran, or panics with `message`.
    #[track_caller]
    pub fn expect(self, message: &str) -> T {
        match self {
            Self::Ran(turn) => turn,
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
    /// One entry per distinct requested ID, ordered by first occurrence.
    pub satisfied: Vec<SelectedQueuedWorkBatchSatisfaction>,
}

impl<T> SelectedQueuedWorkDrainOutcome<T> {
    fn new(turn: Option<T>, satisfied: Vec<SelectedQueuedWorkBatchSatisfaction>) -> Self {
        Self { turn, satisfied }
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

    /// Reports whether this successful drain executed a newly claimed turn.
    ///
    /// `false` has the same fully-satisfied meaning as
    /// [`Self::settled_without_selected_turn`].
    pub fn executed_selected_turn(&self) -> bool {
        self.turn.is_some()
    }

    /// Returns the executed turn or panics with `message` after a successful
    /// drain that was fully satisfied without running a selected turn.
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
    pub async fn stream_next_queued_work(
        &mut self,
        opts: TurnOptions<'_>,
    ) -> Result<QueuedTurnDrain<AssembledTurn>, RuntimeError> {
        match self.stream_queued_work(opts, None).await {
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

    pub async fn stream_selected_queued_work(
        &mut self,
        opts: TurnOptions<'_>,
        batch_ids: &[crate::BatchId],
    ) -> Result<SelectedQueuedWorkDrainOutcome<AssembledTurn>, SelectedQueuedWorkDrainError> {
        self.stream_queued_work(opts, Some(batch_ids))
            .await
            .map(|result| match result {
                QueuedWorkDrainResult::Selected(outcome) => outcome,
                QueuedWorkDrainResult::Automatic(_) => {
                    unreachable!("a selected drain cannot produce an automatic outcome")
                }
            })
    }

    #[expect(
        clippy::expect_used,
        reason = "an automatic drain records its turn or its claim refusal"
    )]
    async fn stream_queued_work(
        &mut self,
        opts: TurnOptions<'_>,
        selected_batch_ids: Option<&[crate::BatchId]>,
    ) -> Result<QueuedWorkDrainResult, SelectedQueuedWorkDrainError> {
        let selected_batch_ids = selected_batch_ids.map(|batch_ids| {
            let mut seen = std::collections::BTreeSet::new();
            batch_ids
                .iter()
                .filter(|batch_id| seen.insert(batch_id.as_str()))
                .cloned()
                .collect::<Vec<_>>()
        });
        let selected_batch_ids = selected_batch_ids.as_deref();
        let stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
        let cancel = opts.cancel.clone();
        let Some(session_execution_lease) = self
            .claim_session_execution_lease_for_queued_work(&opts)
            .await?
        else {
            if let Some(batch_ids) = selected_batch_ids
                && let Some(store) = self
                    .session
                    .as_ref()
                    .and_then(|session| session.history_store())
            {
                let present_ids = store
                    .list_queued_work(&self.state.session_id)
                    .await
                    .map_err(super::runtime_error_from_store_commit)?
                    .into_iter()
                    .map(|batch| batch.batch_id)
                    .collect::<std::collections::BTreeSet<_>>();
                if batch_ids
                    .iter()
                    .all(|batch_id| !present_ids.contains(batch_id))
                {
                    return Ok(QueuedWorkDrainResult::Selected(
                        SelectedQueuedWorkDrainOutcome::new(
                            None,
                            batch_ids
                                .iter()
                                .cloned()
                                .map(|batch_id| {
                                    SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied {
                                        batch_id,
                                    }
                                })
                                .collect(),
                        ),
                    ));
                }
            }
            return if selected_batch_ids.is_some() {
                Err(SelectedQueuedWorkDrainError::Refused {
                    cause: SelectedQueuedWorkDrainRefusalCause::ExecutionLaneBusy,
                })
            } else {
                // The lane claim declines for two different reasons, and only one
                // of them is retryable: a session with no durable store has no
                // queue at all, while a busy lane means someone else is draining
                // work this caller can still get later.
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
        // This snapshot stays current while leading commands drain because
        // `RefreshToolCatalog` never acquires a fresh session lease; any later
        // lease rotation happens only after this command-drain window.
        let session_execution_fence = session_execution_lease.fence();
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            session_execution_lease
                .release_if_live()
                .await
                .map_err(|err| {
                    RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string())
                })?;
            return Ok(if selected_batch_ids.is_some() {
                QueuedWorkDrainResult::Selected(SelectedQueuedWorkDrainOutcome::new(
                    None,
                    Vec::new(),
                ))
            } else {
                QueuedWorkDrainResult::Automatic(QueuedTurnDrain::Empty(
                    EmptyQueuedDrainReason::NoDurableQueue,
                ))
            });
        };
        let activation_controller = opts.scoped_effect_controller();
        if let Err(error) = self
            .defer_orphaned_turn_inputs_before_drain(
                &store,
                &session_execution_fence,
                &TurnId::from(opts.execution_scope_id()),
                &activation_controller,
            )
            .await
        {
            let _ = session_execution_lease.release_if_live().await;
            return Err(error.into());
        }
        let drain_commands_before_turn_input = if selected_batch_ids.is_some() {
            true
        } else {
            self.session_commands_precede_pending_turn_input(store.as_ref())
                .await?
        };
        if drain_commands_before_turn_input {
            let command_controller = opts.scoped_effect_controller();
            loop {
                match self
                    .drain_next_session_command_with_cancellation(
                        &session_execution_fence,
                        cancel.clone(),
                        command_controller.controller(),
                    )
                    .await
                {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(err) => {
                        let _ = session_execution_lease.release_if_live().await;
                        return Err(err.into());
                    }
                }
            }
        }
        if selected_batch_ids.is_none() {
            let input_claim = store
                .claim_next_turn_inputs(
                    &self.state.session_id,
                    &session_execution_fence,
                    &self.runtime_lease_owner,
                    MAX_CLAIMED_TURN_INPUTS,
                )
                .await
                .map_err(super::runtime_error_from_store_commit)?;
            if let Some(input_claim) = input_claim {
                let mut input = input_claim.materialize_turn_input();
                if let Some(hint) = opts.local_cancel_origin_hint() {
                    input.turn_context.set_local_cancel_origin_hint(hint);
                }
                let turn_id = input
                    .trace_turn_id
                    .clone()
                    .unwrap_or_else(|| TurnId::from(opts.execution_scope_id()));
                input.trace_turn_id = Some(turn_id.clone());
                crate::trace::emit_trace(
                    &self.host.core.tracing.trace_sink,
                    &self.host.core.tracing.trace_context,
                    lash_trace::TraceContext::default()
                        .for_session(self.state.session_id.clone())
                        // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                        .for_turn_index(self.state.turn_index + 1)
                        .for_turn(turn_id.clone()),
                    lash_trace::TraceEvent::Custom {
                        name: "turn_input.claimed".to_string(),
                        payload: serde_json::json!({
                            "claim_id": &input_claim.claim_id,
                            "input_ids": input_claim.inputs.iter().map(|input| input.input_id.clone()).collect::<Vec<_>>(),
                        }),
                    },
                    self.host.core.clock.as_ref(),
                );
                let claim_for_abandon =
                    super::turn_input_ingress::TurnInputDrive::Claimed(input_claim.clone());
                let scoped_effect_controller = opts.scoped_effect_controller();
                let mut session_execution_lease = Some(session_execution_lease);
                let result = Box::pin(self.drive_logical_turn(
                    LogicalTurnStart::Input(input),
                    opts.events_or_noop(),
                    opts.turn_events_or_noop(),
                    scoped_effect_controller,
                    None,
                    cancel,
                    LogicalTurnClaims::new(
                        Vec::new(),
                        vec![super::turn_input_ingress::TurnInputDrive::Claimed(
                            input_claim,
                        )],
                    ),
                    &mut session_execution_lease,
                    stopwatch,
                ))
                .await
                .map(AgentFrameRun::into_final_turn);
                if let Err(err) = &result {
                    self.abandon_turn_input_claims_after_local_abort(
                        err,
                        std::slice::from_ref(&claim_for_abandon),
                    )
                    .await;
                }
                return self
                    .settle_session_execution_lease(session_execution_lease.as_ref(), result)
                    .await
                    .map(|turn| {
                        QueuedWorkDrainResult::Automatic(QueuedTurnDrain::Ran(
                            turn.expect("logical turn always contains a terminal physical turn"),
                        ))
                    })
                    .map_err(Into::into);
            }
        }
        // Both claim families answer with the rows they took plus, when they took
        // none, the refusal that explains it. An exact drain reasons about its
        // requested ids instead, so only the automatic family's refusal travels.
        let claim: Result<
            (
                crate::SelectedQueuedWorkClaimOutcome,
                Option<crate::QueuedWorkClaimRefusal>,
            ),
            crate::StoreError,
        > = if let Some(batch_ids) = selected_batch_ids {
            let claim_policy = self
                .host
                .core
                .durability
                .queued_work_batching
                .claim_policy(self.max_context_tokens());
            store
                .claim_ready_queued_work_by_batch_ids(
                    &self.state.session_id,
                    &session_execution_fence,
                    &self.runtime_lease_owner,
                    crate::QueuedWorkClaimBoundary::Idle,
                    batch_ids,
                    claim_policy,
                )
                .await
                .map(|outcome| (outcome, None))
        } else {
            let claim_policy = self
                .host
                .core
                .durability
                .queued_work_batching
                .claim_policy(self.max_context_tokens());
            store
                .claim_ready_queued_work(
                    &self.state.session_id,
                    &session_execution_fence,
                    &self.runtime_lease_owner,
                    crate::QueuedWorkClaimBoundary::Idle,
                    claim_policy,
                )
                .await
                .map(|outcome| {
                    let refusal = outcome.refusal();
                    (
                        crate::SelectedQueuedWorkClaimOutcome::new(outcome.claim(), Vec::new()),
                        refusal,
                    )
                })
        };
        let claim_outcome = match claim {
            Err(crate::StoreError::SelectedQueuedWorkRequiresInterruptedComposition {
                required_batch_ids,
            }) => {
                session_execution_lease
                    .release_if_live()
                    .await
                    .map_err(|err| {
                        RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string())
                    })?;
                return Err(SelectedQueuedWorkDrainError::Refused {
                    cause: SelectedQueuedWorkDrainRefusalCause::
                        InterruptedBatchRequiresFullComposition {
                            required_batch_ids: required_batch_ids
                                .into_iter()
                                .map(crate::BatchId::new)
                                .collect(),
                        },
                });
            }
            Err(crate::StoreError::QueuedWorkRowExceedsContextWindow {
                batch_id,
                batch_enqueue_seq,
                rendered_tokens,
                max_context_tokens,
            }) => {
                // The irreducible residue of FIG-1313: no drain policy, host or
                // shipped, can fit this row. Name it and the window it needs
                // instead of wedging the queue silently.
                session_execution_lease
                    .release_if_live()
                    .await
                    .map_err(|err| {
                        RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string())
                    })?;
                if selected_batch_ids.is_none() {
                    // An automatic drain names no batch ids, so a selected-drain
                    // refusal has nothing to refuse. The fault is deterministic
                    // and terminal: name it as an error the host can classify
                    // instead of letting it reach the empty-reason contract.
                    return Err(SelectedQueuedWorkDrainError::Runtime(RuntimeError::new(
                        RuntimeErrorCode::QueuedWorkRowExceedsContextWindow,
                        format!(
                            "queued row `{batch_id}` (enqueue_seq {batch_enqueue_seq}) renders \
                             {rendered_tokens} tokens, larger than the whole \
                             {max_context_tokens}-token context window"
                        ),
                    )));
                }
                return Err(SelectedQueuedWorkDrainError::Refused {
                    cause: SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow {
                        batch_id: batch_id.clone(),
                        batch_enqueue_seq,
                        required_context_tokens: rendered_tokens,
                        max_context_tokens,
                    },
                });
            }
            other => other.map_err(super::runtime_error_from_store_commit)?,
        };
        let (claim_outcome, claim_refusal) = claim_outcome;
        let already_satisfied_batch_ids = claim_outcome.already_satisfied_batch_ids;
        let Some(claim) = claim_outcome.claim else {
            session_execution_lease
                .release_if_live()
                .await
                .map_err(|err| {
                    RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string())
                })?;
            return if let Some(batch_ids) = selected_batch_ids {
                let already_satisfied = already_satisfied_batch_ids
                    .iter()
                    .map(crate::BatchId::as_str)
                    .collect::<std::collections::BTreeSet<_>>();
                let unclaimed_batch_ids = batch_ids
                    .iter()
                    .filter(|batch_id| !already_satisfied.contains(batch_id.as_str()))
                    .cloned()
                    .collect::<Vec<_>>();
                if unclaimed_batch_ids.is_empty() {
                    Ok(QueuedWorkDrainResult::Selected(
                        SelectedQueuedWorkDrainOutcome::new(
                            None,
                            batch_ids
                                .iter()
                                .cloned()
                                .map(|batch_id| {
                                    SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied {
                                        batch_id,
                                    }
                                })
                                .collect(),
                        ),
                    ))
                } else {
                    Err(SelectedQueuedWorkDrainError::Refused {
                        cause: SelectedQueuedWorkDrainRefusalCause::UnclaimableTogether {
                            unclaimed_batch_ids,
                        },
                    })
                }
            } else {
                Ok(QueuedWorkDrainResult::Automatic(QueuedTurnDrain::Empty(
                    EmptyQueuedDrainReason::ClaimRefused(claim_refusal.expect(
                        "an automatic claim that acquired no rows always names its refusal",
                    )),
                )))
            };
        };
        let mut selected_satisfaction = Vec::new();
        if let Some(batch_ids) = selected_batch_ids {
            let claimed_ids = claim
                .batches
                .iter()
                .map(|batch| batch.batch_id.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            let unclaimed_batch_ids = batch_ids
                .iter()
                .filter(|batch_id| {
                    !claimed_ids.contains(batch_id.as_str())
                        && !already_satisfied_batch_ids.contains(batch_id)
                })
                .cloned()
                .collect::<Vec<_>>();
            if !unclaimed_batch_ids.is_empty() {
                store
                    .abandon_queued_work_claim(&claim)
                    .await
                    .map_err(super::runtime_error_from_store_commit)?;
                session_execution_lease
                    .release_if_live()
                    .await
                    .map_err(|err| {
                        RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string())
                    })?;
                return Err(SelectedQueuedWorkDrainError::Refused {
                    cause: SelectedQueuedWorkDrainRefusalCause::UnclaimableTogether {
                        unclaimed_batch_ids,
                    },
                });
            }
            selected_satisfaction = batch_ids
                .iter()
                .map(|batch_id| {
                    if claimed_ids.contains(batch_id.as_str()) {
                        SelectedQueuedWorkBatchSatisfaction::ClaimedNow {
                            batch_id: batch_id.clone(),
                        }
                    } else {
                        SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied {
                            batch_id: batch_id.clone(),
                        }
                    }
                })
                .collect();
        }
        let mut work = claim.materialize_queued_turn_work();
        if selected_batch_ids.is_some() {
            // A host-selected drain is closed over the rendered batch set. Without this guard,
            // an EarliestSafeBoundary checkpoint in the selected turn could pull unrelated
            // pending batches into the same run after the exact initial claim.
            work.input.turn_context.mark_selected_queued_work_drain();
        }
        if let Some(hint) = opts.local_cancel_origin_hint() {
            work.input.turn_context.set_local_cancel_origin_hint(hint);
        }
        let turn_id = work
            .input
            .trace_turn_id
            .clone()
            .unwrap_or_else(|| TurnId::from(opts.execution_scope_id()));
        work.input.trace_turn_id = Some(turn_id.clone());
        let causes = work.turn_causes.clone();
        crate::trace::emit_trace(
            &self.host.core.tracing.trace_sink,
            &self.host.core.tracing.trace_context,
            lash_trace::TraceContext::default()
                .for_session(self.state.session_id.clone())
                // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                .for_turn_index(self.state.turn_index + 1)
                .for_turn(turn_id.clone()),
            lash_trace::TraceEvent::Custom {
                name: "queued_work.claimed".to_string(),
                payload: queued_work_trace_payload(
                    crate::QueuedWorkClaimBoundary::Idle,
                    &claim,
                    &causes,
                ),
            },
            self.host.core.clock.as_ref(),
        );
        let claim_for_abandon = claim.clone();
        let scoped_effect_controller = opts.scoped_effect_controller();
        let mut session_execution_lease = Some(session_execution_lease);
        let result = Box::pin(self.drive_logical_turn(
            LogicalTurnStart::Input(work.input),
            opts.events_or_noop(),
            opts.turn_events_or_noop(),
            scoped_effect_controller,
            None,
            cancel,
            LogicalTurnClaims::new(vec![claim], Vec::new()),
            &mut session_execution_lease,
            stopwatch,
        ))
        .await
        .map(AgentFrameRun::into_final_turn);
        if let Err(err) = &result {
            self.abandon_queued_work_claims_after_local_abort(
                err,
                std::slice::from_ref(&claim_for_abandon),
            )
            .await;
        }
        self.settle_session_execution_lease(session_execution_lease.as_ref(), result)
            .await
            .map(|turn| {
                if selected_batch_ids.is_some() {
                    QueuedWorkDrainResult::Selected(SelectedQueuedWorkDrainOutcome::new(
                        turn,
                        selected_satisfaction,
                    ))
                } else {
                    QueuedWorkDrainResult::Automatic(QueuedTurnDrain::Ran(
                        turn.expect("logical turn always contains a terminal physical turn"),
                    ))
                }
            })
            .map_err(Into::into)
    }

    async fn session_commands_precede_pending_turn_input(
        &self,
        store: &dyn crate::RuntimePersistence,
    ) -> Result<bool, RuntimeError> {
        let ordering = store
            .pending_session_work_ordering(&self.state.session_id)
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        Ok(ordering.session_command_precedes_turn_input())
    }
}
