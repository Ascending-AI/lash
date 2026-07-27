use super::turn_loop::{SessionExecutionLeaseReleasePolicy, TurnStopwatch};
use super::*;

pub(super) const MAX_AGENT_FRAME_SWITCHES: usize = 16;

pub(super) struct PhysicalTurnExecution {
    pub(super) turn: AssembledTurn,
    pub(super) enqueued_queue_batches: Vec<crate::QueuedWorkBatch>,
}

pub(super) struct LogicalTurnClaims {
    pub(super) queued: Vec<crate::QueuedWorkClaim>,
    pub(super) turn_inputs: Vec<crate::TurnInputClaim>,
}

impl LogicalTurnClaims {
    pub(super) fn new(
        queued: Vec<crate::QueuedWorkClaim>,
        turn_inputs: Vec<crate::TurnInputClaim>,
    ) -> Self {
        Self {
            queued,
            turn_inputs,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.queued.is_empty() && self.turn_inputs.is_empty()
    }

    pub(super) fn commit_effects(
        &self,
        outcome: &TurnOutcome,
        session_id: &str,
        turn_id: &str,
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
        let originating_queue_claims = completed_queue_claims.clone();
        let originating_turn_input_claims = completed_turn_input_claims.clone();
        let enqueued_queue_batches = match outcome {
            TurnOutcome::AgentFrameSwitch { frame_id, task, .. } if claimed => {
                vec![
                    crate::QueuedWorkBatchDraft::new(
                        session_id,
                        crate::DeliveryPolicy::AfterCurrentTurnCommit,
                        crate::SlotPolicy::Exclusive,
                        vec![crate::QueuedWorkPayload::agent_frame_task(
                            frame_id.clone(),
                            task.clone(),
                            protocol_turn_options,
                        )],
                    )
                    .with_source_key(format!("agent-frame-handoff:{turn_id}")),
                ]
            }
            _ => Vec::new(),
        };
        LogicalTurnCommitEffects {
            originating_queue_claims,
            originating_turn_input_claims,
            completed_queue_claims,
            completed_turn_input_claims,
            enqueued_queue_batches,
        }
    }
}

pub(super) struct LogicalTurnCommitEffects {
    pub(super) originating_queue_claims: Vec<crate::QueuedWorkCompletion>,
    pub(super) originating_turn_input_claims: Vec<crate::TurnInputCompletion>,
    pub(super) completed_queue_claims: Vec<crate::QueuedWorkCompletion>,
    pub(super) completed_turn_input_claims: Vec<crate::TurnInputCompletion>,
    pub(super) enqueued_queue_batches: Vec<crate::QueuedWorkBatchDraft>,
}

pub(super) struct PreparedLogicalTurn {
    pub(super) messages: crate::MessageSequence,
    pub(super) previous_prompt_usage: Option<PromptUsage>,
    pub(super) protocol_turn_options: Option<crate::ProtocolTurnOptions>,
    pub(super) protocol_extension: Option<crate::ProtocolTurnExtensionHandle>,
    pub(super) turn_context: crate::TurnContext,
    pub(super) initial_turn_causes: Vec<crate::TurnCause>,
    pub(super) trace_turn_id: String,
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
        String,
    ) {
        match self {
            Self::Input(input) => (
                input.protocol_turn_options.clone(),
                input.turn_context.clone(),
                input.trace_turn_id.clone().unwrap_or_default(),
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
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn drive_logical_turn(
        &mut self,
        mut start: LogicalTurnStart,
        events: &dyn EventSink,
        turn_events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
        cancel: CancellationToken,
        mut claims: LogicalTurnClaims,
        session_execution_lease: Option<&SessionExecutionLeaseGuard>,
        stopwatch: TurnStopwatch,
    ) -> Result<AgentFrameRun, RuntimeError> {
        let (follow_protocol_turn_options, follow_turn_context, supplied_trace_turn_id) =
            start.continuation_state();
        let root_trace_turn_id = if supplied_trace_turn_id.is_empty() {
            scoped_effect_controller.scope_id().to_string()
        } else {
            supplied_trace_turn_id
        };
        let mut turns = Vec::new();

        loop {
            let turn_trace_turn_id = agent_frame_follow_turn_id(&root_trace_turn_id, turns.len());
            let turn_effect_controller = if turns.is_empty() {
                scoped_effect_controller.clone()
            } else {
                ScopedEffectController::borrowed(
                    scoped_effect_controller.controller(),
                    ExecutionScope::turn_incarnation(
                        &self.state.session_id,
                        self.state.incarnation_id.clone(),
                        &turn_trace_turn_id,
                    ),
                )?
            };
            let frame_stopwatch = if turns.is_empty() {
                stopwatch
            } else {
                TurnStopwatch::start(self.host.core.clock.as_ref())
            };
            let execution = match start {
                LogicalTurnStart::Input(mut input) => {
                    input.trace_turn_id = Some(turn_trace_turn_id.clone());
                    Box::pin(self.stream_turn_with_scoped_effect_controller_inner(
                        input,
                        events,
                        turn_events,
                        turn_effect_controller,
                        cancel.clone(),
                        claims.queued,
                        claims.turn_inputs,
                        true,
                        session_execution_lease,
                        SessionExecutionLeaseReleasePolicy::KeepOnAgentFrameSwitch,
                    ))
                    .await?
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
                    Box::pin(self.stream_prepared_turn_inner(
                        prepared.messages,
                        prepared.previous_prompt_usage,
                        prepared.protocol_turn_options,
                        prepared.protocol_extension,
                        prepared.turn_context,
                        prepared.initial_turn_causes,
                        prepared.trace_turn_id,
                        prepared.turn_index,
                        events,
                        turn_events,
                        turn_effect_controller,
                        cancel.clone(),
                        claims.queued,
                        claims.turn_inputs,
                        session_execution_lease,
                        SessionExecutionLeaseReleasePolicy::KeepOnAgentFrameSwitch,
                    ))
                    .await?
                }
            };
            let PhysicalTurnExecution {
                mut turn,
                enqueued_queue_batches,
            } = execution;
            frame_stopwatch.stamp(&mut turn, self.host.core.clock.as_ref());
            let switched_frame = match &turn.outcome {
                TurnOutcome::AgentFrameSwitch { frame_id, task, .. } => {
                    Some((frame_id.clone(), task.clone()))
                }
                _ => None,
            };
            turns.push(turn);
            let Some((frame_id, task)) = switched_frame else {
                return Ok(AgentFrameRun { turns });
            };

            let (mut input, next_claims) = if enqueued_queue_batches.is_empty() {
                let mut input = turn_input_from_text(task);
                input.protocol_turn_options = follow_protocol_turn_options.clone();
                input.turn_context = follow_turn_context.clone();
                (input, LogicalTurnClaims::new(Vec::new(), Vec::new()))
            } else {
                let lease = session_execution_lease.ok_or_else(|| {
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
                let claim = store
                    .claim_ready_queued_work_by_batch_ids(
                        &self.state.session_id,
                        &lease.fence(),
                        &self.runtime_lease_owner,
                        crate::QueuedWorkClaimBoundary::Idle,
                        &batch_ids,
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
                            } if target == &frame_id
                        )
                    })
                });
                if !target_matches {
                    return Err(RuntimeError::new(
                        RuntimeErrorCode::StoreCommitFailed,
                        format!("agent-frame handoff did not target frame `{frame_id}`"),
                    ));
                }
                let materialized = claim.materialize_for_turn();
                let follow_turn_id = agent_frame_follow_turn_id(&root_trace_turn_id, turns.len());
                crate::trace::emit_trace(
                    &self.host.core.tracing.trace_sink,
                    &self.host.core.tracing.trace_context,
                    lash_trace::TraceContext::default()
                        .for_session(self.state.session_id.clone())
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
                (
                    materialized.input,
                    LogicalTurnClaims::new(vec![claim], Vec::new()),
                )
            };
            input.protocol_turn_options = follow_protocol_turn_options.clone();
            input.turn_context = follow_turn_context.clone();

            if turns.len() >= MAX_AGENT_FRAME_SWITCHES {
                let terminal_trace_turn_id =
                    agent_frame_follow_turn_id(&root_trace_turn_id, turns.len());
                let terminal_effect_controller = ScopedEffectController::borrowed(
                    scoped_effect_controller.controller(),
                    ExecutionScope::turn_incarnation(
                        &self.state.session_id,
                        self.state.incarnation_id.clone(),
                        &terminal_trace_turn_id,
                    ),
                )?;
                let terminal_stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
                let mut terminal = self
                    .finish_logical_turn_error(
                        format!(
                            "logical turn exceeded the limit of {MAX_AGENT_FRAME_SWITCHES} agent frame switches"
                        ),
                        terminal_trace_turn_id,
                        events,
                        turn_events,
                        terminal_effect_controller,
                        cancel.clone(),
                        next_claims,
                        session_execution_lease,
                    )
                    .await?;
                terminal_stopwatch.stamp(&mut terminal.turn, self.host.core.clock.as_ref());
                turns.push(terminal.turn);
                return Ok(AgentFrameRun { turns });
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
    root_turn_id: &str,
    completed_turn_count: usize,
) -> String {
    if completed_turn_count == 0 {
        root_turn_id.to_string()
    } else {
        format!("{root_turn_id}:agent-frame:{completed_turn_count}")
    }
}
