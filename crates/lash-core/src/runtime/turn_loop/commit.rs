//! The commit phase: finalize the assembled turn, stage its usage, and drive
//! the head-advancing commit that makes the turn durable.
//!
//! The phase types are consumed in sequence and each transition takes the
//! previous one by value, so a committed turn cannot be adopted twice and
//! post-commit delivery cannot start before adoption.

use super::*;
use crate::TurnId;

/// The attempts of a finished turn whose usage never arrived after an abort
/// or failure, in call order, for the unreported ledger row and later
/// host-invoked reconciliation.
fn unreported_usage_attempts(
    llm_calls: &[crate::LlmCallRecord],
    model: &str,
) -> Vec<crate::runtime::UnreportedUsageAttempt> {
    llm_calls
        .iter()
        .flat_map(|call| {
            call.attempts
                .iter()
                .filter(|attempt| attempt.usage_disposition.is_unreported_after_interruption())
                .map(move |attempt| crate::runtime::UnreportedUsageAttempt {
                    call_id: call.call_id.0.clone(),
                    attempt_ordinal: attempt.ordinal,
                    source: "turn".to_string(),
                    model: model.to_string(),
                    generation_id: attempt
                        .evidence
                        .as_ref()
                        .and_then(|evidence| evidence.provider_response_id.clone()),
                })
        })
        .collect()
}

pub(super) struct TurnFinishInput {
    pub(super) turn_pipeline: TurnBoundary,
    pub(super) assembler: TurnAssembler,
    pub(super) new_messages: crate::MessageSequence,
    pub(super) policy: SessionPolicy,
    pub(super) turn_index: usize,
    pub(super) trace_turn_id: TurnId,
}

struct PreparedTurn {
    turn_pipeline: TurnBoundary,
    turn: AssembledTurn,
    events: Vec<SessionStreamEvent>,
}

/// What the final commit writes: the session it advances, the usage it stages,
/// the claim settlement it carries, and the lease it is fenced by.
struct TurnCommitRequest<'commit> {
    session: Option<&'commit mut Session>,
    staged_usage: session_manager::StagedTokenLedger,
    commit_effects: super::logical_turn::LogicalTurnCommitEffects,
    session_execution_lease: Option<&'commit SessionExecutionLeaseGuard>,
    release_session_execution_lease: bool,
    trace_turn_id: &'commit TurnId,
    recorded_attachment_intent_ids: std::collections::BTreeSet<crate::AttachmentId>,
}

/// The local commit-admission handles: only the head-advancing attempt uses
/// them, and they are dropped when the store needs no admission.
struct TurnCommitAdmission<'admission> {
    cancellation: CancellationToken,
    effect_controller: &'admission dyn crate::RuntimeEffectController,
    turn_phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
}

impl PreparedTurn {
    fn outcome(&self) -> &TurnOutcome {
        &self.turn.outcome
    }

    fn final_operation(&self) -> crate::OperationId {
        self.turn_pipeline.final_operation()
    }

    async fn commit(
        self,
        request: TurnCommitRequest<'_>,
        admission: TurnCommitAdmission<'_>,
    ) -> Result<CommittedTurn, crate::StoreError> {
        let TurnCommitAdmission {
            cancellation,
            effect_controller,
            turn_phase_probe,
        } = admission;
        let has_durable_store = request
            .session
            .as_deref()
            .and_then(Session::history_store)
            .is_some();
        if !has_durable_store
            || !super::commit_admission::requires_local_commit_admission(effect_controller)
        {
            return self.commit_after_admission(request).await;
        }
        let session_id = self.turn_pipeline.state().session_id.clone();
        let work_identity = request.trace_turn_id.to_string();
        super::run_head_advancing_commit_attempt(
            session_id.clone(),
            work_identity.clone(),
            cancellation,
            move |waited, queue_depth| async move {
                super::commit_admission::record_product_commit_admission(
                    "turn_final_commit",
                    &session_id,
                    &work_identity,
                    waited,
                    queue_depth,
                );
                let _product_commit_phase = super::RuntimeNamedPhase::begin(
                    turn_phase_probe,
                    "commit_admission.product_attempt",
                );
                Box::pin(self.commit_after_admission(request)).await
            },
        )
        .await
    }

