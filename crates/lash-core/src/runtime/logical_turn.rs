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

/// Rows a resumed queued run retook under its resuming generation because
/// its checkpoints had been assigned them (FIG-3552).
///
/// They are not the run's input. A replayed checkpoint restores the first
/// execution's claim to such a row, and that claim is superseded by this one;
/// the turn settles the row under this claim instead, so ownership moves only
/// through the claim CAS and a superseded restored claim can only mean another
/// driver took the row.
#[derive(Default)]
pub(crate) struct ReacquiredClaims {
    pub(crate) queued: Vec<crate::QueuedWorkClaim>,
    pub(crate) turn_inputs: Vec<crate::TurnInputClaim>,
}

fn claim_authority<C>(claim: &crate::WorkClaim<C>) -> (u64, u64) {
    (claim.session_lease_generation, claim.fencing_token)
}

/// The reacquired claim that outranks `claim` and holds the row `holds` names.
fn outranking<C>(
    reacquired: &[crate::WorkClaim<C>],
    claim: &crate::WorkClaim<C>,
    holds: impl Fn(&crate::WorkClaim<C>) -> bool,
) -> Option<usize> {
    reacquired.iter().position(|candidate| {
        claim_authority(candidate) > claim_authority(claim) && holds(candidate)
    })
}

/// Settle every queued-work row this turn holds, moving each row a
/// reacquired claim outranks onto that claim.
fn queued_work_completions(
    held: &[crate::QueuedWorkClaim],
    reacquired: &[crate::QueuedWorkClaim],
) -> Vec<crate::QueuedWorkCompletion> {
    let mut moved: Vec<Vec<crate::BatchId>> = vec![Vec::new(); reacquired.len()];
    let mut completions = Vec::new();
    for claim in held {
        let mut completion = claim.completion();
        completion.batch_ids.retain(|id| {
            let Some(index) = outranking(reacquired, claim, |candidate| {
                candidate.batches.iter().any(|batch| batch.batch_id == *id)
            }) else {
                return true;
            };
            if !moved[index].contains(id) {
                moved[index].push(id.clone());
            }
            false
        });
        if !completion.batch_ids.is_empty() {
            completions.push(completion);
        }
    }
    for (claim, batch_ids) in reacquired.iter().zip(moved) {
        if !batch_ids.is_empty() {
            let mut completion = claim.completion();
            completion.batch_ids = batch_ids;
            completions.push(completion);
        }
    }
    completions
}

/// [`queued_work_completions`] for turn input: a moved row carries its
/// delivery record with it.
fn turn_input_completions(
    held: &[crate::TurnInputClaim],
    reacquired: &[crate::TurnInputClaim],
) -> Vec<crate::TurnInputCompletion> {
    let mut moved: Vec<(Vec<crate::InputId>, Vec<crate::TurnInputApplication>)> =
        vec![(Vec::new(), Vec::new()); reacquired.len()];
    let mut completions = Vec::new();
    for claim in held {
        let mut completion = claim.completion();
        let mut kept = Vec::new();
        for id in std::mem::take(&mut completion.input_ids) {
            let Some(index) = outranking(reacquired, claim, |candidate| {
                candidate.inputs.iter().any(|input| input.input_id == id)
            }) else {
                kept.push(id);
                continue;
            };
            let (ids, applications) = &mut moved[index];
            if !ids.contains(&id) {
                applications.extend(
                    completion
                        .applications
                        .iter()
                        .filter(|application| application.input_id == id)
                        .cloned(),
                );
                ids.push(id);
            }
        }
        completion
            .applications
            .retain(|application| kept.contains(&application.input_id));
        completion.input_ids = kept;
        if !completion.input_ids.is_empty() {
            completions.push(completion);
        }
    }
    for (claim, (input_ids, applications)) in reacquired.iter().zip(moved) {
        if !input_ids.is_empty() {
            let mut completion = claim.completion();
            completion.input_ids = input_ids;
            completion.applications = applications;
            completions.push(completion);
        }
    }
    completions
}

pub(super) struct PhysicalTurnExecution {
    pub(super) turn: AssembledTurn,
    pub(super) post_commit_delivery_failed: bool,
    /// Claimed at this turn's terminal checkpoint and withheld from it, for
    /// the logical run to start a follow-on turn with.
    pub(super) withheld_terminal_work: Option<WithheldTerminalWork>,
}

pub(super) struct LogicalTurnClaims {
    pub(super) queued: Vec<crate::QueuedWorkClaim>,
    /// The turn-input rows this turn drives, each under the generation-fenced
    /// claim it will settle.
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

