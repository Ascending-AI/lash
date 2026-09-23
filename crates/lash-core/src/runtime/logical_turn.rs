use super::turn_loop::{
    LogicalTurnErrorContext, PreparedTurnExecuteContext, SessionExecutionLeaseReleasePolicy,
    TurnLeaseScope, TurnPrepareContext, TurnSinks, TurnStopwatch,
};
use super::*;
use crate::TurnId;

pub const MAX_AGENT_FRAME_SWITCHES: usize = 16;

/// How many follow-on physical turns one logical run may start from work
/// claimed at a terminal checkpoint (FIG-3157), so a wake storm cannot run a
/// logical turn forever.
pub const MAX_TERMINAL_CHECKPOINT_FOLLOW_ONS: usize = 16;

/// Work claimed at a terminal checkpoint and withheld from that checkpoint's
/// delivery.
///
/// FIG-3157: a terminal finish ends the turn. The committed finish is the
/// turn's answer, so a delivery claimed at `BeforeCompletion` never extends it
/// — it starts a follow-on physical turn inside the same logical run, carrying
/// the claimed work as that turn's input.
#[derive(Default)]
pub(in crate::runtime) struct WithheldTerminalWork {
    pub(in crate::runtime) queued: Vec<crate::QueuedWorkClaim>,
    pub(in crate::runtime) turn_inputs: Vec<crate::TurnInputClaim>,
}

impl WithheldTerminalWork {
    pub(in crate::runtime) fn is_empty(&self) -> bool {
        self.queued.is_empty() && self.turn_inputs.is_empty()
    }

    pub(in crate::runtime) fn take_if_any(&mut self) -> Option<Self> {
        (!self.is_empty()).then(|| std::mem::take(self))
    }
}

pub(super) struct PhysicalTurnExecution {
    pub(super) turn: AssembledTurn,
    pub(super) enqueued_queue_batches: Vec<crate::QueuedWorkBatch>,
    pub(super) post_commit_delivery_failed: bool,
    /// Claimed at this turn's terminal checkpoint and withheld from it, for
    /// the logical run to start a follow-on turn with.
    pub(super) withheld_terminal_work: Option<WithheldTerminalWork>,
}

pub(super) struct LogicalTurnClaims {
    pub(super) queued: Vec<crate::QueuedWorkClaim>,
    /// The turn-input rows this turn drives, each with the authority it will
    /// settle under: a generation-fenced claim, or none at all when the turn
    /// accepted the row itself and settles it at the head CAS (ADR 0069 §5).
    pub(super) turn_inputs: Vec<crate::TurnInputClaim>,
    /// Work this turn claimed at its terminal checkpoint and withheld from the
    /// delivery. It is never settled as this turn's completed work: it is the
    /// follow-on turn's input, and holding it keeps the session execution
    /// lease live across the commit that ends this turn. A cancelled turn
    /// starts no follow-on for withheld turn input; its commit settles that
    /// input through the undelivered disposition instead (FIG-3531).
    pub(super) withheld_terminal_work: Option<WithheldTerminalWork>,
    /// Withheld turn input a turn that aborted on a cancel hands straight to
    /// the undelivered disposition, whatever outcome the commit assembles
    /// (FIG-3531).
    pub(super) undelivered_turn_inputs: Vec<crate::TurnInputClaim>,
}

impl LogicalTurnClaims {
    pub(super) fn new(
        queued: Vec<crate::QueuedWorkClaim>,
        turn_inputs: Vec<crate::TurnInputClaim>,
    ) -> Self {
        Self {
            queued,
            turn_inputs,
            withheld_terminal_work: None,
            undelivered_turn_inputs: Vec::new(),
        }
    }

    pub(super) fn with_undelivered_turn_inputs(
        mut self,
        undelivered: Vec<crate::TurnInputClaim>,
    ) -> Self {
        self.undelivered_turn_inputs = undelivered;
        self
    }