    async fn commit_after_admission(
        mut self,
        request: TurnCommitRequest<'_>,
    ) -> Result<CommittedTurn, crate::StoreError> {
        let TurnCommitRequest {
            session,
            staged_usage,
            commit_effects,
            session_execution_lease,
            release_session_execution_lease,
            trace_turn_id,
            recorded_attachment_intent_ids,
        } = request;
        let accepted = self
            .turn_pipeline
            .final_commit(
                &mut self.turn,
                session,
                staged_usage.deltas(),
                commit_effects.claim_settlement,
                session_execution_lease.map(|lease| lease.fence().fencing_token),
                commit_effects.enqueued_queue_batches,
                // Any active-turn input that missed the turn's final
                // checkpoint must become the next ordinary user turn.
                Some(trace_turn_id.clone()),
                recorded_attachment_intent_ids,
                release_session_execution_lease
                    .then(|| session_execution_lease.map(SessionExecutionLeaseGuard::completion))
                    .flatten(),
            )
            .await?;
        Ok(CommittedTurn {
            turn: self.turn,
            events: self.events,
            resident_state: self.turn_pipeline.into_final_state(),
            accepted,
            staged_usage,
            release_session_execution_lease,
            retained_lease_continuity: if release_session_execution_lease {
                None
            } else {
                session_execution_lease.and_then(SessionExecutionLeaseGuard::continuity)
            },
        })
    }
}

impl TypedTurnPhase for PreparedTurn {
    const RUNTIME_PHASE: RuntimeTurnPhase = RuntimeTurnPhase::PreparedTurn;
}

struct CommittedTurn {
    turn: AssembledTurn,
    events: Vec<SessionStreamEvent>,
    resident_state: RuntimeSessionState,
    accepted: AcceptedTurnCommit,
    staged_usage: session_manager::StagedTokenLedger,
    release_session_execution_lease: bool,
    retained_lease_continuity: Option<SessionExecutionLeaseContinuity>,
}

impl TypedTurnPhase for CommittedTurn {
    const RUNTIME_PHASE: RuntimeTurnPhase = RuntimeTurnPhase::CommittedTurn;
}

impl CommittedTurn {
    /// Synchronize the accepted durable commit into the resident runtime.
    /// This transition intentionally cannot await; consuming `self` is the
    /// only way to obtain the post-commit delivery phase.
    fn adopt(
        self,
        runtime: &mut LashRuntime,
        trace_turn_id: &TurnId,
        session_execution_lease: Option<&SessionExecutionLeaseGuard>,
    ) -> Result<PostCommitDelivery, crate::StoreError> {
        let (enqueued_queue_batches, confirmed_usage) = self.accepted.into_parts();
        self.staged_usage.confirm_identities(&confirmed_usage)?;
        if self.release_session_execution_lease
            && let Some(lease) = session_execution_lease
        {
            lease.mark_released();
        }
        runtime
            .resident_session
            .retain_committed_lease_continuity(self.retained_lease_continuity);
        runtime.state = self.resident_state;
        let observation_revision = if runtime.state.checkpoint_ref.is_some() {
            runtime.state.head_revision
        } else {
            runtime.state.turn_index as u64
        };
        runtime
            .resident_session
            .record_committed_observation_turn(observation_revision, trace_turn_id);
        Ok(PostCommitDelivery {
            turn: self.turn,
            events: self.events,
            enqueued_queue_batches,
            post_commit_delivery_failed: false,
        })
    }
}

/// What the commit phase needs to settle one physical turn: the assembled turn
/// itself, the claims it must settle, and the lease and control handles the
/// settlement runs under.
pub(in crate::runtime) struct TurnCommitContext<'commit, 'run> {
    pub(in crate::runtime) finish: TurnFinishInput,
    pub(in crate::runtime) claims: &'commit LogicalTurnClaims,
    pub(in crate::runtime) events: &'commit dyn EventSink,
    pub(in crate::runtime) scoped_effect_controller: &'commit ScopedEffectController<'run>,
    pub(in crate::runtime) cancel_state: &'commit CancellationToken,
    pub(in crate::runtime) lease: TurnLeaseScope<'commit>,
    pub(in crate::runtime) turn_control: &'commit ActiveTurnControl,
}