    /// `journaled_drive_claims` names the claims of the journaled initial
    /// drive set: a superseded one cedes the turn whatever generation it was
    /// taken under (ADR 0069 §6). Every other claim cedes when it is
    /// superseded after being restored from an earlier execution (FIG-3552).
    /// `reacquired` names the rows a resumed queued run retook under its
    /// resuming generation: they settle under those claims.
    pub(super) fn commit_effects(
        &self,
        outcome: &TurnOutcome,
        journaled_drive_claims: &std::collections::BTreeSet<String>,
        reacquired: &ReacquiredClaims,
        pending_follow_on: Option<crate::store::PendingFollowOn>,
    ) -> LogicalTurnCommitEffects {
        let completed_queue_claims = queued_work_completions(&self.queued, &reacquired.queued);
        let completed_turn_input_claims =
            turn_input_completions(&self.turn_inputs, &reacquired.turn_inputs);
        let queue_claim_generations = self
            .queued
            .iter()
            .chain(&reacquired.queued)
            .map(|claim| (claim.claim_id.clone(), claim.session_lease_generation))
            .collect();
        let turn_input_claim_generations = self
            .turn_inputs
            .iter()
            .chain(&reacquired.turn_inputs)
            .map(|claim| (claim.claim_id.clone(), claim.session_lease_generation))
            .collect();
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
            .with_undelivered_turn_inputs(undelivered_turn_inputs)
            .with_journaled_drive_claims(journaled_drive_claims.clone()),
            pending_follow_on,
        }
    }
}

