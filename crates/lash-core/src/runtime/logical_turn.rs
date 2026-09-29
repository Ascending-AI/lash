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
/// the root throughout (FIG-3927).
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

/// The rows one physical turn drives, each admitted to the turn's root
/// (FIG-3927): the root's own admission, and what its checkpoints admitted.
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

    /// The withheld work the logical run drives in a follow-on turn once this
    /// turn has committed. See [`Self::carries_follow_on_work`].
    pub(super) fn take_follow_on_work(&mut self, cancelled: bool) -> Option<WithheldTerminalWork> {
        let carries = self.carries_follow_on_work(cancelled);
        let withheld = self.withheld_terminal_work.take()?;
        carries.then_some(withheld)
    }

    /// What this turn's commit settles: every row it drove completes, and
    /// withheld work no follow-on turn drives is handed back.
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
/// continuing the depth the turn itself was owed with. Every other outcome
/// leaves nothing: a turn commits only while the head owes nothing or owes
/// this very turn, and this commit is that follow-on's terminal record.
pub(super) fn follow_on_after_turn(
    state: &RuntimeSessionState,
    outcome: &TurnOutcome,
    turn_id: &TurnId,
) -> Result<Option<crate::store::PendingFollowOn>, RuntimeError> {
    let TurnOutcome::AgentFrameSwitch {
        frame_key, task, ..
    } = outcome
    else {
        return Ok(None);
    };
    let chain_depth = state
        .pending_follow_on
        .as_ref()
        .filter(|owed| owed.is_turn(turn_id))
        .map_or(0, |owed| owed.chain_depth)
        .saturating_add(1);
    crate::store::PendingFollowOn::after_switch(
        turn_id,
        crate::session_graph::frame_node_id(&state.session_id, frame_key.as_str()),
        task.clone(),
        Some(state.effective_protocol_turn_options().clone()),
        chain_depth,
        state.authority.resolved_run.as_deref().cloned(),
    )
    .map(Some)
    .map_err(super::runtime_error_from_store_commit)
}

pub(super) struct PreparedLogicalTurn {
    pub(super) messages: crate::MessageSequence,
    pub(super) previous_prompt_usage: Option<TokenUsage>,
    pub(super) protocol_turn_options: Option<crate::ProtocolTurnOptions>,
    pub(super) turn_context: crate::TurnContext,
    pub(super) initial_turn_causes: Vec<crate::TurnCause>,
    pub(super) trace_turn_id: TurnId,
    pub(super) turn_index: usize,
}