    pub(super) fn with_withheld_terminal_work(
        mut self,
        withheld: Option<WithheldTerminalWork>,
    ) -> Self {
        self.withheld_terminal_work = withheld;
        self
    }

    pub(super) fn is_empty(&self) -> bool {
        self.queued.is_empty() && self.turn_inputs.is_empty()
    }

    /// Whether this turn leaves withheld work for a follow-on turn, given
    /// whether it committed as cancelled. A cancelled turn settles withheld
    /// turn input through the undelivered disposition instead of carrying it
    /// (FIG-3531); withheld queued work is carried either way.
    pub(super) fn carries_follow_on_work(&self, cancelled: bool) -> bool {
        self.withheld_terminal_work
            .as_ref()
            .is_some_and(|withheld| {
                !withheld.queued.is_empty() || (!cancelled && !withheld.turn_inputs.is_empty())
            })
    }

    /// The withheld work the logical run drives in a follow-on turn once this
    /// turn has committed. See [`Self::carries_follow_on_work`].
    pub(super) fn take_follow_on_work(&mut self, cancelled: bool) -> Option<WithheldTerminalWork> {
        let mut withheld = self.withheld_terminal_work.take()?;
        if cancelled {
            withheld.turn_inputs.clear();
        }
        withheld.take_if_any()
    }

    pub(super) fn commit_effects(
        &self,
        outcome: &TurnOutcome,
        session_id: &SessionId,
        turn_id: &TurnId,
        protocol_turn_options: Option<crate::ProtocolTurnOptions>,
    ) -> LogicalTurnCommitEffects {
        let claimed = !self.is_empty();
        let completed_queue_claims: Vec<_> =
            self.queued.iter().map(|claim| claim.completion()).collect();
        let completed_turn_input_claims: Vec<_> = self
            .turn_inputs
            .iter()
            .map(|claim| claim.completion())
            .collect();
        let queue_claim_generations = self
            .queued
            .iter()
            .map(|claim| (claim.claim_id.clone(), claim.session_lease_generation))
            .collect();
        let turn_input_claim_generations = self
            .turn_inputs
            .iter()
            .map(|claim| (claim.claim_id.clone(), claim.session_lease_generation))
            .collect();
        let enqueued_queue_batches = match outcome {
            TurnOutcome::AgentFrameSwitch {
                frame_key, task, ..
            } if claimed => {
                vec![
                    crate::QueuedWorkBatchDraft::new(
                        session_id,
                        crate::DeliveryPolicy::AfterCurrentTurnCommit,
                        crate::TurnWorkPayload::agent_frame_task(
                            crate::session_graph::frame_node_id(session_id, frame_key.as_str()),
                            task.clone(),
                            protocol_turn_options,
                        ),
                    )
                    .with_source_key(format!("agent-frame-handoff:{turn_id}")),
                ]
            }
            _ => Vec::new(),
        };
        // A cancelled turn never delivers the input it withheld from its
        // terminal checkpoint: it starts no follow-on for it (FIG-3531).
        let cancelled = matches!(outcome, TurnOutcome::Stopped(TurnStop::Cancelled { .. }));
        let withheld_turn_inputs = self
            .withheld_terminal_work
            .iter()
            .filter(|_| cancelled)
            .flat_map(|withheld| &withheld.turn_inputs);
        let undelivered_turn_inputs = self
            .undelivered_turn_inputs
            .iter()
            .chain(withheld_turn_inputs)
            .cloned()
            .collect();
        LogicalTurnCommitEffects {
            claim_settlement: TurnClaimSettlement::new(
                completed_queue_claims,
                completed_turn_input_claims,
                queue_claim_generations,
                turn_input_claim_generations,
            )
            .with_undelivered_turn_inputs(undelivered_turn_inputs),
            enqueued_queue_batches,
        }
    }
}

pub(super) struct LogicalTurnCommitEffects {
    pub(super) claim_settlement: TurnClaimSettlement,
    pub(super) enqueued_queue_batches: Vec<crate::QueuedWorkBatchDraft>,
}

