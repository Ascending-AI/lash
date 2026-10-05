use super::turn_loop::{LogicalTurnErrorContext, TurnPrepareContext, TurnSinks, TurnStopwatch};
use super::*;
use crate::TurnId;

pub const MAX_AGENT_FRAME_SWITCHES: usize = 16;

/// How many follow-on physical turns one logical run may start from work
/// admitted at a terminal checkpoint (FIG-3157), so a wake storm cannot run a
/// logical turn forever.
pub const MAX_TERMINAL_CHECKPOINT_FOLLOW_ONS: usize = 16;

/// Work admitted at a terminal checkpoint and withheld from that
/// checkpoint's delivery.
///
/// FIG-3157: a terminal finish ends the turn. The committed finish is the
/// turn's answer, so a delivery admitted at `BeforeCompletion` never extends
/// it — it starts a follow-on physical turn inside the same logical run,
/// carrying the admitted work as that turn's input. The rows stay bound to
/// the run throughout (FIG-3927).
#[derive(Clone, Default)]
pub(in crate::runtime) struct WithheldTerminalWork {
    pub(in crate::runtime) queued: Vec<crate::AdmittedQueuedWork>,
    pub(in crate::runtime) turn_inputs: Vec<crate::AdmittedTurnInputs>,
}

impl WithheldTerminalWork {
    pub(in crate::runtime) fn is_empty(&self) -> bool {
        self.queued.is_empty() && self.turn_inputs.is_empty()
    }

    pub(in crate::runtime) fn take_if_any(&mut self) -> Option<Self> {
        (!self.is_empty()).then(|| std::mem::take(self))
    }

    /// Put `earlier` ahead of this work: what an earlier physical turn of
    /// the logical run withheld, which this turn carries on to the run's
    /// follow-on together with its own (FIG-4044).
    pub(in crate::runtime) fn carry_earlier(&mut self, earlier: Option<Self>) {
        let Some(mut earlier) = earlier else {
            return;
        };
        earlier.queued.append(&mut self.queued);
        earlier.turn_inputs.append(&mut self.turn_inputs);
        *self = earlier;
    }
}

pub(super) struct PhysicalTurnExecution {
    pub(super) turn: AssembledTurn,
    pub(super) post_commit_delivery_failed: bool,
    /// Admitted at this turn's terminal checkpoint and withheld from it, for
    /// the logical run to start a follow-on turn with.
    pub(super) withheld_terminal_work: Option<WithheldTerminalWork>,
}

/// The rows one physical turn executes, each admitted to the turn's run
/// (FIG-3927): the run's own admission, and what its checkpoints admitted.
pub(super) struct LogicalTurnAdmissions {
    pub(super) queued: Vec<crate::AdmittedQueuedWork>,
    pub(super) turn_inputs: Vec<crate::AdmittedTurnInputs>,
    /// Work this turn admitted at its terminal checkpoint and withheld from
    /// the delivery. It is never settled as this turn's completed work: it is
    /// the follow-on turn's input. A cancelled turn starts no follow-on: its
    /// commit hands all of it to the cancellation instead — input to the
    /// undelivered disposition (FIG-3531), wakes to be released (FIG-3543,
    /// ADR 0101 §10).
    pub(super) withheld_terminal_work: Option<WithheldTerminalWork>,
    /// Withheld work a turn that aborted on a cancel hands straight to the
    /// cancellation, whatever outcome the commit assembles (FIG-3531,
    /// FIG-3543).
    pub(super) undelivered: WithheldTerminalWork,
    /// Whether the logical run may start another follow-on turn after this
    /// one ([`MAX_TERMINAL_CHECKPOINT_FOLLOW_ONS`]). A turn whose run spent
    /// the bound hands its withheld work back open in its own commit.
    follow_on_allowed: bool,
}

impl LogicalTurnAdmissions {
    pub(super) fn new(
        queued: Vec<crate::AdmittedQueuedWork>,
        turn_inputs: Vec<crate::AdmittedTurnInputs>,
    ) -> Self {
        Self {
            queued,
            turn_inputs,
            withheld_terminal_work: None,
            undelivered: WithheldTerminalWork::default(),
            follow_on_allowed: true,
        }
    }

    pub(super) fn with_undelivered(mut self, undelivered: WithheldTerminalWork) -> Self {
        self.undelivered = undelivered;
        self
    }