pub(super) enum LogicalTurnStart {
    /// An input, with the protocol turn options a follow-on turn recorded
    /// beyond its root's view (`None` for a root's own first turn).
    Input(TurnInput, Option<crate::ProtocolTurnOptions>),
    /// A recovered follow-on whose recovery bound is spent (ADR 0101 §3): it
    /// never runs, and commits as the failed turn carrying
    /// `FollowOnRecoveryExhausted` with its task as the delivered input.
    ExhaustedFollowOn(crate::store::PendingFollowOn),
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
            Self::Input(input, options) => (
                options.clone(),
                input.turn_context.clone(),
                input
                    .trace_turn_id
                    .clone()
                    .unwrap_or_else(|| TurnId::from("")),
            ),
            Self::ExhaustedFollowOn(owed) => (
                owed.options.as_deref().cloned(),
                crate::TurnContext::default(),
                owed.follow_on_turn_id.clone(),
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

    /// End the root at the commit of its physical turn `committed_turn`,
    /// handing back `withheld`: the rows that turn withheld from its
    /// terminal checkpoint for a follow-on turn this run will not drive
    /// after all (FIG-3157, FIG-3927).
    ///
    /// The commit that withheld them owed their follow-on, so it wrote no
    /// terminal, and the rows stay bound to the root. Once the follow-on
    /// fails before its commit, or the commit before it fails its delivery,
    /// nothing else ends the root or redrives it: its run returns, and no row
    /// owes a drive. So the root ends here, at the turn whose answer it
    /// committed, in one commit under the root's drive fence that releases
    /// the withheld rows open at their own positions, each owing its session a
    /// drive again, and writes the terminal a redrive of the root reads. A
    /// root that still owes a frame's follow-on is left to the drive that
    /// recovers it, and a commit that fails leaves the root as it was, to the
    /// next drive that resumes it.
    async fn end_root_without_follow_on(
        &mut self,
        committed_turn: &TurnId,
        outcome: &TurnOutcome,
        withheld: WithheldTerminalWork,
    ) {
        let Some(drive_commit) = self
            .drive_root
            .as_ref()
            .and_then(|root| root.commit_facts(committed_turn, outcome, false))
        else {
            return;
        };
        let refused = |runtime: &mut Self, error: &dyn std::fmt::Display| {
            runtime.invalidate_resident_session_state();
            tracing::warn!(
                error = %error,
                root = %drive_commit.root,
                "failed to end a root whose withheld follow-on work will not run"
            );
        };
        // The failed turn may have dirtied the resident state; the root ends
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
            crate::ExecutionScope::turn(self.state.session_id.as_str(), committed_turn.clone()),
            "root-end",
        );
        let fleet_format = self.fleet_format();
        let (mut commit, persisted_node_ids) =
            match crate::store::RuntimeCommit::persisted_state_with_operation_and_budget(
                &mut self.state,
                &[],
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
                drive_commit.root.clone(),
                crate::TurnCancelDisposition::Defer,
            ));
        }
        commit.drive_fence = Some(Box::new(drive_commit.fence.clone()));
        commit.root_terminal = drive_commit.terminal.clone().map(Box::new);
        match crate::store::commit_runtime_state_verified(store.as_ref(), commit).await {
            Ok(receipt) => {
                if receipt.receipt_replayed {
                    self.invalidate_resident_session_state();
                } else {
                    self.state.apply_persisted_commit_result(receipt);
                    self.state.mark_node_ids_persisted(persisted_node_ids);
                }
                if let Some(root) = self.drive_root.as_mut() {
                    root.mark_terminal_written();
                }
            }
            Err(error) => refused(self, &error),
        }
    }

    /// Drive one logical turn while everything it publishes reaches the host
    /// sinks outside the drive.
    ///
    /// Every physical turn of the run publishes through one [`TurnObserver`],
    /// so the host receives one ordered stream; the drive never waits on a
    /// host sink, and nothing it commits is read back from what it published.
    /// The call returns only once the host has received the whole stream (the
    /// observer's host contract).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn drive_logical_turn(
        &mut self,
        start: LogicalTurnStart,
        events: &dyn EventSink,
        turn_events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
        local_stop: LocalTurnStop,
        admissions: LogicalTurnAdmissions,
        drive_fence: Option<&DriveFence>,
        stopwatch: TurnStopwatch,
    ) -> Result<AgentFrameRun, RuntimeError> {
        let (observer, mut observations) = TurnObserver::open(events, turn_events);
        let drive = std::pin::pin!(self.drive_observed_logical_turn(
            start,
            &observer,
            scoped_effect_controller,
            local_stop,
            admissions,
            drive_fence,
            stopwatch,
        ));
        drive_with_observations(drive, &mut observations, |observation| {
            super::turn_loop::publish_observation(events, turn_events, observation)
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn drive_observed_logical_turn(
        &mut self,
        mut start: LogicalTurnStart,
        observer: &TurnObserver,
        scoped_effect_controller: ScopedEffectController<'_>,
        local_stop: LocalTurnStop,
        mut admissions: LogicalTurnAdmissions,
        drive_fence: Option<&DriveFence>,
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
        // The first physical turn's id. Every later turn of this logical run
        // counts on from it: a follow-on takes the id its committed switch
        // wrote on the head, and a terminal-checkpoint follow-on takes the next
        // physical index (ADR 0101 §3).
        let mut turn_trace_turn_id = if supplied_trace_turn_id.is_empty() {
            TurnId::from(scoped_effect_controller.scope_id())
        } else {
            supplied_trace_turn_id
        };
        // An admission never mixes run specs, so the head input's spec is the
        // root's; a root of wakes runs the default spec, and a follow-on the
        // shape its parent root recorded on the pending fact (FIG-3877).
        let root_spec = admissions
            .turn_inputs
            .first()
            .and_then(|admitted| admitted.inputs.first())
            .and_then(|input| input.run_spec.clone());
        let inherited = self
            .state
            .pending_follow_on
            .as_deref()
            .filter(|owed| owed.is_turn(&turn_trace_turn_id))
            .and_then(|owed| owed.resolved_run.as_deref().cloned());
        self.resolve_turn_config(
            &scoped_effect_controller,
            &turn_trace_turn_id,
            root_spec.as_ref(),
            inherited,
        )
        .await?;
        let mut turns: Vec<AssembledTurn> = Vec::new();
        // FIG-3157: work admitted at a terminal checkpoint, withheld from the
        // delivery so the committed finish stayed the turn's answer, waiting
        // for the follow-on turn that drives it.
        let mut carried_withheld: Option<WithheldTerminalWork> = None;
        // The last committed physical turn, and the withheld rows the
        // follow-on turn now running drives (FIG-3157).
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
            // terminal record instead, whatever drive reached it (ADR 0101
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
                    owed.task.clone(),
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
                            owed.task.clone(),
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
                    delivered_task: Some(task),
                    sinks: TurnSinks { observer },
                    scoped_effect_controller: turn_effect_controller,
                    admissions: admissions.with_follow_on_allowed(false),
                    drive_fence,
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
                LogicalTurnStart::Input(mut input, protocol_turn_options) => {
                    input.trace_turn_id = Some(turn_trace_turn_id.clone());
                    Box::pin(self.stream_turn_with_scoped_effect_controller_inner(
                        TurnPrepareContext {
                            input,
                            protocol_turn_options,
                            sinks: TurnSinks { observer },
                            scoped_effect_controller: turn_effect_controller,
                            local_stop: local_stop.clone(),
                            admissions: std::mem::replace(
                                &mut admissions,
                                LogicalTurnAdmissions::new(Vec::new(), Vec::new()),
                            ),
                            materialize_initial_admissions: true,
                            drive_fence,
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
                // root admitted stay bound to the root, and input routed to the
                // frame while it was live stays open: the root's terminal write
                // releases the one and re-defers the other (FIG-3927 §2.6), and
                // a redrive of the root drives its journal again (ADR 0101 A3).
                //
                // The rejected turn may have mutated the live execution before
                // it failed (an after-turn hook refusing finalization runs after
                // the executor already applied the turn), so the resident state
                // is invalidated exactly like a rejected follow-on turn's: the
                // next use reloads from the accepted snapshot instead of running
                // the executor this turn dirtied.
                //
                // A follow-on that fails before its commit stays owed on the
                // head (ADR 0101 §3): the next drive recovers it.
                Err(err) if turns.is_empty() => {
                    self.invalidate_resident_session_state();
                    return Err(err);
                }
                // A FIG-3157 follow-on that failed ends its root at the turn
                // that withheld its rows, unless the failure parked the root:
                // a park holds the root's rows until it is resolved.
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
                        Box::pin(self.end_root_without_follow_on(committed, &outcome, withheld))
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
                // so the root ends at this commit instead.
                if let (Some(withheld), Some(last)) = (carried_withheld.take(), turns.last()) {
                    let outcome = last.outcome.clone();
                    Box::pin(self.end_root_without_follow_on(
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
                turn_trace_turn_id = owed.follow_on_turn_id.clone();
                // A late crash may replay this root after its follow-on has
                // committed. In that case the first frame's recorded commit
                // fixes the follow-on index; loading the newer head would
                // rename its earlier recorded effects. An uncommitted
                // follow-on still refreshes the head, including graph writes
                // made after the first frame's commit.
                if let (Some(previous), Some(store)) = (
                    turns.last(),
                    self.session
                        .as_ref()
                        .and_then(|session| session.history_store()),
                ) && store
                    .committed_turn_exists(&owed.follow_on_turn_id)
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
                let (input, options) = follow_on_input(&owed, follow_turn_context.clone());
                start = LogicalTurnStart::Input(input, options);
                // Work an earlier turn withheld at its terminal checkpoint
                // still waits for its FIG-3157 follow-on, which runs after
                // the frame's. The frame's turn carries it: its commit owes
                // that follow-on too, so it neither settles the rows nor
                // ends the root that holds them (FIG-4044).
                admissions = LogicalTurnAdmissions::new(Vec::new(), Vec::new())
                    .with_withheld_terminal_work(carried_withheld.take());
                continue;
            }
            // FIG-3157: the turn ended on its committed answer. Work it
            // admitted at the terminal checkpoint starts the next turn now
            // — no idle gap, no wait for the user, the same drive fence and
            // generation throughout. A run past its follow-on bound carried
            // nothing: its commit handed the withheld rows back open.
            let Some(withheld) = carried_withheld.take() else {
                return Ok(AgentFrameRun {
                    turns,
                    acceptance: None,
                });
            };
            follow_on_turns += 1;
            turn_trace_turn_id = next_physical_turn_id(&turn_trace_turn_id)
                .map_err(super::runtime_error_from_store_commit)?;
            let mut input = TurnInput::items(Vec::new());
            input.turn_context = follow_turn_context.clone();
            follow_on_rows = Some(withheld.clone());
            admissions = LogicalTurnAdmissions::new(withheld.queued, withheld.turn_inputs)
                .with_follow_on_allowed(follow_on_turns < MAX_TERMINAL_CHECKPOINT_FOLLOW_ONS);
            announce_queued_work = false;
            start = LogicalTurnStart::Input(input, follow_protocol_turn_options.clone());
        }
    }
}

/// The input of the follow-on `owed`: its task, and the protocol turn
/// options the switch recorded (ADR 0101 §3).
pub(super) fn follow_on_input(
    owed: &crate::store::PendingFollowOn,
    turn_context: crate::TurnContext,
) -> (TurnInput, Option<crate::ProtocolTurnOptions>) {
    let mut input = TurnInput::text(owed.task.clone());
    input.turn_context = turn_context;
    input.trace_turn_id = Some(owed.follow_on_turn_id.clone());
    (input, owed.options.as_deref().cloned())
}

/// The next physical turn of the logical run `current` belongs to.
pub(super) fn next_physical_turn_id(current: &TurnId) -> Result<TurnId, crate::StoreError> {
    let (root, index) = crate::store::PhysicalTurn::split_turn_id(current);
    let next = crate::StoreError::checked_monotonic_increment("physical_turn_index", index)?;
    Ok(crate::store::PhysicalTurn::derive_turn_id(&root, next))
}