pub(super) struct PreparedLogicalTurn {
    pub(super) messages: crate::MessageSequence,
    pub(super) previous_prompt_usage: Option<TokenUsage>,
    pub(super) protocol_turn_options: Option<crate::ProtocolTurnOptions>,
    pub(super) protocol_extension: Option<crate::ProtocolTurnExtensionHandle>,
    pub(super) turn_context: crate::TurnContext,
    pub(super) initial_turn_causes: Vec<crate::TurnCause>,
    pub(super) trace_turn_id: TurnId,
    pub(super) turn_index: usize,
}

pub(super) enum LogicalTurnStart {
    Input(TurnInput),
    Prepared(PreparedLogicalTurn),
}

impl LogicalTurnStart {
    fn continuation_state(
        &self,
    ) -> (
        Option<crate::ProtocolTurnOptions>,
        crate::TurnContext,
        TurnId,
    ) {
        match self {
            Self::Input(input) => (
                input.protocol_turn_options.clone(),
                input.turn_context.clone(),
                input
                    .trace_turn_id
                    .clone()
                    .unwrap_or_else(|| TurnId::from("")),
            ),
            Self::Prepared(prepared) => (
                prepared.protocol_turn_options.clone(),
                prepared.turn_context.clone(),
                prepared.trace_turn_id.clone(),
            ),
        }
    }
}

impl LashRuntime {
    async fn emit_physical_turn_start(
        turn_events: &dyn TurnActivitySink,
        turn_id: &TurnId,
        claims: &LogicalTurnClaims,
        announce_queued_work: bool,
    ) {
        super::turn_loop::emit_turn_started_to_sink(turn_events, turn_id).await;
        if !announce_queued_work {
            // Work withheld from a terminal checkpoint already announced its
            // start at the boundary that claimed it (FIG-3157).
            return;
        }
        for claim in &claims.queued {
            let work = claim.materialize_queued_turn_work();
            super::turn_loop::emit_queued_work_started_to_sink(
                turn_events,
                turn_id,
                crate::QueuedWorkClaimBoundary::Idle,
                claim,
                work.turn_causes,
            )
            .await;
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "a follow-on failure follows a committed turn"
    )]
    fn record_follow_on_failure(&mut self, turns: &mut [AssembledTurn], err: RuntimeError) {
        self.invalidate_resident_session_state();
        turns
            .last_mut()
            .expect("a follow-on failure requires an earlier committed turn")
            .errors
            .push(super::turn_loop::post_commit_delivery_issue(
                crate::FailureCode::from(&err.code),
                err.message,
            ));
    }