/// The cancellation tail of the execute phase: the driver remainder a cancelled
/// effect loop left behind, handed to the commit phase to settle.
pub(super) struct CancelledTurnFinishContext<'cancel, 'run> {
    pub(super) driver: TurnDriverRemainder,
    pub(super) assembler: TurnAssembler,
    pub(super) cancellation_messages: crate::MessageSequence,
    pub(super) events: &'cancel dyn EventSink,
    pub(super) finish_scoped_effect_controller: &'cancel ScopedEffectController<'run>,
    pub(super) cancel: &'cancel CancellationToken,
    pub(super) lease: TurnLeaseScope<'cancel>,
    pub(super) turn_control: &'cancel ActiveTurnControl,
    pub(super) turn_index: usize,
    pub(super) trace_turn_id: TurnId,
}

/// The terminal turn a logical run commits when it refuses to switch agent
/// frames again.
pub(in crate::runtime) struct LogicalTurnErrorContext<'error, 'run> {
    pub(in crate::runtime) message: String,
    pub(in crate::runtime) trace_turn_id: TurnId,
    pub(in crate::runtime) sinks: TurnSinks<'error>,
    pub(in crate::runtime) scoped_effect_controller: ScopedEffectController<'run>,
    pub(in crate::runtime) cancel: CancellationToken,
    pub(in crate::runtime) claims: LogicalTurnClaims,
    pub(in crate::runtime) session_execution_lease: Option<&'error SessionExecutionLeaseGuard>,
}