    pub(super) fn with_withheld_terminal_work(
        mut self,
        withheld: Option<WithheldTerminalWork>,
    ) -> Self {
        self.withheld_terminal_work = withheld;
        self
    }

    pub(super) fn with_follow_on_allowed(mut self, allowed: bool) -> Self {
        self.follow_on_allowed = allowed;
        self
    }

    pub(super) fn follow_on_allowed(&self) -> bool {
        self.follow_on_allowed
    }

    /// Whether this turn leaves withheld work for a follow-on turn, given
    /// whether it committed as cancelled. A cancelled turn carries nothing:
    /// its commit hands the withheld work to the cancellation instead, and a
    /// run past its follow-on bound hands it back open.
    pub(super) fn carries_follow_on_work(&self, cancelled: bool) -> bool {
        !cancelled
            && self.follow_on_allowed
            && self
                .withheld_terminal_work
                .as_ref()
                .is_some_and(|withheld| !withheld.is_empty())
    }

    /// The withheld work the logical run executes in a follow-on turn once this
    /// turn has committed. See [`Self::carries_follow_on_work`].
    pub(super) fn take_follow_on_work(&mut self, cancelled: bool) -> Option<WithheldTerminalWork> {
        let carries = self.carries_follow_on_work(cancelled);
        let withheld = self.withheld_terminal_work.take()?;
        carries.then_some(withheld)
    }

    /// What this turn's commit settles: every row it drove completes, and
    /// withheld work no follow-on turn executes is handed back.
    pub(super) fn commit_effects(
        &self,
        outcome: &TurnOutcome,
        pending_follow_on: Option<crate::store::PendingFollowOn>,
    ) -> LogicalTurnCommitEffects {
        let cancelled = matches!(outcome, TurnOutcome::Stopped(TurnStop::Cancelled { .. }));
        let mut undelivered = WithheldTerminalWork {
            queued: self.undelivered.queued.clone(),
            turn_inputs: self.undelivered.turn_inputs.clone(),
        };
        if !self.carries_follow_on_work(cancelled)
            && let Some(withheld) = &self.withheld_terminal_work
        {
            undelivered.queued.extend(withheld.queued.iter().cloned());
            undelivered
                .turn_inputs
                .extend(withheld.turn_inputs.iter().cloned());
        }
        LogicalTurnCommitEffects {
            ingress_settlement: TurnIngressSettlement::new(
                self.queued
                    .iter()
                    .map(crate::AdmittedQueuedWork::completion)
                    .collect(),
                self.turn_inputs
                    .iter()
                    .map(crate::AdmittedTurnInputs::completion)
                    .collect(),
            )
            .with_undelivered(undelivered),
            pending_follow_on,
        }
    }
}

pub(super) struct LogicalTurnCommitEffects {
    pub(super) ingress_settlement: TurnIngressSettlement,
    /// The follow-on the head owes once this turn commits (ADR 0101 §3).
    pub(super) pending_follow_on: Option<crate::store::PendingFollowOn>,
}