    /// Hand back work claimed at a terminal checkpoint that this logical run
    /// will not drive after all. The rows return to the queue exactly as they
    /// were, for the next drain to claim.
    async fn abandon_withheld_terminal_work(&self, withheld: WithheldTerminalWork) {
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return;
        };
        if !withheld.queued.is_empty()
            && let Err(err) = store.abandon_queued_work_claims(&withheld.queued).await
        {
            tracing::warn!(
                error = %err,
                claim_count = withheld.queued.len(),
                "failed to abandon queued work withheld from a terminal checkpoint"
            );
        }
        let turn_input_claims = &withheld.turn_inputs;
        if !turn_input_claims.is_empty()
            && let Err(err) = store.abandon_turn_input_claims(turn_input_claims).await
        {
            tracing::warn!(
                error = %err,
                claim_count = turn_input_claims.len(),
                "failed to abandon turn input claimed at a terminal checkpoint"
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn drive_logical_turn(
        &mut self,
        mut start: LogicalTurnStart,
        events: &dyn EventSink,
        turn_events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
        cancel: CancellationToken,
        mut claims: LogicalTurnClaims,
        session_execution_lease: &mut Option<SessionExecutionLeaseGuard>,
        stopwatch: TurnStopwatch,
    ) -> Result<AgentFrameRun, RuntimeError> {
        // FIG-3353: the shared funnel for every logical turn — an open that
        // declared it would not run one is refused before any effect.
        self.refuse_turn_execution_on_preserved_tool_surface()?;
        let (follow_protocol_turn_options, follow_turn_context, supplied_trace_turn_id) =
            start.continuation_state();
        if !supplied_trace_turn_id.is_empty()
            && scoped_effect_controller
                .execution_scope()
                .validates_turn_trace_id()
            && supplied_trace_turn_id.as_str() != scoped_effect_controller.scope_id()
        {
            return Err(RuntimeError::new(
                RuntimeErrorCode::ExecutionScopeTurnIdMismatch,
                format!(
                    "input trace_turn_id `{supplied_trace_turn_id}` does not match execution scope id `{}`",
                    scoped_effect_controller.scope_id()
                ),
            ));
        }
        let root_trace_turn_id = if let Some(run) = &self.queued_run {
            TurnId::from(run.scope.id())
        } else if supplied_trace_turn_id.is_empty() {
            TurnId::from(scoped_effect_controller.scope_id())
        } else {
            supplied_trace_turn_id
        };
        let mut turns: Vec<AssembledTurn> = Vec::new();
        // FIG-3157: work claimed at a terminal checkpoint, withheld from the
        // delivery so the committed finish stayed the turn's answer, waiting
        // for the follow-on turn that drives it.
        let mut carried_withheld: Option<WithheldTerminalWork> = None;
        let mut announce_queued_work = true;
        let mut follow_on_turns = 0usize;

        loop {
            let turn_trace_turn_id = self
                .queued_run
                .as_ref()
                .map(|run| run.position.turn_id.clone())
                .unwrap_or_else(|| agent_frame_follow_turn_id(&root_trace_turn_id, turns.len()));
            // A frame switch creates a new physical turn identity, but it does
            // not create new effect authority. Every frame in this admitted
            // run therefore keeps the controller's exact execution scope.
            let turn_effect_controller = scoped_effect_controller.clone();
            let teardown_effect_controller = turn_effect_controller.clone();
            let frame_stopwatch = if turns.is_empty() {
                stopwatch
            } else {
                TurnStopwatch::start(self.host.core.clock.as_ref())
            };
            Self::emit_physical_turn_start(
                turn_events,
                &turn_trace_turn_id,
                &claims,
                announce_queued_work,
            )
            .await;
            announce_queued_work = true;
            let at_queued_frame_limit = self.queued_run.as_ref().is_some_and(|run| {
                run.position.physical_ordinal >= MAX_AGENT_FRAME_SWITCHES as u64
                    && run.last_commit.as_ref().is_some_and(|commit| {
                        matches!(
                            commit.progress,
                            crate::store::QueuedRunProgress::Advance {
                                include_outbox: true,
                                ..
                            }
                        )
                    })
            });
            if at_queued_frame_limit {
                let terminal = Box::pin(self.finish_logical_turn_error(LogicalTurnErrorContext {
                    message: format!("logical turn exceeded the limit of {MAX_AGENT_FRAME_SWITCHES} agent frame switches"),
                    trace_turn_id: turn_trace_turn_id,
                    sinks: TurnSinks { events, turn_events },
                    scoped_effect_controller: turn_effect_controller,
                    cancel: cancel.clone(),
                    claims,
                    session_execution_lease: session_execution_lease.as_ref(),
                })).await;
                let mut terminal = match terminal {
                    Ok(terminal) => terminal,
                    Err(error) => {
                        self.invalidate_resident_session_state();
                        return Err(error);
                    }
                };
                frame_stopwatch.stamp(&mut terminal.turn, self.host.core.clock.as_ref());
                turns.push(terminal.turn);
                if let Some(store) = self
                    .session
                    .as_ref()
                    .and_then(|session| session.history_store())
                    && store
                        .pending_queued_run(&self.state.session_id)
                        .await
                        .map_err(super::runtime_error_from_store_commit)?
                        .is_some()
                {
                    return Err(RuntimeError::new(
                        RuntimeErrorCode::QueuedRunPending,
                        "frame-switch limit committed; withheld queued work awaits the next drain",
                    ));
                }
                self.queued_run = None;
                return Ok(AgentFrameRun {
                    turns,
                    acceptance: None,
                });
            }
            let execution_result = match start {
                LogicalTurnStart::Input(mut input) => {
                    input.trace_turn_id = Some(turn_trace_turn_id.clone());
                    Box::pin(self.stream_turn_with_scoped_effect_controller_inner(
                        TurnPrepareContext {
                            input,
                            sinks: TurnSinks {
                                events,
                                turn_events,
                            },
                            scoped_effect_controller: turn_effect_controller,
                            cancel: cancel.clone(),
                            queued_claims: claims.queued,
                            turn_input_claims: claims.turn_inputs,
                            materialize_initial_claims: true,
                            lease: TurnLeaseScope {
                                guard: session_execution_lease.as_ref(),
                                release_policy:
                                    SessionExecutionLeaseReleasePolicy::KeepOnAgentFrameSwitch,
                            },
                        },
                    ))
                    .await
                }
                LogicalTurnStart::Prepared(mut prepared) => {
                    prepared.trace_turn_id = turn_trace_turn_id.clone();
                    // Host-prepared turns enter the physical stream directly,
                    // bypassing the Input branch's owner-binding wrapper.
                    // Keep this guard on the logical-turn caller's stack so all
                    // puts are attributed before final-commit stamping.
                    let _attachment_owner_binding = self
                        .host
                        .core
                        .durability
                        .attachment_store
                        .bind_turn_scoped(prepared.trace_turn_id.clone());
                    Box::pin(self.stream_prepared_turn_inner(PreparedTurnExecuteContext {
                        turn: prepared,
                        sinks: TurnSinks {
                            events,
                            turn_events,
                        },
                        scoped_effect_controller: turn_effect_controller,
                        cancel: cancel.clone(),
                        initial_queue_claims: claims.queued,
                        initial_turn_input_claims: claims.turn_inputs,
                        lease: TurnLeaseScope {
                            guard: session_execution_lease.as_ref(),
                            release_policy:
                                SessionExecutionLeaseReleasePolicy::KeepOnAgentFrameSwitch,
                        },
                    }))
                    .await
                }
            };
            let execution = match execution_result {
                Ok(execution) => execution,
                Err(err) if self.queued_run.is_some() => {
                    self.invalidate_resident_session_state();
                    return Err(err);
                }
                // FIG-1573: this frame ended without reaching a commit, so the
                // commit-time re-defer never ran. Inputs routed into it while it
                // was live are pinned to a turn id no later turn will ever carry
                // again, so the teardown owes them the same repair. Both ids are
                // dead by construction here, which keeps the live-turn hazard out.
                //
                // The rejected turn may have mutated the live execution before
                // it failed (an after-turn hook refusing finalization runs after
                // the executor already applied the turn), so the resident state
                // is invalidated exactly like a rejected follow-on turn's: the
                // next use reloads from the accepted snapshot instead of running
                // the executor this turn dirtied.
                Err(err) if turns.is_empty() => {
                    self.defer_orphaned_turn_inputs_after_teardown(
                        &turn_trace_turn_id,
                        session_execution_lease
                            .as_ref()
                            .map(|lease| lease.fence())
                            .as_ref(),
                        &teardown_effect_controller,
                    )
                    .await;
                    self.invalidate_resident_session_state();
                    if let Some(withheld) = carried_withheld.take() {
                        self.abandon_withheld_terminal_work(withheld).await;
                    }
                    return Err(err);
                }
                Err(err) => {
                    self.defer_orphaned_turn_inputs_after_teardown(
                        &turn_trace_turn_id,
                        session_execution_lease
                            .as_ref()
                            .map(|lease| lease.fence())
                            .as_ref(),
                        &teardown_effect_controller,
                    )
                    .await;
                    self.record_follow_on_failure(&mut turns, err);
                    if let Some(withheld) = carried_withheld.take() {
                        self.abandon_withheld_terminal_work(withheld).await;
                    }
                    return Ok(AgentFrameRun {
                        turns,
                        acceptance: None,
                    });
                }
            };
            let PhysicalTurnExecution {
                mut turn,
                enqueued_queue_batches,
                post_commit_delivery_failed,
                withheld_terminal_work,
            } = execution;
            if let Some(withheld) = withheld_terminal_work {
                let carried = carried_withheld.get_or_insert_with(WithheldTerminalWork::default);
                carried.queued.extend(withheld.queued);
                carried.turn_inputs.extend(withheld.turn_inputs);
            }
            frame_stopwatch.stamp(&mut turn, self.host.core.clock.as_ref());
            let switched_frame = match &turn.outcome {
                TurnOutcome::AgentFrameSwitch {
                    frame_key, task, ..
                } => Some((frame_key.clone(), task.clone())),
                _ => None,
            };
            turns.push(turn);
            if self.queued_run.is_some() {
                let store = self
                    .session
                    .as_ref()
                    .and_then(|session| session.history_store())
                    .ok_or_else(|| {
                        RuntimeError::new(
                            RuntimeErrorCode::QueuedRunPending,
                            "queued continuation requires persistence",
                        )
                    })?;
                let pending = store
                    .pending_queued_run(&self.state.session_id)
                    .await
                    .map_err(super::runtime_error_from_store_commit)?;
                let Some(pending) = pending else {
                    self.queued_run = None;
                    return Ok(AgentFrameRun {
                        turns,
                        acceptance: None,
                    });
                };
                self.queued_run = Some(Box::new(pending.clone()));
                let lease = session_execution_lease
                    .as_ref()
                    .filter(|lease| !lease.is_lost())
                    .ok_or_else(|| {
                        RuntimeError::new(
                            RuntimeErrorCode::QueuedRunPending,
                            "queued continuation awaits a live execution lane",
                        )
                    })?;
                let frame_limit_due = pending.position.physical_ordinal
                    >= MAX_AGENT_FRAME_SWITCHES as u64
                    && matches!(
                        pending.last_commit.as_ref().map(|commit| &commit.progress),
                        Some(crate::store::QueuedRunProgress::Advance {
                            include_outbox: true,
                            ..
                        })
                    );
                if post_commit_delivery_failed
                    || (turns.len() >= MAX_TERMINAL_CHECKPOINT_FOLLOW_ONS && !frame_limit_due)
                {
                    return Err(RuntimeError::new(
                        RuntimeErrorCode::QueuedRunPending,
                        "committed queued continuation awaits the next drain",
                    ));
                }
                let selection = store
                    .select_queued_run(
                        &lease.fence(),
                        &pending.scope,
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
                    .map_err(super::runtime_error_from_store_commit)?;
                announce_queued_work = matches!(
                    pending.last_commit.as_ref().map(|commit| &commit.progress),
                    Some(crate::store::QueuedRunProgress::Advance {
                        include_outbox: true,
                        ..
                    })
                );
                let (mut input, next_claims) =
                    self.queued_run_input(selection, announce_queued_work)?;
                input.protocol_turn_options = follow_protocol_turn_options.clone();
                input.turn_context = follow_turn_context.clone();
                claims = next_claims;
                start = LogicalTurnStart::Input(input);
                carried_withheld = None;
                continue;
            }
            if post_commit_delivery_failed {
                if let Some(withheld) = carried_withheld.take() {
                    self.abandon_withheld_terminal_work(withheld).await;
                }
                return Ok(AgentFrameRun {
                    turns,
                    acceptance: None,
                });
            }
            let Some((frame_key, task)) = switched_frame else {
                // FIG-3157: the turn ended on its committed answer. Work it
                // claimed at the terminal checkpoint starts the next turn now
                // — no idle gap, no wait for the user, the same session
                // execution lease and generation throughout.
                let Some(withheld) = carried_withheld.take() else {
                    return Ok(AgentFrameRun {
                        turns,
                        acceptance: None,
                    });
                };
                // The claim is only generation-valid while the lease that
                // fenced it is live (ADR 0029). A run whose lane lapsed hands
                // the rows back instead, for a successor to reclaim.
                let lane_live = session_execution_lease
                    .as_ref()
                    .is_some_and(|guard| !guard.is_lost());
                if !lane_live {
                    self.abandon_withheld_terminal_work(withheld).await;
                    return Ok(AgentFrameRun {
                        turns,
                        acceptance: None,
                    });
                }
                if follow_on_turns >= MAX_TERMINAL_CHECKPOINT_FOLLOW_ONS {
                    // Bounded like an agent-frame chain: hand the rows back so
                    // a later drain takes them instead of running forever.
                    self.abandon_withheld_terminal_work(withheld).await;
                    return Ok(AgentFrameRun {
                        turns,
                        acceptance: None,
                    });
                }
                follow_on_turns += 1;
                let mut input = TurnInput::items(Vec::new());
                input.protocol_turn_options = follow_protocol_turn_options.clone();
                input.turn_context = follow_turn_context.clone();
                claims = LogicalTurnClaims::new(withheld.queued, withheld.turn_inputs);
                announce_queued_work = false;
                start = LogicalTurnStart::Input(input);
                continue;
            };

            let next = async {
                if enqueued_queue_batches.is_empty() {
                    let mut input = turn_input_from_text(task);
                    input.protocol_turn_options = follow_protocol_turn_options.clone();
                    input.turn_context = follow_turn_context.clone();
                    return Ok((input, LogicalTurnClaims::new(Vec::new(), Vec::new())));
                }
                let lease = session_execution_lease.as_ref().ok_or_else(|| {
                    RuntimeError::new(
                        RuntimeErrorCode::StoreCommitFailed,
                        "claimed agent-frame handoff requires a session execution lease",
                    )
                })?;
                let store = self
                    .session
                    .as_ref()
                    .and_then(|session| session.history_store())
                    .ok_or_else(|| {
                        RuntimeError::new(
                            RuntimeErrorCode::StoreCommitFailed,
                            "claimed agent-frame handoff requires a runtime persistence store",
                        )
                    })?;
                let batch_ids = enqueued_queue_batches
                    .iter()
                    .map(|batch| batch.batch_id.clone())
                    .collect::<Vec<_>>();
                let claim_policy = self
                    .host
                    .core
                    .durability
                    .queued_work_batching
                    .claim_policy(self.max_context_tokens());
                let claim = store
                    .claim_ready_queued_work_by_batch_ids(
                        &self.state.session_id,
                        &lease.fence(),
                        &self.runtime_lease_owner,
                        crate::QueuedWorkClaimBoundary::Idle,
                        &batch_ids,
                        claim_policy,
                    )
                    .await
                    .map_err(super::runtime_error_from_store_commit)?
                    .ok_or_else(|| {
                        RuntimeError::new(
                            RuntimeErrorCode::StoreCommitFailed,
                            format!(
                                "failed to claim committed agent-frame handoff batch `{}`",
                                batch_ids.join(",")
                            ),
                        )
                    })?;
                let target_matches = claim.batches.iter().all(|batch| {
                    batch.items.iter().all(|item| {
                        matches!(
                            &item.payload,
                            crate::QueuedWorkPayload::AgentFrameTask {
                                frame_id: target,
                                ..
                            } if Some(target.as_str())
                                == self.state.current_frame_node_id.as_deref()
                        )
                    })
                });
                if !target_matches {
                    return Err(RuntimeError::new(
                        RuntimeErrorCode::StoreCommitFailed,
                        format!(
                            "agent-frame handoff did not target frame node id derived from frame key `{}`",
                            frame_key.as_str()
                        ),
                    ));
                }
                let materialized = claim.materialize_queued_turn_work();
                let follow_turn_id = agent_frame_follow_turn_id(&root_trace_turn_id, turns.len());
                crate::trace::emit_trace(
                    &self.host.core.tracing.trace_sink,
                    &self.host.core.tracing.trace_context,
                    lash_trace::TraceContext::default()
                        .for_session(self.state.session_id.clone())
                        // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                        .for_turn_index(self.state.turn_index + 1)
                        .for_turn(follow_turn_id),
                    lash_trace::TraceEvent::Custom {
                        name: "queued_work.claimed".to_string(),
                        payload: super::turn_loop::queued_work_trace_payload(
                            crate::QueuedWorkClaimBoundary::Idle,
                            &claim,
                            &materialized.turn_causes,
                        ),
                    },
                    self.host.core.clock.as_ref(),
                );
                Ok((
                    materialized.input,
                    LogicalTurnClaims::new(vec![claim], Vec::new()),
                ))
            }
            .await;
            let (mut input, next_claims) = match next {
                Ok(next) => next,
                Err(err) => {
                    self.record_follow_on_failure(&mut turns, err);
                    if let Some(withheld) = carried_withheld.take() {
                        self.abandon_withheld_terminal_work(withheld).await;
                    }
                    return Ok(AgentFrameRun {
                        turns,
                        acceptance: None,
                    });
                }
            };
            input.protocol_turn_options = follow_protocol_turn_options.clone();
            input.turn_context = follow_turn_context.clone();

            if turns.len() >= MAX_AGENT_FRAME_SWITCHES {
                let terminal_trace_turn_id =
                    agent_frame_follow_turn_id(&root_trace_turn_id, turns.len());
                let terminal_effect_controller = scoped_effect_controller.clone();
                let terminal_stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
                Self::emit_physical_turn_start(
                    turn_events,
                    &terminal_trace_turn_id,
                    &next_claims,
                    true,
                )
                .await;
                let terminal_result = Box::pin(self.finish_logical_turn_error(
                        LogicalTurnErrorContext {
                            message: format!(
                                "logical turn exceeded the limit of {MAX_AGENT_FRAME_SWITCHES} agent frame switches"
                            ),
                            trace_turn_id: terminal_trace_turn_id,
                            sinks: TurnSinks {
                                events,
                                turn_events,
                            },
                            scoped_effect_controller: terminal_effect_controller,
                            cancel: cancel.clone(),
                            claims: next_claims,
                            session_execution_lease: session_execution_lease.as_ref(),
                        },
                    ))
                    .await;
                if let Some(withheld) = carried_withheld.take() {
                    self.abandon_withheld_terminal_work(withheld).await;
                }
                let mut terminal = match terminal_result {
                    Ok(terminal) => terminal,
                    Err(err) => {
                        self.record_follow_on_failure(&mut turns, err);
                        return Ok(AgentFrameRun {
                            turns,
                            acceptance: None,
                        });
                    }
                };
                terminal_stopwatch.stamp(&mut terminal.turn, self.host.core.clock.as_ref());
                turns.push(terminal.turn);
                return Ok(AgentFrameRun {
                    turns,
                    acceptance: None,
                });
            }

            claims = next_claims;
            start = LogicalTurnStart::Input(input);
        }
    }
}

pub(super) fn turn_input_from_text(text: String) -> TurnInput {
    TurnInput::text(text)
}

pub(super) fn agent_frame_follow_turn_id(
    root_turn_id: &TurnId,
    completed_turn_count: usize,
) -> TurnId {
    crate::store::QueuedRunPosition::derive_turn_id(root_turn_id, completed_turn_count as u64)
}
