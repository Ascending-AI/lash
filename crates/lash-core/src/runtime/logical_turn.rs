use super::turn_input_ingress::TurnInputDrive;
use super::turn_loop::{SessionExecutionLeaseReleasePolicy, TurnStopwatch};
use super::*;
use crate::facade_support::RuntimeSessionStateFacadeOps;

pub(super) const MAX_AGENT_FRAME_SWITCHES: usize = 16;

pub(super) struct PhysicalTurnExecution {
    pub(super) turn: AssembledTurn,
    pub(super) enqueued_queue_batches: Vec<crate::QueuedWorkBatch>,
    pub(super) post_commit_delivery_failed: bool,
}

pub(super) struct LogicalTurnClaims {
    pub(super) queued: Vec<crate::QueuedWorkClaim>,
    /// The turn-input rows this turn drives, each with the authority it will
    /// settle under: a generation-fenced claim, or none at all when the turn
    /// accepted the row itself and settles it at the head CAS (ADR 0069 §5).
    pub(super) turn_inputs: Vec<TurnInputDrive>,
}

impl LogicalTurnClaims {
    pub(super) fn new(
        queued: Vec<crate::QueuedWorkClaim>,
        turn_inputs: Vec<TurnInputDrive>,
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
        let queue_claim_generations = self
            .queued
            .iter()
            .map(|claim| (claim.claim_id.clone(), claim.session_lease_generation))
            .collect();
        // Only a claimed drive has a generation, so only a claimed drive can be
        // superseded by a later one and have its settlement dropped and
        // retried. An unclaimed settlement retires at its first lost head CAS.
        let turn_input_claim_generations = self
            .turn_inputs
            .iter()
            .filter_map(TurnInputDrive::claim_generation)
            .collect();
        let enqueued_queue_batches = match outcome {
            TurnOutcome::AgentFrameSwitch {
                frame_key, task, ..
            } if claimed => {
                vec![
                    crate::QueuedWorkBatchDraft::new(
                        session_id,
                        crate::DeliveryPolicy::AfterCurrentTurnCommit,
                        vec![crate::QueuedWorkPayload::agent_frame_task(
                            crate::session_graph::frame_node_id(session_id, frame_key.as_str()),
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
            queue_claim_generations,
            turn_input_claim_generations,
            enqueued_queue_batches,
        }
    }
}

pub(super) struct LogicalTurnCommitEffects {
    pub(super) originating_queue_claims: Vec<crate::QueuedWorkCompletion>,
    pub(super) originating_turn_input_claims: Vec<crate::TurnInputCompletion>,
    pub(super) completed_queue_claims: Vec<crate::QueuedWorkCompletion>,
    pub(super) completed_turn_input_claims: Vec<crate::TurnInputCompletion>,
    pub(super) queue_claim_generations: std::collections::HashMap<String, u64>,
    pub(super) turn_input_claim_generations: std::collections::HashMap<String, u64>,
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
    async fn emit_physical_turn_start(
        turn_events: &dyn TurnActivitySink,
        turn_id: &str,
        claims: &LogicalTurnClaims,
    ) {
        super::turn_loop::emit_turn_started_to_sink(turn_events, turn_id).await;
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

    fn record_follow_on_failure(&mut self, turns: &mut [AssembledTurn], err: RuntimeError) {
        self.invalidate_resident_session_state();
        turns
            .last_mut()
            .expect("a follow-on failure requires an earlier committed turn")
            .errors
            .push(super::turn_loop::post_commit_delivery_issue(
                err.code.as_str(),
                err.message,
            ));
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
        let (follow_protocol_turn_options, follow_turn_context, supplied_trace_turn_id) =
            start.continuation_state();
        let root_trace_turn_id = if supplied_trace_turn_id.is_empty() {
            scoped_effect_controller.scope_id().to_string()
        } else {
            supplied_trace_turn_id
        };
        let mut turns: Vec<AssembledTurn> = Vec::new();

        loop {
            let turn_trace_turn_id = agent_frame_follow_turn_id(&root_trace_turn_id, turns.len());
            let turn_effect_controller = if turns.is_empty() {
                scoped_effect_controller.clone()
            } else {
                match ScopedEffectController::borrowed(
                    scoped_effect_controller.controller(),
                    self.state.turn_scope(&turn_trace_turn_id),
                ) {
                    Ok(controller) => controller,
                    // FIG-1573 exempt: this follow-on turn never started, so no
                    // input can be pinned to `turn_trace_turn_id` - a host can
                    // only route into a turn it has observed running. The turn
                    // that *did* run reached its commit, which carried its own
                    // re-defer.
                    Err(err) => {
                        self.invalidate_resident_session_state();
                        turns
                            .last_mut()
                            .expect("a follow-on scope is created only after a committed turn")
                            .errors
                            .push(super::turn_loop::post_commit_delivery_issue(
                                err.code.as_str(),
                                err.message,
                            ));
                        return Ok(AgentFrameRun {
                            turns,
                            acceptance: None,
                        });
                    }
                }
            };
            let frame_stopwatch = if turns.is_empty() {
                stopwatch
            } else {
                TurnStopwatch::start(self.host.core.clock.as_ref())
            };
            Self::emit_physical_turn_start(turn_events, &turn_trace_turn_id, &claims).await;
            let execution_result = match start {
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
                        session_execution_lease.as_ref(),
                        SessionExecutionLeaseReleasePolicy::KeepOnAgentFrameSwitch,
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
                        session_execution_lease.as_ref(),
                        SessionExecutionLeaseReleasePolicy::KeepOnAgentFrameSwitch,
                    ))
                    .await
                }
            };
            let execution = match execution_result {
                Ok(execution) => execution,
                // FIG-1573: this frame ended without reaching a commit, so the
                // commit-time re-defer never ran. Inputs routed into it while it
                // was live are pinned to a turn id no later turn will ever carry
                // again, so the teardown owes them the same repair. Both ids are
                // dead by construction here, which keeps the live-turn hazard out.
                Err(err) if turns.is_empty() => {
                    self.defer_orphaned_turn_inputs_after_teardown(
                        &turn_trace_turn_id,
                        session_execution_lease
                            .as_ref()
                            .map(|lease| lease.fence())
                            .as_ref(),
                    )
                    .await;
                    return Err(err);
                }
                Err(err) => {
                    self.defer_orphaned_turn_inputs_after_teardown(
                        &turn_trace_turn_id,
                        session_execution_lease
                            .as_ref()
                            .map(|lease| lease.fence())
                            .as_ref(),
                    )
                    .await;
                    self.record_follow_on_failure(&mut turns, err);
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
            } = execution;
            frame_stopwatch.stamp(&mut turn, self.host.core.clock.as_ref());
            let switched_frame = match &turn.outcome {
                TurnOutcome::AgentFrameSwitch {
                    frame_key, task, ..
                } => Some((frame_key.clone(), task.clone())),
                _ => None,
            };
            turns.push(turn);
            if post_commit_delivery_failed {
                return Ok(AgentFrameRun {
                    turns,
                    acceptance: None,
                });
            }
            let Some((frame_key, task)) = switched_frame else {
                return Ok(AgentFrameRun {
                    turns,
                    acceptance: None,
                });
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
                let terminal_effect_controller = match ScopedEffectController::borrowed(
                    scoped_effect_controller.controller(),
                    self.state.turn_scope(&terminal_trace_turn_id),
                ) {
                    Ok(controller) => controller,
                    Err(err) => {
                        self.record_follow_on_failure(&mut turns, err);
                        return Ok(AgentFrameRun {
                            turns,
                            acceptance: None,
                        });
                    }
                };
                let terminal_stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
                Self::emit_physical_turn_start(turn_events, &terminal_trace_turn_id, &next_claims)
                    .await;
                let terminal_result = Box::pin(self.finish_logical_turn_error(
                        format!(
                            "logical turn exceeded the limit of {MAX_AGENT_FRAME_SWITCHES} agent frame switches"
                        ),
                        terminal_trace_turn_id,
                        events,
                        turn_events,
                        terminal_effect_controller,
                        cancel.clone(),
                        next_claims,
                        session_execution_lease.as_ref(),
                    ))
                    .await;
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
    root_turn_id: &str,
    completed_turn_count: usize,
) -> String {
    if completed_turn_count == 0 {
        root_turn_id.to_string()
    } else {
        format!("{root_turn_id}:agent-frame:{completed_turn_count}")
    }
}