/// The follow-on the terminal commit of physical turn `turn_id` leaves on the
/// head (ADR 0101 §3).
///
/// A frame switch owes the switched frame one follow-on turn, the next
/// physical turn of the same logical run; its chain depth counts this switch,
/// continuing the depth the turn itself was owed with, and it carries the
/// record the running run resolved, the logical run's recovery bound
/// included: a follow-on re-records its parent's record verbatim, so the
/// whole chain carries the one its run resolved. A segment boundary
/// (FIG-4739) owes the run's continuation the same way, in the frame the
/// turn ran in — `frame`, the current frame of the state the turn assembled —
/// and at the turn's own chain depth, since a boundary switches no frame;
/// `segment_boundary` is what the turn took at it: the protocol iterations
/// the run has spent through the turn, and the code cell the boundary
/// stopped inside, if it stopped inside one. Every other outcome leaves nothing: a turn commits only
/// while the head owes nothing or owes this very turn, and this commit is
/// that follow-on's terminal record.
pub(super) fn follow_on_after_turn(
    state: &RuntimeSessionState,
    outcome: &TurnOutcome,
    turn_id: &TurnId,
    run: &TurnId,
    frame: Option<&crate::FrameNodeId>,
    segment_boundary: Option<&crate::runtime::turn_driver::BoundaryTaken>,
) -> Result<Option<crate::store::PendingFollowOn>, RuntimeError> {
    let owed = state
        .pending_follow_on
        .as_ref()
        .filter(|owed| owed.is_turn(turn_id));
    let (frame_id, work, chain_depth) = match outcome {
        TurnOutcome::AgentFrameSwitch {
            frame_key, task, ..
        } => (
            crate::session_graph::frame_node_id(&state.session_id, frame_key.as_str()),
            crate::store::FollowOnWork::FrameTask { task: task.clone() },
            owed.map_or(0, |owed| owed.chain_depth).saturating_add(1),
        ),
        TurnOutcome::SegmentBoundary { reason } => {
            let frame_id = frame.cloned().ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::RecordedTerminationUnavailable,
                    format!("the continuation of `{turn_id}` requires the frame the turn ran in"),
                )
            })?;
            let captured = segment_boundary.ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::ExecutionStateCaptureFailed,
                    "a segment boundary requires its logical opener's captured state",
                )
            })?;
            (
                frame_id,
                crate::store::FollowOnWork::Continuation(crate::store::RunContinuation {
                    reason: *reason,
                    protocol_iterations: captured.iterations,
                    cell: captured.cell.clone(),
                    opener: captured.opener.clone(),
                    tools: captured.tools.clone(),
                }),
                owed.map_or(0, |owed| owed.chain_depth),
            )
        }
        TurnOutcome::Finished(_) | TurnOutcome::Stopped(_) => return Ok(None),
    };
    let resolved = state.authority.run_view().ok_or_else(|| {
        RuntimeError::new(
            RuntimeErrorCode::RecordedTerminationUnavailable,
            format!("the follow-on of `{turn_id}` requires its run's recorded run"),
        )
    })?;
    let physical_ordinal = crate::store::PhysicalTurn::physical_ordinal_of(run, turn_id)
        .ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::ExecutionScopeTurnIdMismatch,
                format!("physical turn `{turn_id}` does not belong to run `{run}`"),
            )
        })?;
    match work {
        crate::store::FollowOnWork::Continuation(continuation) => {
            crate::store::PendingFollowOn::after_boundary(
                run,
                physical_ordinal,
                frame_id,
                continuation,
                chain_depth,
                resolved.run.clone(),
            )
        }
        crate::store::FollowOnWork::FrameTask { task } => {
            crate::store::PendingFollowOn::after_switch(
                run,
                physical_ordinal,
                frame_id,
                task,
                chain_depth,
                resolved.run.clone(),
            )
        }
    }
    .map(Some)
    .map_err(super::runtime_error_from_store_commit)
}

pub(super) struct PreparedLogicalTurn {
    pub(super) trace_metadata: std::collections::BTreeMap<String, serde_json::Value>,
    pub(super) messages: crate::MessageSequence,
    pub(super) prelude: Box<crate::runtime::effect::TurnPrelude>,
    pub(super) previous_prompt_usage: Option<TokenUsage>,
    pub(super) turn_context: crate::TurnContext,
    pub(super) initial_turn_causes: Vec<crate::TurnCause>,
    pub(super) trace_turn_id: TurnId,
    pub(super) turn_index: usize,
}

pub(super) enum LogicalTurnStart {
    /// An input, with the protocol turn options a follow-on turn recorded
    /// beyond its run's view (`None` for a run's own first turn).
    Input(TurnInput),
    /// A recovered follow-on whose recovery bound is spent (ADR 0101 §3): it
    /// never runs, and commits as the failed turn carrying
    /// `FollowOnRecoveryExhausted` with its task as the delivered input.
    ExhaustedFollowOn(Box<crate::store::PendingFollowOn>),
}

impl LogicalTurnStart {
    fn continuation_state(&self) -> (crate::TurnContext, Option<TurnId>) {
        match self {
            Self::Input(input) => (input.turn_context.clone(), input.trace_turn_id.clone()),
            Self::ExhaustedFollowOn(owed) => (
                crate::TurnContext::default(),
                Some(owed.follow_on_turn_id.clone()),
            ),
        }
    }
}