impl LashRuntime {
    pub(super) async fn finish_turn(
        &mut self,
        context: TurnCommitContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let TurnCommitContext {
            finish,
            claims,
            events,
            scoped_effect_controller,
            cancel_state,
            lease:
                TurnLeaseScope {
                    guard: session_execution_lease,
                    release_policy: session_execution_lease_release_policy,
                },
            turn_control,
        } = context;
        let turn_control_host = Arc::clone(&self.host.core.control.effect_host);
        let turn_control_binding =
            turn_control_binding(turn_control_host.as_ref(), scoped_effect_controller).await?;
        let turn_control_resolver = match &turn_control_binding {
            crate::TurnControlBinding::HostOwned { resolver, peek: _ }
            | crate::TurnControlBinding::RunScoped {
                resolver,
                durable_cancel_after_llm: _,
            } => *resolver,
        };
        let TurnFinishInput {
            mut turn_pipeline,
            assembler,
            new_messages,
            policy,
            turn_index,
            trace_turn_id,
        } = finish;
        turn_pipeline.state_mut().policy = self.state.effective_policy().clone();
        turn_pipeline.state_mut().turn_index = turn_index;

        if !assembler.token_usage.is_zero() {
            session_manager::record_token_usage_shared(
                &self.shared_token_ledger,
                "turn",
                &policy.model.id,
                &assembler.token_usage,
            );
        }
        // ADR 0031: an attempt the host aborted or that failed before the
        // provider's usage arrived was still billed. Write the hole as a typed
        // unreported row (even at zero usage) and remember the attempt so a
        // host can reconcile it later; a turn with no interruption and no
        // usage still writes nothing.
        let unreported = unreported_usage_attempts(&assembler.llm_calls, &policy.model.id);
        if !unreported.is_empty() {
            let descriptors = unreported
                .iter()
                .map(|attempt| crate::UnreportedLedgerAttempt {
                    call_id: attempt.call_id.clone(),
                    attempt_ordinal: attempt.attempt_ordinal,
                    generation_id: attempt.generation_id.clone(),
                })
                .collect::<Vec<_>>();
            session_manager::record_unreported_attempts_shared(
                &self.shared_token_ledger,
                "turn",
                &policy.model.id,
                &descriptors,
            );
            self.unreported_usage_attempts.extend(unreported);
        }

        // The evidence the executed turn already named travels into the gate,
        // so the request id a host saw on the streamed outcome is the one the
        // committed report carries. Only a cancellation with no assembled
        // evidence at all falls back to lash's internal mint.
        let assembled_cancellation = match &assembler.outcome {
            Some(TurnOutcome::Stopped(TurnStop::Cancelled { evidence })) => Some(evidence.clone()),
            _ => None,
        };
        let assembled_cancelled = assembled_cancellation.is_some();
        let lease_was_lost = session_execution_lease.is_some_and(|lease| lease.is_lost());
        if lease_was_lost && cancel_state.is_cancelled() && !assembled_cancelled {
            return Err(RuntimeError::new(
                RuntimeErrorCode::SessionExecutionLeaseLost,
                "session execution lease was lost while the turn was active",
            ));
        }
        let cancellation = turn_control
            .settle_before_commit(
                turn_control_resolver,
                assembled_cancelled || (cancel_state.is_cancelled() && !lease_was_lost),
                assembled_cancellation,
            )
            .await?;
        if let Some(evidence) = cancellation.as_ref()
            && let Some(store) = self.session.as_ref().and_then(Session::history_store)
        {
            store
                .record_turn_cancel_request(crate::TurnCancelRequest {
                    address: crate::TurnAddress::new(&self.state.session_id, &trace_turn_id),
                    request_id: evidence.request_id.clone(),
                    origin: evidence.origin.clone(),
                    reason: evidence.reason.clone(),
                    undelivered: evidence.undelivered,
                    mode: evidence.mode,
                })
                .await
                .map_err(|err| {
                    RuntimeError::new(crate::RuntimeErrorCode::RuntimeStore, err.to_string())
                })?;
        }
        if cancellation.is_some() {
            cancel_state.cancel();
        }
        // Interruption derives from sealed evidence, never the raw token: a
        // lease-loss wakeup cancels the token without evidence and must not
        // become a Cancelled outcome. When a durable cancel races lease loss,
        // the final commit's head CAS and any claim batch-ownership checks are
        // the arbiters. Lease loss alone does not reject a current-head commit.
        let interrupted = cancellation.is_some();

        turn_pipeline.finalize_turn_read_state(new_messages, interrupted);
        for diagnostic in turn_pipeline.take_projection_diagnostics() {
            crate::trace::emit_trace(
                &self.host.core.tracing.trace_sink,
                &self.host.core.tracing.trace_context,
                lash_trace::TraceContext::default()
                    .for_session(self.state.session_id.clone())
                    .for_turn_index(turn_index)
                    .for_turn(trace_turn_id.clone()),
                lash_trace::TraceEvent::Custom {
                    name: "session_graph.read_projection".to_string(),
                    payload: serde_json::json!({
                        "durably_appended_messages": diagnostic.durably_appended_messages,
                        "observation_only_messages": diagnostic.observation_only_messages,
                        "id_mismatch_message_ids": diagnostic.id_mismatches,
                    }),
                },
                self.host.core.clock.as_ref(),
            );
        }
        if !assembler.token_usage.is_zero() {
            turn_pipeline.state_mut().token_usage = assembler.token_usage.clone();
        }

        let last_prompt_usage = assembler.last_llm_usage().and_then(normalize_prompt_usage);
        turn_pipeline.state_mut().last_prompt_usage = last_prompt_usage;
        let assembled_state = turn_pipeline.export_state_for_assembly();
        let assembled = assembler.finish(
            assembled_state,
            cancellation,
            None,
            &self.host.core.control.termination,
        );

        let Some(session) = self.session.as_ref() else {
            self.state.apply_snapshot(&assembled.state);
            let observation_revision = if self.state.checkpoint_ref.is_some() {
                self.state.head_revision
            } else {
                self.state.turn_index as u64
            };
            self.resident_session
                .record_committed_observation_turn(observation_revision, &trace_turn_id);
            self.emit_completed_turn_trace(&assembled.state, &assembled.outcome, &trace_turn_id);
            publish_terminal_after_commit(
                turn_control,
                turn_control_resolver,
                &TurnTerminal::Committed {
                    outcome: assembled.outcome.clone(),
                    session_revision: None,
                },
                &self.state.session_id,
                &trace_turn_id,
            )
            .await;
            return Ok(PhysicalTurnExecution {
                turn: assembled,
                enqueued_queue_batches: Vec::new(),
                post_commit_delivery_failed: false,
            });
        };

        let plugins = Arc::clone(session.plugins());
        let manager = match self.runtime_session_services_for_turn(
            None,
            session_execution_lease,
            turn_pipeline.graph_appends(),
        ) {
            Ok(manager) => manager,
            Err(err) => {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::PluginSessionManager,
                    err.to_string(),
                ));
            }
        };

        self.mark_phase_begin(PreparedTurn::RUNTIME_PHASE);
        let finalized = match plugins
            .finalize_turn_with_phase_probe(
                assembled,
                manager.state_service(),
                manager.lifecycle_service(),
                manager.graph_service(),
                self.turn_phase_probe.clone(),
                &trace_turn_id,
            )
            .await
        {
            Ok(finalized) => finalized,
            Err(err) => {
                self.mark_phase_end(PreparedTurn::RUNTIME_PHASE);
                return Err(RuntimeError::new(
                    RuntimeErrorCode::PluginFinalizeTurn,
                    err.to_string(),
                ));
            }
        };
        let returned_turn = finalized.turn;
        let prepared = PreparedTurn {
            turn_pipeline,
            turn: returned_turn,
            events: finalized.events,
        };
        let release_session_execution_lease =
            session_execution_lease_release_policy.should_release(prepared.outcome());
        let commit_effects = claims.commit_effects(
            prepared.outcome(),
            &self.state.session_id,
            &trace_turn_id,
            Some(self.state.effective_protocol_turn_options().clone()),
        );
        let queued_work_completion_trace =
            commit_effects.claim_settlement.queued.completions.clone();
        let turn_input_completion_trace = commit_effects
            .claim_settlement
            .turn_inputs
            .completions
            .clone();
        let staged_usage = match session_manager::stage_token_ledger_shared(
            &self.shared_token_ledger,
            &prepared.final_operation(),
        ) {
            Ok(staged_usage) => staged_usage,
            Err(err) => {
                self.mark_phase_end(PreparedTurn::RUNTIME_PHASE);
                return Err(runtime_error_from_store_commit(err));
            }
        };
        let committed = match Box::pin(
            prepared.commit(
                TurnCommitRequest {
                    session: self.session.as_mut(),
                    staged_usage,
                    commit_effects,
                    session_execution_lease,
                    release_session_execution_lease,
                    trace_turn_id: &trace_turn_id,
                    recorded_attachment_intent_ids: self
                        .host
                        .core
                        .durability
                        .attachment_store
                        .recorded_turn_intent_ids(&trace_turn_id),
                },
                TurnCommitAdmission {
                    cancellation: cancel_state.clone(),
                    effect_controller: scoped_effect_controller.controller(),
                    turn_phase_probe: self.turn_phase_probe.clone(),
                },
            ),
        )
        .await
        {
            Ok(committed) => committed,
            Err(err) => {
                crate::trace::emit_store_error(
                    &self.host.core.tracing.trace_sink,
                    &self.host.core.tracing.trace_context,
                    lash_trace::TraceContext::default()
                        .for_session(self.state.session_id.clone())
                        .for_turn(trace_turn_id.clone()),
                    "turn_commit",
                    &err,
                    self.host.core.clock.as_ref(),
                );
                // Reported here, not inside the commit: the guard reference and the
                // claimant are already live in this future, so naming the writer
                // costs nothing, while carrying evidence through the commit await
                // would grow every turn future.
                trace_commit_cas_rejected(
                    &self.state.session_id,
                    session_execution_lease
                        .map(SessionExecutionLeaseGuard::commit_evidence)
                        .as_deref(),
                    &self.runtime_lease_owner,
                    &self.runtime_lease_executor_id,
                    &err,
                );
                self.mark_phase_end(PreparedTurn::RUNTIME_PHASE);
                return Err(runtime_error_from_store_commit(err));
            }
        };
        self.mark_phase_end(PreparedTurn::RUNTIME_PHASE);
        self.mark_phase_begin(CommittedTurn::RUNTIME_PHASE);
        let mut delivery = committed
            .adopt(self, &trace_turn_id, session_execution_lease)
            .map_err(runtime_error_from_store_commit)?;
        self.mark_phase_end(CommittedTurn::RUNTIME_PHASE);
        self.mark_phase_begin(PostCommitDelivery::RUNTIME_PHASE);

        emit_session_events_to_sink(events, delivery.events).await;
        publish_terminal_after_commit(
            turn_control,
            turn_control_resolver,
            &TurnTerminal::Committed {
                outcome: delivery.turn.outcome.clone(),
                session_revision: None,
            },
            &self.state.session_id,
            &trace_turn_id,
        )
        .await;
        if matches!(delivery.turn.outcome, TurnOutcome::AgentFrameSwitch { .. })
            && let Some(session) = self.session.as_mut()
        {
            let protocol_session = Arc::clone(session.plugins().protocol_session());
            let session_id = self.state.session_id.clone();
            if let Err(err) = protocol_session
                .restore_session(
                    crate::plugin::ProtocolSessionContext::new(session, &session_id),
                    crate::plugin::ProtocolSessionRestoreView::new(&self.state),
                )
                .await
            {
                delivery.turn.errors.push(post_commit_delivery_issue(
                    "protocol_restore_session",
                    err.to_string(),
                ));
                delivery.post_commit_delivery_failed = true;
                self.invalidate_resident_session_state();
            }
        }
        if !queued_work_completion_trace.is_empty() {
            crate::trace::emit_trace(
                &self.host.core.tracing.trace_sink,
                &self.host.core.tracing.trace_context,
                lash_trace::TraceContext::default()
                    .for_session(delivery.turn.state.session_id.clone())
                    .for_turn_index(delivery.turn.state.turn_index)
                    .for_turn(trace_turn_id.clone()),
                lash_trace::TraceEvent::Custom {
                    name: "queued_work.completed".to_string(),
                    payload: queued_work_completion_trace_payload(&queued_work_completion_trace),
                },
                self.host.core.clock.as_ref(),
            );
        }
        if !turn_input_completion_trace.is_empty() {
            crate::trace::emit_trace(
                &self.host.core.tracing.trace_sink,
                &self.host.core.tracing.trace_context,
                lash_trace::TraceContext::default()
                    .for_session(delivery.turn.state.session_id.clone())
                    .for_turn_index(delivery.turn.state.turn_index)
                    .for_turn(trace_turn_id.clone()),
                lash_trace::TraceEvent::Custom {
                    name: "turn_input.completed".to_string(),
                    payload: turn_input_completion_trace_payload(&turn_input_completion_trace),
                },
                self.host.core.clock.as_ref(),
            );
        }
        // A final physical turn has already atomically released its lane, so
        // TurnPersisted observers are genuinely lane-less. Agent-frame
        // switches retain the guard and their observers must borrow it.
        let post_commit_session_execution_lease = if release_session_execution_lease {
            None
        } else {
            session_execution_lease
        };
        match self
            .emit_turn_persisted_event(
                &delivery.turn,
                scoped_effect_controller,
                &trace_turn_id,
                post_commit_session_execution_lease,
            )
            .await
        {
            Ok(Some(error)) => {
                let mut issue = crate::plugin::plugin_lifecycle_hook_issue(error);
                issue.retryable = Some(false);
                delivery.turn.errors.push(issue);
                delivery.post_commit_delivery_failed = true;
                self.invalidate_resident_session_state();
            }
            Ok(None) => {}
            Err(err) => {
                delivery
                    .turn
                    .errors
                    .push(post_commit_delivery_issue(err.code.as_str(), err.message));
                delivery.post_commit_delivery_failed = true;
                self.invalidate_resident_session_state();
            }
        }
        self.mark_phase_end(PostCommitDelivery::RUNTIME_PHASE);

        self.emit_completed_turn_trace(
            &delivery.turn.state,
            &delivery.turn.outcome,
            &trace_turn_id,
        );
        Ok(PhysicalTurnExecution {
            turn: delivery.turn,
            enqueued_queue_batches: delivery.enqueued_queue_batches,
            post_commit_delivery_failed: delivery.post_commit_delivery_failed,
        })
    }

    pub(super) async fn finish_cancelled_turn_after_effect_abort(
        &mut self,
        context: CancelledTurnFinishContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let CancelledTurnFinishContext {
            driver,
            mut assembler,
            cancellation_messages,
            events,
            finish_scoped_effect_controller,
            cancel,
            lease:
                TurnLeaseScope {
                    guard: session_execution_lease,
                    release_policy: session_execution_lease_release_policy,
                },
            turn_control,
            turn_index,
            trace_turn_id,
        } = context;
        let TurnDriverRemainder {
            policy,
            turn_pipeline,
            pending_queue_claims,
            pending_turn_input_claims,
            ..
        } = driver;
        emit_terminal_sequence(
            &mut assembler,
            events,
            None,
            TurnStop::Cancelled {
                evidence: turn_control.evidence_or_internal(),
            },
        )
        .await;
        let claims = LogicalTurnClaims::new(pending_queue_claims, pending_turn_input_claims);
        Box::pin(self.finish_turn(TurnCommitContext {
            finish: TurnFinishInput {
                turn_pipeline,
                assembler,
                new_messages: cancellation_messages,
                policy: policy.policy,
                turn_index,
                trace_turn_id,
            },
            claims: &claims,
            events,
            scoped_effect_controller: finish_scoped_effect_controller,
            cancel_state: cancel,
            lease: TurnLeaseScope {
                guard: session_execution_lease,
                release_policy: session_execution_lease_release_policy,
            },
            turn_control,
        }))
        .await
    }

    fn emit_completed_turn_trace(
        &self,
        state: &SessionSnapshot,
        outcome: &TurnOutcome,
        trace_turn_id: &TurnId,
    ) {
        if self.host.core.tracing.trace_sink.is_none() {
            return;
        }

        let trace_outcome = trace_outcome(outcome);
        crate::trace::emit_trace(
            &self.host.core.tracing.trace_sink,
            &self.host.core.tracing.trace_context,
            lash_trace::TraceContext::default()
                .for_session(state.session_id.clone())
                .for_turn_index(state.turn_index)
                .for_turn(trace_turn_id.to_string()),
            lash_trace::TraceEvent::TurnCompleted {
                outcome: trace_outcome,
            },
            self.host.core.clock.as_ref(),
        );
    }

    pub(in crate::runtime) async fn finish_logical_turn_error(
        &mut self,
        context: LogicalTurnErrorContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let LogicalTurnErrorContext {
            message,
            trace_turn_id,
            sinks: TurnSinks {
                events,
                turn_events,
            },
            scoped_effect_controller,
            cancel,
            claims,
            session_execution_lease,
        } = context;
        let turn_control_host = Arc::clone(&self.host.core.control.effect_host);
        let turn_control_binding =
            turn_control_binding(turn_control_host.as_ref(), &scoped_effect_controller).await?;
        let turn_control_resolver = match &turn_control_binding {
            crate::TurnControlBinding::HostOwned { resolver, peek: _ }
            | crate::TurnControlBinding::RunScoped {
                resolver,
                durable_cancel_after_llm: _,
            } => *resolver,
        };
        let turn_control = Arc::new(
            ActiveTurnControl::new(
                turn_control_resolver,
                TurnAddress::new(&self.state.session_id, &trace_turn_id),
            )
            .await?,
        );
        let mut assembler = TurnAssembler::default();
        emit_terminal_sequence(
            &mut assembler,
            events,
            Some(TerminalDiagnostic {
                kind: TerminalDiagnosticKind::Runtime,
                code: Some("agent_frame_switch_limit".to_string()),
                message,
                retryable: Some(false),
                activity: TerminalActivityTarget::UnscopedSink {
                    sink: turn_events,
                    turn_id: &trace_turn_id,
                },
            }),
            TurnStop::RuntimeError,
        )
        .await;

        let messages = crate::MessageSequence::from_base(self.state.read_model().messages);
        let mut turn_pipeline = TurnBoundary::from_state_with_clock(
            self.state.clone(),
            Arc::clone(&self.host.core.clock),
            self.state.turn_scope(&trace_turn_id),
            self.host.core.durability.commit_budget,
        );
        turn_pipeline.apply_prepared_messages(&messages);
        let finish_result = Box::pin(self.finish_turn(TurnCommitContext {
            finish: TurnFinishInput {
                turn_pipeline,
                assembler,
                new_messages: messages,
                policy: self.state.effective_policy().clone(),
                // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                turn_index: self.state.turn_index + 1,
                trace_turn_id,
            },
            claims: &claims,
            events,
            scoped_effect_controller: &scoped_effect_controller,
            cancel_state: &cancel,
            lease: TurnLeaseScope {
                guard: session_execution_lease,
                release_policy: SessionExecutionLeaseReleasePolicy::KeepOnAgentFrameSwitch,
            },
            turn_control: &turn_control,
        }))
        .await;
        if let Err(err) = &finish_result {
            self.abandon_queued_work_claims_after_local_abort(err, &claims.queued)
                .await;
            self.abandon_turn_input_claims_after_local_abort(err, &claims.turn_inputs)
                .await;
        }
        finish_result
    }
}