pub(super) struct LogicalTurnCommitEffects {
    pub(super) claim_settlement: TurnClaimSettlement,
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
    )
    .map(Some)
    .map_err(super::runtime_error_from_store_commit)
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
        claims: &LogicalTurnClaims,
        announce_queued_work: bool,
    ) {
        let mut cursor =
            super::turn_loop::turn_observation_cursor(scoped_effect_controller, turn_id, "start");
        super::turn_loop::emit_turn_started(observer, &mut cursor, turn_id);
        if !announce_queued_work {
            // Work withheld from a terminal checkpoint already announced its
            // start at the boundary that claimed it (FIG-3157).
            return;
        }
        for claim in &claims.queued {
            let work = claim.materialize_queued_checkpoint_work();
            super::turn_loop::emit_queued_work_started(
                observer,
                &mut cursor,
                turn_id,
                crate::QueuedWorkClaimBoundary::Idle,
                claim,
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
        claims: LogicalTurnClaims,
        session_execution_lease: &mut Option<SessionExecutionLeaseGuard>,
        stopwatch: TurnStopwatch,
    ) -> Result<AgentFrameRun, RuntimeError> {
        let (observer, mut observations) = TurnObserver::open(events, turn_events);
        let drive = std::pin::pin!(self.drive_observed_logical_turn(
            start,
            &observer,
            scoped_effect_controller,
            local_stop,
            claims,
            session_execution_lease,
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
        // The first physical turn's id. Every later turn of this logical run
        // counts on from it: a follow-on takes the id its committed switch
        // wrote on the head, and a terminal-checkpoint follow-on takes the next
        // physical index (ADR 0101 §3).
        let mut turn_trace_turn_id = if let Some(run) = &self.queued_run {
            run.position.turn_id.clone()
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
            if let Some(run) = &self.queued_run {
                turn_trace_turn_id = run.position.turn_id.clone();
            }
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
                observer,
                &scoped_effect_controller,
                &turn_trace_turn_id,
                &claims,
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
                let terminal = Box::pin(self.finish_logical_turn_error(LogicalTurnErrorContext {
                    code,
                    message,
                    trace_turn_id: turn_trace_turn_id,
                    delivered_task: Some(task),
                    sinks: TurnSinks { observer },
                    scoped_effect_controller: turn_effect_controller,
                    claims,
                    session_execution_lease: session_execution_lease.as_ref(),
                }))
                .await;
                if let Some(withheld) = carried_withheld.take() {
                    self.abandon_withheld_terminal_work(withheld).await;
                }
                let mut terminal = match terminal {
                    Ok(terminal) => terminal,
                    Err(error) if turns.is_empty() || self.queued_run.is_some() => {
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
                            sinks: TurnSinks { observer },
                            scoped_effect_controller: turn_effect_controller,
                            local_stop: local_stop.clone(),
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
                        sinks: TurnSinks { observer },
                        scoped_effect_controller: turn_effect_controller,
                        local_stop: local_stop.clone(),
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
                //
                // A parked turn is exempt: its journal diverged at the refusal,
                // so it issues no further journaled effect (the repair's cancel
                // gate peek is one), and its turn id is the one its redrive
                // carries, so the inputs routed to it are not orphaned.
                //
                // A follow-on that fails before its commit stays owed on the
                // head (ADR 0101 §3): the next drive recovers it.
                Err(err) if turns.is_empty() => {
                    if !parks(&err) && !self.owes_follow_on(&turn_trace_turn_id) {
                        self.defer_orphaned_turn_inputs_after_teardown(
                            &turn_trace_turn_id,
                            session_execution_lease
                                .as_ref()
                                .map(|lease| lease.fence())
                                .as_ref(),
                            &teardown_effect_controller,
                        )
                        .await;
                    }
                    self.invalidate_resident_session_state();
                    if let Some(withheld) = carried_withheld.take() {
                        self.abandon_withheld_terminal_work(withheld).await;
                    }
                    return Err(err);
                }
                Err(err) => {
                    if !parks(&err) && !self.owes_follow_on(&turn_trace_turn_id) {
                        self.defer_orphaned_turn_inputs_after_teardown(
                            &turn_trace_turn_id,
                            session_execution_lease
                                .as_ref()
                                .map(|lease| lease.fence())
                                .as_ref(),
                            &teardown_effect_controller,
                        )
                        .await;
                    }
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
                let frame_limit_due = self.state.pending_follow_on.as_ref().is_some_and(|owed| {
                    owed.is_turn(&pending.position.turn_id)
                        && owed.chain_depth as usize >= MAX_AGENT_FRAME_SWITCHES
                });
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
                // Work withheld from a terminal checkpoint already announced
                // its start at the boundary that claimed it (FIG-3157), and a
                // follow-on claims nothing.
                announce_queued_work = false;
                let (mut input, next_claims) = self.queued_run_input(selection, false)?;
                // A follow-on runs under the options its switch recorded.
                if !self
                    .state
                    .pending_follow_on
                    .as_ref()
                    .is_some_and(|owed| owed.is_turn(&pending.position.turn_id))
                {
                    input.protocol_turn_options = follow_protocol_turn_options.clone();
                }
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
            // A committed frame switch owes its follow-on, which runs next,
            // in this run, under the id the switch wrote (ADR 0101 §3). The
            // one path for every session: durable or store-less, the fact is
            // on the resident head.
            if let Some(owed) = self.state.pending_follow_on.as_deref().cloned() {
                // A lane that lapsed is never silently reacquired for the
                // follow-on: it stays owed on the head, and the next drive
                // recovers it (ADR 0101 §3).
                if session_execution_lease
                    .as_ref()
                    .is_some_and(SessionExecutionLeaseGuard::is_lost)
                {
                    self.record_follow_on_failure(
                        &mut turns,
                        RuntimeError::new(
                            RuntimeErrorCode::SessionExecutionLeaseLost,
                            format!(
                                "follow-on turn `{}` awaits a live execution lane; the next \
                                 drive recovers it",
                                owed.follow_on_turn_id
                            ),
                        ),
                    );
                    if let Some(withheld) = carried_withheld.take() {
                        self.abandon_withheld_terminal_work(withheld).await;
                    }
                    return Ok(AgentFrameRun {
                        turns,
                        acceptance: None,
                    });
                }
                turn_trace_turn_id = owed.follow_on_turn_id.clone();
                start =
                    LogicalTurnStart::Input(follow_on_input(&owed, follow_turn_context.clone()));
                claims = LogicalTurnClaims::new(Vec::new(), Vec::new());
                continue;
            }
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
            turn_trace_turn_id = next_physical_turn_id(&turn_trace_turn_id)
                .map_err(super::runtime_error_from_store_commit)?;
            let mut input = TurnInput::items(Vec::new());
            input.protocol_turn_options = follow_protocol_turn_options.clone();
            input.turn_context = follow_turn_context.clone();
            claims = LogicalTurnClaims::new(withheld.queued, withheld.turn_inputs);
            announce_queued_work = false;
            start = LogicalTurnStart::Input(input);
        }
    }
}

/// The input of the follow-on `owed`: its task, under the protocol turn
/// options the switch recorded (ADR 0101 §3).
pub(super) fn follow_on_input(
    owed: &crate::store::PendingFollowOn,
    turn_context: crate::TurnContext,
) -> TurnInput {
    let mut input = TurnInput::text(owed.task.clone());
    input.protocol_turn_options = owed.options.as_deref().cloned();
    input.turn_context = turn_context;
    input.trace_turn_id = Some(owed.follow_on_turn_id.clone());
    input
}

/// The next physical turn of the logical run `current` belongs to.
pub(super) fn next_physical_turn_id(current: &TurnId) -> Result<TurnId, crate::StoreError> {
    let (root, index) = crate::store::QueuedRunPosition::split_turn_id(current);
    let next = crate::StoreError::checked_monotonic_increment("physical_turn_index", index)?;
    Ok(crate::store::QueuedRunPosition::derive_turn_id(&root, next))
}

impl LashRuntime {
    /// Whether the head owes `turn_id` as its pending follow-on: such a turn
    /// that ends without committing stays owed, with the input pinned to it,
    /// for the next drive to recover (ADR 0101 §3).
    fn owes_follow_on(&self, turn_id: &TurnId) -> bool {
        self.state
            .pending_follow_on
            .as_ref()
            .is_some_and(|owed| owed.is_turn(turn_id))
    }
}

/// Whether `err` parked its turn on a replay refusal (FIG-3586).
fn parks(err: &crate::RuntimeError) -> bool {
    err.turn_failure_cause() == crate::TurnFailureCause::Parked
}