impl LashRuntime {
    fn emit_physical_turn_start(
        observer: &TurnObserver,
        scoped_effect_controller: &ScopedEffectController<'_>,
        turn_id: &TurnId,
        admissions: &LogicalTurnAdmissions,
        announce_queued_work: bool,
    ) {
        let mut cursor =
            super::turn_loop::turn_observation_cursor(scoped_effect_controller, turn_id, "start");
        super::turn_loop::emit_turn_started(observer, &mut cursor, turn_id);
        if !announce_queued_work {
            // Work withheld from a terminal checkpoint already announced its
            // start at the boundary that admitted it (FIG-3157).
            return;
        }
        for queued in &admissions.queued {
            let work = queued.materialize_queued_checkpoint_work();
            super::turn_loop::emit_queued_work_started(
                observer,
                &mut cursor,
                turn_id,
                crate::AdmissionBoundary::Idle,
                queued,
                work.turn_causes,
            );
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

    /// End the run at the commit of its physical turn `committed_turn`,
    /// handing back `withheld`: the rows that turn withheld from its
    /// terminal checkpoint for a follow-on turn this run will not execute
    /// after all (FIG-3157, FIG-3927).
    ///
    /// The commit that withheld them owed their follow-on, so it wrote no
    /// terminal, and the rows stay bound to the run. Once the follow-on
    /// fails before its commit, or the commit before it fails its delivery,
    /// nothing else ends the run or redrives it: its run returns, and no row
    /// owes a shift. So the run ends here, at the turn whose answer it
    /// committed, in one commit under the run's shift fence that releases
    /// the withheld rows open at their own positions, each owing its session a
    /// shift again, and writes the terminal a redrive of the run reads. A
    /// run that still owes a frame's follow-on is left to the shift that
    /// recovers it, and a commit that fails leaves the run as it was, to the
    /// next shift that resumes it.
    async fn end_run_without_follow_on(
        &mut self,
        committed_turn: &TurnId,
        outcome: &TurnOutcome,
        withheld: WithheldTerminalWork,
    ) {
        let Some(shift_commit) = self
            .shift_run
            .as_ref()
            .and_then(|run| run.commit_facts(committed_turn, outcome, false))
        else {
            return;
        };
        let refused = |runtime: &mut Self, error: &dyn std::fmt::Display| {
            runtime.invalidate_resident_session_state();
            tracing::warn!(
                error = %error,
                run = %shift_commit.run,
                "failed to end a run whose withheld follow-on work will not run"
            );
        };
        // The failed turn may have dirtied the resident state; the run ends
        // over the head its last commit wrote.
        if let Err(error) = self.refresh_resident_head().await {
            refused(self, &error);
            return;
        }
        if self.state.pending_follow_on.is_some() {
            return;
        }
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return;
        };
        let operation = crate::OperationId::new(
            crate::ExecutionScope::turn(self.state.session_id.clone(), committed_turn.clone()),
            "run-end",
        );
        let fleet_format = self.fleet_format();
        let (mut commit, persisted_node_ids) =
            match crate::store::RuntimeCommit::persisted_state_with_operation_and_budget(
                &mut self.state,
                operation,
                self.host.core.durability.commit_budget,
                fleet_format,
            ) {
                Ok(commit) => commit,
                Err(error) => {
                    refused(self, &error);
                    return;
                }
            };
        let settlement =
            super::turn_settlement::TurnIngressSettlement::default().with_undelivered(withheld);
        if !settlement.is_empty() {
            commit.ingress = Some(settlement.into_ingress(
                shift_commit.run.clone(),
                crate::TurnCancelUndeliveredInputPolicy::Defer,
            ));
        }
        commit.shift_fence = Some(Box::new(shift_commit.fence.clone()));
        commit.run_terminal = shift_commit.terminal.clone().map(Box::new);
        match store
            .commit_runtime_state_verified(commit, self.host.core.tracing.metrics())
            .await
        {
            Ok(receipt) => {
                if receipt.receipt_replayed {
                    self.invalidate_resident_session_state();
                } else {
                    self.state.apply_persisted_commit_result(receipt);
                    self.state.mark_node_ids_persisted(persisted_node_ids);
                }
                if let Some(run) = self.shift_run.as_mut() {
                    run.mark_terminal_written();
                }
            }
            Err(error) => refused(self, &error),
        }
    }

    /// How this runtime's turns frame their stream deltas for the host.
    pub(super) fn delta_framing(&self) -> super::turn_observer::DeltaFraming {
        super::turn_observer::DeltaFraming {
            clock: std::sync::Arc::clone(&self.host.core.clock),
            coalescing: self.host.core.control.delta_coalescing,
        }
    }

    /// Execute one logical turn while everything it publishes reaches the host
    /// sinks outside the shift.
    ///
    /// Every physical turn of the run publishes through one [`TurnObserver`],
    /// so the host receives one ordered stream; the shift never waits on a
    /// host sink, and nothing it commits is read back from what it published.
    /// The call returns only once the host has received the whole stream (the
    /// observer's host contract).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_logical_turn(
        &mut self,
        start: LogicalTurnStart,
        events: &dyn EventSink,
        turn_events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
        local_stop: LocalTurnStop,
        admissions: LogicalTurnAdmissions,
        shift_fence: Option<&ShiftFence>,
        stopwatch: TurnStopwatch,
    ) -> Result<AgentFrameRun, RuntimeError> {
        let (observer, mut observations) =
            TurnObserver::open(events, turn_events, self.delta_framing());
        let shift = std::pin::pin!(self.execute_observed_logical_turn(
            start,
            &observer,
            scoped_effect_controller,
            local_stop,
            admissions,
            shift_fence,
            stopwatch,
        ));
        work_with_observations(shift, &mut observations, |observation| {
            super::turn_loop::publish_observation(events, turn_events, observation)
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_observed_logical_turn(
        &mut self,
        mut start: LogicalTurnStart,
        observer: &TurnObserver,
        scoped_effect_controller: ScopedEffectController<'_>,
        local_stop: LocalTurnStop,
        mut admissions: LogicalTurnAdmissions,
        shift_fence: Option<&ShiftFence>,
        stopwatch: TurnStopwatch,
    ) -> Result<AgentFrameRun, RuntimeError> {
        let (follow_turn_context, supplied_trace_turn_id) = start.continuation_state();
        // A session operation owns no turn (FIG-3607 contract 4): a logical
        // turn is opened by its run's turn scope, or runs inside a process
        // or a runtime operation.
        if let crate::ExecutionScope::SessionOperation { operation_id, .. } =
            scoped_effect_controller.execution_scope()
        {
            return Err(RuntimeError::new(
                RuntimeErrorCode::ExecutionScopeTurnIdMismatch,
                format!(
                    "session operation `{operation_id}` owns no turn: a logical turn runs under \
                     its run's turn scope"
                ),
            ));
        }
        // A turn scope is its logical run's: the turn that starts under it
        // is the run's first physical turn, or, for a recovered follow-on,
        // a later physical turn of that run.
        if let Some(supplied_trace_turn_id) = supplied_trace_turn_id.as_ref()
            && let Some(scope_run) = scoped_effect_controller.execution_scope().turn_id()
            && crate::store::PhysicalTurn::physical_ordinal_of(scope_run, supplied_trace_turn_id)
                .is_none()
        {
            return Err(RuntimeError::new(
                RuntimeErrorCode::ExecutionScopeTurnIdMismatch,
                format!(
                    "input trace_turn_id `{supplied_trace_turn_id}` does not match execution scope id `{}`",
                    scoped_effect_controller.scope_id()
                ),
            ));
        }
        // The first physical turn's id. Every later turn of this logical run
        // counts on from it: a follow-on takes the id its committed switch
        // wrote on the head, and a terminal-checkpoint follow-on takes the next
        // physical index (ADR 0101 §3).
        let mut turn_trace_turn_id = match supplied_trace_turn_id {
            Some(supplied_trace_turn_id) => supplied_trace_turn_id,
            None => TurnId::parse(scoped_effect_controller.scope_id())?,
        };
        let logical_run = self
            .shift_run
            .as_ref()
            .map(|executed| executed.run().clone())
            .or_else(|| {
                self.state
                    .pending_follow_on
                    .as_deref()
                    .filter(|owed| owed.is_turn(&turn_trace_turn_id))
                    .map(|owed| owed.run_turn_id())
            })
            .unwrap_or_else(|| turn_trace_turn_id.clone());
        let mut physical_ordinal = crate::store::PhysicalTurn::physical_ordinal_of(
            &logical_run,
            &turn_trace_turn_id,
        )
        .ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::ExecutionScopeTurnIdMismatch,
                format!(
                    "physical turn `{turn_trace_turn_id}` does not belong to run `{logical_run}`"
                ),
            )
        })?;
        // An admission never mixes run specs, so the head input's spec is the
        // run's; a run of wakes runs the default spec, and a follow-on the
        // shape its parent run recorded on the pending fact (FIG-3877).
        let admitted_run_spec = admissions
            .turn_inputs
            .first()
            .and_then(|admitted| admitted.inputs.first())
            .and_then(|input| input.run_spec.clone());
        let inherited = self
            .state
            .pending_follow_on
            .as_deref()
            .filter(|owed| owed.is_turn(&turn_trace_turn_id))
            .map(|owed| (*owed.resolved_run).clone());
        // Activation publishes the protocol capabilities that config resolution
        // records, including its renderer. Physical preparation consumes them.
        self.materialize_turn_session(&scoped_effect_controller)
            .await?;
        self.resolve_turn_config(
            &scoped_effect_controller,
            &turn_trace_turn_id,
            admitted_run_spec.as_ref(),
            inherited,
        )
        .await?;
        let mut turns: Vec<AssembledTurn> = Vec::new();
        // FIG-3157: work admitted at a terminal checkpoint, withheld from the
        // delivery so the committed finish stayed the turn's answer, waiting
        // for the follow-on turn that executes it.
        let mut carried_withheld: Option<WithheldTerminalWork> = None;
        // The last committed physical turn, and the withheld rows the
        // follow-on turn now running executes (FIG-3157).
        let mut committed_turn: Option<TurnId> = None;
        let mut follow_on_rows: Option<WithheldTerminalWork> = None;
        let mut announce_queued_work = true;
        let mut follow_on_turns = 0usize;

        loop {
            // A frame switch creates a new physical turn identity, but it does
            // not create new effect authority. Every frame in this admitted
            // run therefore keeps the controller's exact execution scope.
            let turn_effect_controller = scoped_effect_controller.clone();
            let frame_stopwatch = if turns.is_empty() {
                stopwatch
            } else {
                TurnStopwatch::start(self.host.core.clock.as_ref())
            };
            Self::emit_physical_turn_start(
                observer,
                &scoped_effect_controller,
                &turn_trace_turn_id,
                &admissions,
                announce_queued_work,
            );
            announce_queued_work = true;
            // A follow-on that must not run commits its failure as its
            // terminal record instead, whatever shift reached it (ADR 0101
            // §3): its recovery bound is spent, or its chain passed
            // MAX_AGENT_FRAME_SWITCHES switches (the chain bound travels with
            // the follow-on).
            let terminal = match &start {
                LogicalTurnStart::ExhaustedFollowOn(owed) => Some((
                    crate::TurnFailureCode::FollowOnRecoveryExhausted,
                    format!(
                        "follow-on turn `{}` was recovered {} times, past the host's bound; it \
                         commits failed instead of running",
                        owed.follow_on_turn_id, owed.attempts
                    ),
                    owed.owes.task().map(str::to_owned),
                )),
                _ => self
                    .state
                    .pending_follow_on
                    .as_ref()
                    .filter(|owed| {
                        owed.is_turn(&turn_trace_turn_id)
                            && owed.chain_depth as usize >= MAX_AGENT_FRAME_SWITCHES
                    })
                    .map(|owed| {
                        (
                            crate::TurnFailureCode::AgentFrameSwitchLimit,
                            format!(
                                "logical turn exceeded the limit of {MAX_AGENT_FRAME_SWITCHES} agent frame switches"
                            ),
                            owed.owes.task().map(str::to_owned),
                        )
                    }),
            };
            if let Some((code, message, task)) = terminal {
                // A terminal record starts no follow-on: work an earlier turn
                // withheld for one is handed back open by its commit.
                let terminal = Box::pin(self.finish_logical_turn_error(LogicalTurnErrorContext {
                    code,
                    message,
                    trace_turn_id: turn_trace_turn_id,
                    delivered_task: task,
                    sinks: TurnSinks { observer },
                    scoped_effect_controller: turn_effect_controller,
                    admissions: admissions.with_follow_on_allowed(false),
                    shift_fence,
                }))
                .await;
                let mut terminal = match terminal {
                    Ok(terminal) => terminal,
                    Err(error) if turns.is_empty() => {
                        self.invalidate_resident_session_state();
                        return Err(error);
                    }
                    Err(error) => {
                        self.record_follow_on_failure(&mut turns, error);
                        return Ok(AgentFrameRun {
                            turns,
                            acceptance: None,
                        });
                    }
                };
                frame_stopwatch.stamp(&mut terminal.turn, self.host.core.clock.as_ref());
                turns.push(terminal.turn);
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
                            sinks: TurnSinks { observer },
                            scoped_effect_controller: turn_effect_controller,
                            local_stop: local_stop.clone(),
                            admissions: std::mem::replace(
                                &mut admissions,
                                LogicalTurnAdmissions::new(Vec::new(), Vec::new()),
                            ),
                            materialize_initial_admissions: true,
                            shift_fence,
                        },
                    ))
                    .await
                }
                // Committed as its terminal above; it never executes.
                LogicalTurnStart::ExhaustedFollowOn(owed) => Err(RuntimeError::new(
                    RuntimeErrorCode::FollowOnPending,
                    format!(
                        "follow-on turn `{}` commits its exhaustion before any execution",
                        owed.follow_on_turn_id
                    ),
                )),
            };
            let execution = match execution_result {
                Ok(execution) => execution,
                // This frame ended without reaching a commit. The rows its
                // run admitted stay bound to the run, and input routed to the
                // frame while it was live stays open: the run's terminal write
                // releases the one and re-defers the other (FIG-3927 §2.6), and
                // a redrive of the run executes its journal again (ADR 0101 A3).
                //
                // The rejected turn may have mutated the live execution before
                // it failed (an after-turn hook refusing finalization runs after
                // the executor already applied the turn), so the resident state
                // is invalidated exactly like a rejected follow-on turn's: the
                // next use reloads from the accepted snapshot instead of running
                // the executor this turn dirtied.
                //
                // A follow-on that fails before its commit stays owed on the
                // head (ADR 0101 §3): the next shift recovers it.
                Err(err) if turns.is_empty() => {
                    self.invalidate_resident_session_state();
                    return Err(err);
                }
                // A FIG-3157 follow-on that failed ends its run at the turn
                // that withheld its rows, unless the failure parked the run:
                // a park holds the run's rows until it is resolved.
                Err(err) => {
                    let parked = err.turn_failure_cause() == crate::TurnFailureCause::Parked;
                    self.record_follow_on_failure(&mut turns, err);
                    if let (false, Some(withheld), Some(committed), Some(last)) = (
                        parked,
                        follow_on_rows.take(),
                        committed_turn.as_ref(),
                        turns.last(),
                    ) {
                        let outcome = last.outcome.clone();
                        Box::pin(self.end_run_without_follow_on(committed, &outcome, withheld))
                            .await;
                    }
                    return Ok(AgentFrameRun {
                        turns,
                        acceptance: None,
                    });
                }
            };
            committed_turn = Some(turn_trace_turn_id.clone());
            follow_on_rows = None;
            let PhysicalTurnExecution {
                mut turn,
                post_commit_delivery_failed,
                withheld_terminal_work,
            } = execution;
            if let Some(withheld) = withheld_terminal_work {
                let carried = carried_withheld.get_or_insert_with(WithheldTerminalWork::default);
                carried.queued.extend(withheld.queued);
                carried.turn_inputs.extend(withheld.turn_inputs);
            }
            frame_stopwatch.stamp(&mut turn, self.host.core.clock.as_ref());
            turns.push(turn);
            if post_commit_delivery_failed {
                // The run stops before the follow-on its withheld rows owe,
                // so the run ends at this commit instead.
                if let (Some(withheld), Some(last)) = (carried_withheld.take(), turns.last()) {
                    let outcome = last.outcome.clone();
                    Box::pin(self.end_run_without_follow_on(
                        &turn_trace_turn_id,
                        &outcome,
                        withheld,
                    ))
                    .await;
                }
                return Ok(AgentFrameRun {
                    turns,
                    acceptance: None,
                });
            }
            // A committed frame switch owes its follow-on, which runs next,
            // in this run, under the id the switch wrote (ADR 0101 §3). The
            // one path for every session: durable or store-less, the fact is
            // on the resident head.
            if let Some(owed) = self.state.pending_follow_on.as_deref().cloned() {
                // A segment boundary ends this invocation's part of the run
                // (FIG-4739): the continuation it owes stays on the head for
                // the next shift to admit, in an invocation of its own. A
                // turn that takes a boundary carries no withheld work.
                if matches!(owed.owes, crate::store::FollowOnWork::Continuation(_)) {
                    return Ok(AgentFrameRun {
                        turns,
                        acceptance: None,
                    });
                }
                turn_trace_turn_id = owed.follow_on_turn_id.clone();
                physical_ordinal = owed.physical_index();
                self.pin_committed_follow_on_index(turns.last(), &owed.follow_on_turn_id)
                    .await?;
                start =
                    LogicalTurnStart::Input(follow_on_input(&owed, follow_turn_context.clone()));
                // Work an earlier turn withheld at its terminal checkpoint
                // still waits for its FIG-3157 follow-on, which runs after
                // the frame's. The frame's turn carries it: its commit owes
                // that follow-on too, so it neither settles the rows nor
                // ends the run that holds them (FIG-4044).
                admissions = LogicalTurnAdmissions::new(Vec::new(), Vec::new())
                    .with_withheld_terminal_work(carried_withheld.take());
                continue;
            }
            // FIG-3157: the turn ended on its committed answer. Work it
            // admitted at the terminal checkpoint starts the next turn now
            // — no idle gap, no wait for the user, the same shift fence and
            // generation throughout. A run past its follow-on bound carried
            // nothing: its commit handed the withheld rows back open.
            let Some(withheld) = carried_withheld.take() else {
                return Ok(AgentFrameRun {
                    turns,
                    acceptance: None,
                });
            };
            follow_on_turns += 1;
            turn_trace_turn_id = next_physical_turn_id(&logical_run, physical_ordinal)
                .map_err(super::runtime_error_from_store_commit)?;
            physical_ordinal += 1;
            self.pin_committed_follow_on_index(turns.last(), &turn_trace_turn_id)
                .await?;
            let mut input = TurnInput::items(Vec::new());
            input.turn_context = follow_turn_context.clone();
            follow_on_rows = Some(withheld.clone());
            admissions = LogicalTurnAdmissions::new(withheld.queued, withheld.turn_inputs)
                .with_follow_on_allowed(follow_on_turns < MAX_TERMINAL_CHECKPOINT_FOLLOW_ONS);
            announce_queued_work = false;
            start = LogicalTurnStart::Input(input);
        }
    }
}

impl LashRuntime {
    /// Pin the turn index of `follow_on`, the physical turn that runs after
    /// `previous` in the same run, when it has already committed.
    ///
    /// A late crash, or a tier that replays the run's journal at every
    /// await, may replay this run after its follow-on has committed. In
    /// that case the previous turn's recorded commit fixes the follow-on
    /// index; loading the newer head would rename its earlier recorded
    /// effects. An uncommitted follow-on still refreshes the head, including
    /// graph writes made after the previous turn's commit. The one rule for
    /// a frame follow-on and a terminal-checkpoint follow-on.
    async fn pin_committed_follow_on_index(
        &mut self,
        previous: Option<&AssembledTurn>,
        follow_on: &TurnId,
    ) -> Result<(), RuntimeError> {
        if let (Some(previous), Some(store)) = (
            previous,
            self.session
                .as_ref()
                .and_then(|session| session.history_store()),
        ) && store
            .committed_turn_exists(follow_on)
            .await
            .map_err(super::runtime_error_from_store_commit)?
        {
            self.admitted_turn_index =
                Some(previous.state.turn_index.checked_add(1).ok_or_else(|| {
                    RuntimeError::new(
                        RuntimeErrorCode::StoreCommitFailed,
                        "follow-on turn index exceeds platform range",
                    )
                })?);
        }
        Ok(())
    }
}

/// The input of the follow-on `owed`: its task (ADR 0101 §3), or nothing for
/// a run's continuation, which goes on from the history its boundary
/// committed (FIG-4739). It runs under the recorded run its run resolved,
/// which the fact carries.
pub(super) fn follow_on_input(
    owed: &crate::store::PendingFollowOn,
    turn_context: crate::TurnContext,
) -> TurnInput {
    let mut input = match &owed.owes {
        crate::store::FollowOnWork::Continuation(_) => TurnInput::items(Vec::new()),
        crate::store::FollowOnWork::FrameTask { task } => TurnInput::text(task.clone()),
    };
    input.turn_context = turn_context;
    input.trace_turn_id = Some(owed.follow_on_turn_id.clone());
    input
}

/// The physical turn after `physical_ordinal` within the known logical `run`.
pub(super) fn next_physical_turn_id(
    run: &TurnId,
    physical_ordinal: u64,
) -> Result<TurnId, crate::StoreError> {
    let next =
        crate::StoreError::checked_monotonic_increment("physical_turn_index", physical_ordinal)?;
    Ok(crate::store::PhysicalTurn::derive_turn_id(run, next))
}
