//! The commit phase: finalize the assembled turn and execute the head-advancing
//! commit that makes the turn durable. The commit carries no usage: every
//! model call the turn made was delivered by its own usage meter (ADR 0125).
//!
//! The phase types are consumed in sequence and each transition takes the
//! previous one by value, so a committed turn cannot be adopted twice and
//! post-commit delivery cannot start before adoption.

use super::*;
use crate::TurnId;

/// Trace a final commit whose head compare-and-set the store rejected,
/// naming the runtime that attempted it.
fn trace_commit_cas_rejected(
    session_id: &crate::SessionId,
    writer: &crate::LeaseOwnerIdentity,
    executor_id: &str,
    error: &crate::StoreError,
) {
    let crate::StoreError::HeadRevisionConflict { expected, actual } = error else {
        return;
    };
    tracing::warn!(
        session_id = %session_id,
        owner_id = %writer.owner_id,
        incarnation_id = %writer.incarnation_id,
        executor_id,
        expected_head_revision = expected,
        actual_head_revision = actual,
        event = "session_head.commit_cas_rejected",
        "the commit's head compare-and-set was rejected"
    );
}

/// Select the exact closure operation a recovered turn must finish.
///
/// A successor shift may settle and consume this persisted operation, but may
/// not replace it with an authorization carrying its new shift epoch. The
/// binding and admitted physical scope remain part of the authorization being
/// adopted, so recovery cannot broaden the original authority.
pub(super) fn recovered_turn_cancel_closure(
    pending: Vec<crate::TurnCancelClosureAuthorization>,
    address: &crate::TurnAddress,
    binding_id: &str,
    admitted_scope: &crate::ExecutionScope,
) -> Result<Option<crate::TurnCancelClosureAuthorization>, RuntimeError> {
    let Some(authorization) = pending
        .into_iter()
        .find(|authorization| authorization.address() == *address)
    else {
        return Ok(None);
    };
    authorization.validate()?;
    if authorization.binding_id() != binding_id || authorization.admitted_scope() != admitted_scope
    {
        return Err(RuntimeError::new(
            RuntimeErrorCode::InvalidTurnCancelRequest,
            format!(
                "pending turn cancellation closure for `{address:?}` does not match the admitted binding and scope"
            ),
        ));
    }
    Ok(Some(authorization))
}

pub(super) struct TurnFinishInput {
    pub(super) turn_pipeline: TurnBoundary,
    pub(super) recorded_assembly: RecordedTurnAssembly,
    pub(super) new_messages: crate::MessageSequence,
    pub(super) turn_index: usize,
    pub(super) trace_turn_id: TurnId,
    /// The protocol iterations the run has spent through this turn, when the
    /// turn ended at a segment boundary (FIG-4739): what the continuation its
    /// commit owes records.
    pub(super) segment_boundary: Option<crate::runtime::turn_driver::BoundaryTaken>,
}

struct PreparedTurn {
    turn_pipeline: TurnBoundary,
    turn: AssembledTurn,
    events: Vec<SessionStreamEvent>,
}

/// What the final commit writes: the session it advances and the ingress
/// settlement it carries under its run's shift fence.
struct TurnCommitRequest<'commit> {
    session: Option<&'commit mut Session>,
    commit_effects: super::logical_turn::LogicalTurnCommitEffects,
    trace_turn_id: &'commit TurnId,
    recorded_attachment_intent_ids: std::collections::BTreeSet<crate::AttachmentId>,
    interrupted_turn: Option<crate::store::InterruptedTurnClosure>,
    turn_control_resolver: &'commit dyn crate::AwaitEventResolver,
}

/// The local commit-admission handles: only the head-advancing attempt uses
/// them, and they are dropped when the store needs no admission.
struct TurnCommitAdmission<'admission> {
    effect_controller: &'admission dyn crate::RuntimeEffectController,
    turn_phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
}

impl PreparedTurn {
    fn outcome(&self) -> &TurnOutcome {
        &self.turn.outcome
    }

    async fn commit(
        self,
        request: TurnCommitRequest<'_>,
        admission: TurnCommitAdmission<'_>,
    ) -> Result<CommittedTurn, crate::StoreError> {
        let TurnCommitAdmission {
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
            return Box::pin(self.commit_after_admission(request)).await;
        }
        let session_id = self.turn_pipeline.state().session_id.clone();
        let work_identity = request.trace_turn_id.to_string();
        super::run_head_advancing_commit_attempt(
            session_id.clone(),
            work_identity.clone(),
            // A turn that reached its commit, cancelled or not, commits: its
            // admission wait is not cancellable.
            CancellationToken::new(),
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
            commit_effects,
            trace_turn_id: _,
            recorded_attachment_intent_ids,
            interrupted_turn,
            turn_control_resolver,
        } = request;
        Box::pin(self.turn_pipeline.final_commit(
            &mut self.turn,
            session,
            commit_effects.ingress_settlement,
            commit_effects.pending_follow_on,
            // Any active-turn input that missed the turn's final
            // checkpoint must become the next ordinary user turn: the
            // closure names the turn.
            interrupted_turn,
            Some(turn_control_resolver),
            recorded_attachment_intent_ids,
        ))
        .await?;
        Ok(CommittedTurn {
            turn: self.turn,
            events: self.events,
            resident_state: self.turn_pipeline.into_final_state(),
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
    ) -> Result<PostCommitDelivery, RuntimeError> {
        runtime.install_resident_state(self.resident_state)?;
        let observation_revision =
            crate::runtime::observation::observation_revision(&runtime.state);
        runtime
            .resident_session
            .record_committed_observation_turn(observation_revision.as_u64(), trace_turn_id);
        Ok(PostCommitDelivery {
            turn: self.turn,
            events: self.events,
            post_commit_delivery_failed: false,
        })
    }
}

/// What the commit phase needs to settle one physical turn: the assembled turn
/// itself, the admitted rows it must settle, and the shift fence and control
/// handles the settlement runs under.
pub(in crate::runtime) struct TurnCommitContext<'commit, 'run> {
    pub(in crate::runtime) finish: TurnFinishInput,
    pub(in crate::runtime) admissions: &'commit LogicalTurnAdmissions,
    pub(in crate::runtime) scoped_effect_controller: &'commit ScopedEffectController<'run>,
    /// The cancellation the turn recorded honouring, if any: a journaled
    /// peek's answer, never a live token (FIG-3672 P9).
    pub(in crate::runtime) honoured_cancel: Option<crate::TurnCancellationEvidence>,
    pub(in crate::runtime) shift_fence: Option<&'commit ShiftFence>,
    pub(in crate::runtime) turn_control: &'commit ActiveTurnControl,
    /// What the turn publishes through. The terminal publication waits
    /// until the host has received every event the turn queued (see
    /// `turn_observer`'s host contract).
    pub(in crate::runtime) observer: &'commit TurnObserver,
}

/// The cancellation tail of the execute phase: the driver remainder a cancelled
/// effect loop left behind, handed to the commit phase to settle.
pub(super) struct CancelledTurnFinishContext<'cancel, 'run> {
    pub(super) driver: TurnDriverRemainder,
    pub(super) cancellation_messages: crate::MessageSequence,
    pub(super) finish_scoped_effect_controller: &'cancel ScopedEffectController<'run>,
    pub(super) shift_fence: Option<&'cancel ShiftFence>,
    pub(super) turn_control: &'cancel ActiveTurnControl,
    pub(super) turn_index: usize,
    pub(super) trace_turn_id: TurnId,
    pub(super) observer: &'cancel TurnObserver,
}

/// The terminal turn a logical run commits when it refuses to switch agent
/// frames again.
pub(in crate::runtime) struct LogicalTurnErrorContext<'error, 'run> {
    /// The typed failure the terminal carries.
    pub(in crate::runtime) code: crate::TurnFailureCode,
    pub(in crate::runtime) message: String,
    pub(in crate::runtime) trace_turn_id: TurnId,
    /// The input the failed turn was owed, recorded as its delivered input: a
    /// follow-on that never ran still answers its task (ADR 0101 §3).
    pub(in crate::runtime) delivered_task: Option<String>,
    pub(in crate::runtime) sinks: TurnSinks<'error>,
    pub(in crate::runtime) scoped_effect_controller: ScopedEffectController<'run>,
    pub(in crate::runtime) admissions: LogicalTurnAdmissions,
    pub(in crate::runtime) shift_fence: Option<&'error ShiftFence>,
}

impl LashRuntime {
    /// Exercise terminal commit with a run whose installed run record was
    /// lost. The conformance laws use the real commit path on each store.
    #[cfg(feature = "testing")]
    pub async fn finish_without_recorded_run_for_testing(
        &mut self,
        run: TurnId,
        opts: TurnOptions<'_>,
    ) -> Result<(), RuntimeError> {
        self.uninstall_run_view()?;
        let controller = opts.scoped_effect_controller();
        let binding =
            turn_control_binding(self.host.core.control.effect_host.as_ref(), &controller).await?;
        let control = ActiveTurnControl::new(
            binding.resolver(),
            TurnAddress::new(&self.state.session_id, &run),
        )
        .await?;
        let (observer, _observations) =
            TurnObserver::open(opts.events_or_noop(), opts.turn_events_or_noop());
        let admissions = LogicalTurnAdmissions::new(Vec::new(), Vec::new());
        let pipeline = TurnBoundary::from_state_with_clock(
            self.state.clone(),
            Arc::clone(&self.host.core.clock),
            self.state.turn_scope(&run),
            self.host.core.durability.commit_budget,
        )
        .with_metrics(self.host.core.tracing.metrics().clone());
        self.finish_turn(TurnCommitContext {
            finish: TurnFinishInput {
                segment_boundary: None,
                turn_pipeline: pipeline,
                recorded_assembly: RecordedTurnAssembly::new(),
                new_messages: crate::MessageSequence::default(),
                turn_index: self.state.turn_index,
                trace_turn_id: run,
            },
            admissions: &admissions,
            scoped_effect_controller: &controller,
            honoured_cancel: None,
            shift_fence: None,
            turn_control: &control,
            observer: &observer,
        })
        .await
        .map(|_| ())
    }

    /// Commit one physical turn. A stopped turn's terminal, held since the
    /// turn recorded it, publishes only once this commit is accepted; a
    /// failed commit publishes none of it (ADR 0122).
    pub(super) async fn finish_turn(
        &mut self,
        context: TurnCommitContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let observer = context.observer;
        let finished = Box::pin(self.commit_finished_turn(context)).await;
        if finished.is_err() {
            observer.abandon_terminal();
        }
        finished
    }

    /// The termination policy the running run recorded in its
    /// [`ResolvedRun`](crate::ResolvedRun). The logical-turn funnel installs
    /// that record before the run's first physical turn and every resident
    /// refresh re-installs it, so every commit of the run reads it.
    fn recorded_termination(&self) -> Result<crate::runtime::TerminationPolicy, RuntimeError> {
        self.state
            .authority
            .run_view()
            .map(|view| view.run.termination.clone())
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::RecordedTerminationUnavailable,
                    "terminal assembly requires the run's recorded termination policy",
                )
            })
    }

    async fn commit_finished_turn(
        &mut self,
        context: TurnCommitContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let termination = self.recorded_termination()?;
        let TurnCommitContext {
            finish,
            admissions,
            scoped_effect_controller,
            honoured_cancel,
            shift_fence,
            turn_control,
            observer,
        } = context;
        let turn_control_host = Arc::clone(&self.host.core.control.effect_host);
        let turn_control_binding =
            turn_control_binding(turn_control_host.as_ref(), scoped_effect_controller).await?;
        let turn_control_resolver = turn_control_binding.resolver();
        let turn_control_binding_id = turn_control_binding.binding_id().to_string();
        let TurnFinishInput {
            mut turn_pipeline,
            recorded_assembly: assembly,
            new_messages,
            turn_index,
            trace_turn_id,
            segment_boundary,
        } = finish;
        turn_pipeline.state_mut().policy = self.state.effective_policy().clone();
        turn_pipeline.state_mut().turn_index = turn_index;

        // The evidence the executed turn already named travels into the gate,
        // so the request id a host saw on the streamed outcome is the one the
        // committed report carries. Only a cancellation with no assembled
        // evidence at all falls back to lash's internal mint.
        let assembled_cancellation = match &assembly.outcome {
            Some(TurnOutcome::Stopped(TurnStop::Cancelled { evidence })) => Some(evidence.clone()),
            _ => None,
        };
        let mut interrupted_turn_cancel_intent =
            match self.session.as_ref().and_then(Session::history_store) {
                Some(store) => Some(
                    store
                        .turn_cancel_request_intent(&crate::TurnAddress::new(
                            &self.state.session_id,
                            &trace_turn_id,
                        ))
                        .await
                        .map_err(runtime_error_from_store_commit)?,
                ),
                None => None,
            };
        let turn_cancel_closure_authorization = match (
            self.session.as_ref().and_then(Session::history_store),
            shift_fence,
            interrupted_turn_cancel_intent.clone(),
        ) {
            (Some(store), Some(fence), Some(observed)) => {
                let address = crate::TurnAddress::new(&self.state.session_id, &trace_turn_id);
                let admitted_scope = crate::runtime::effect::executor::admitted_turn_cancel_scope(
                    &address,
                    scoped_effect_controller.execution_scope(),
                    &turn_control_binding_id,
                );
                if let Some(authorization) = recovered_turn_cancel_closure(
                    store
                        // The exact persisted operation is matched to this
                        // address, binding, and scope below; activation's fence
                        // check is separate.
                        .pending_turn_cancel_closure_pins()
                        .await
                        .map_err(runtime_error_from_store_commit)?,
                    &address,
                    &turn_control_binding_id,
                    &admitted_scope,
                )? {
                    Some(authorization)
                } else {
                    let mut observed = observed;
                    loop {
                        let authorization = turn_control.closure_authorization(
                            &turn_control_binding_id,
                            admitted_scope.clone(),
                            fence,
                            observed.clone(),
                            honoured_cancel.as_ref(),
                            assembled_cancellation.clone(),
                        )?;
                        match store
                            .authorize_turn_cancel_closure(fence, &authorization)
                            .await
                        {
                            Ok(_) => {
                                interrupted_turn_cancel_intent =
                                    Some(authorization.observed_intent().clone());
                                break Some(authorization);
                            }
                            Err(crate::StoreError::TurnCancelIntentChanged { .. }) => {
                                observed = store
                                    .turn_cancel_request_intent(&address)
                                    .await
                                    .map_err(runtime_error_from_store_commit)?;
                            }
                            Err(error) => return Err(runtime_error_from_store_commit(error)),
                        }
                    }
                }
            }
            _ => None,
        };
        let interrupted_turn = match (
            turn_cancel_closure_authorization.as_ref(),
            interrupted_turn_cancel_intent,
        ) {
            (Some(authorization), Some(observed_intent)) => {
                Some(crate::store::InterruptedTurnClosure {
                    settlement: turn_control
                        .settle_authorized(
                            turn_control_resolver,
                            authorization,
                            honoured_cancel.as_ref(),
                        )
                        .await?,
                    observed_intent,
                })
            }
            // A turn that commits to a store closes its cancellation gate
            // under its run's shift fence. With no fence no closure was
            // authorized, and the store has nothing to settle the turn by.
            (None, Some(_)) => {
                return Err(runtime_error_from_store_commit(
                    crate::StoreError::TurnCancelClosureAuthorizationMismatch {
                        session_id: self.state.session_id.clone(),
                        turn_id: trace_turn_id.clone(),
                    },
                ));
            }
            (_, None) => None,
        };
        let cancellation = match interrupted_turn.as_ref() {
            Some(interrupted) => interrupted.cancellation().cloned(),
            None => {
                turn_control
                    .settle_before_commit(
                        turn_control_resolver,
                        honoured_cancel.as_ref(),
                        assembled_cancellation,
                    )
                    .await?
            }
        };
        // Interruption derives from the sealed gate evidence. When a durable
        // cancel races a successor shift, the final commit's shift fence and
        // head CAS are the arbiters.
        let interrupted = cancellation.is_some();

        turn_pipeline.finalize_turn_read_state(new_messages, interrupted);
        let turn_trace = self.host.core.tracing.turn_execution(
            &self.state.session_id,
            &trace_turn_id,
            scoped_effect_controller,
        );
        for diagnostic in turn_pipeline.take_projection_diagnostics() {
            turn_trace.observe(|| {
                (
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
                )
            });
        }
        if !assembly.token_usage.is_zero() {
            turn_pipeline.state_mut().token_usage = assembly.token_usage.clone();
        }

        let last_prompt_usage = assembly
            .last_llm_usage()
            .filter(|usage| !usage.is_zero())
            .cloned();
        turn_pipeline.state_mut().last_prompt_usage = last_prompt_usage;
        let assembled_state = turn_pipeline.export_state_for_assembly();
        // The run's recorded termination policy, never this worker's: a
        // replay or redrive on a worker with another policy assembles the
        // same terminal for the same recorded work (FIG-4389).
        let assembled = assembly.finish(assembled_state, cancellation.clone(), None, &termination);

        let Some(session) = self.session.as_ref() else {
            // A store-less session keeps the head's follow-on resident: the
            // same one path writes and clears it (ADR 0101 §3).
            let pending_follow_on = crate::runtime::logical_turn::follow_on_after_turn(
                &self.state,
                &assembled.outcome,
                &trace_turn_id,
                self.shift_run
                    .as_ref()
                    .map_or(&trace_turn_id, |ran_execution| ran_execution.run()),
                assembled.state.current_frame_node_id.as_ref(),
                segment_boundary.as_ref(),
            )?;
            self.state.adopt_snapshot(assembled.state.clone());
            self.state.pending_follow_on = pending_follow_on.map(Box::new);
            let observation_revision =
                crate::runtime::observation::observation_revision(&self.state);
            self.resident_session
                .record_committed_observation_turn(observation_revision.as_u64(), &trace_turn_id);
            self.emit_completed_turn_trace(
                &turn_trace,
                &assembled.state,
                &assembled.outcome,
                &trace_turn_id,
            );
            observer.release_terminal();
            observer.published().await;
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
                post_commit_delivery_failed: false,
                withheld_terminal_work: None,
            });
        };

        let plugins = Arc::clone(session.plugins());
        let manager = match self
            .runtime_session_services_for_turn(shift_fence, turn_pipeline.graph_appends())
        {
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
            .dispatch(self.turn_phase_probe.as_ref())
            .finalize_turn(
                assembled,
                manager.state_service(),
                manager.graph_service(),
                &trace_turn_id,
                self.services.clock.as_ref(),
            )
            .await
        {
            Ok(finalized) => finalized,
            Err(err) => {
                self.mark_phase_end(PreparedTurn::RUNTIME_PHASE);
                return Err(err.into_turn_failure(RuntimeErrorCode::PluginFinalizeTurn));
            }
        };
        let returned_turn = finalized.turn;
        let prepared = PreparedTurn {
            turn_pipeline,
            turn: returned_turn,
            events: finalized.events,
        };
        // The follow-on this turn's terminal commit leaves on the head: a frame
        // switch writes it, and any other outcome of the turn it names clears
        // it (ADR 0101 §3).
        let pending_follow_on = match crate::runtime::logical_turn::follow_on_after_turn(
            &self.state,
            prepared.outcome(),
            &trace_turn_id,
            self.shift_run
                .as_ref()
                .map_or(&trace_turn_id, |ran_execution| ran_execution.run()),
            prepared.turn.state.current_frame_node_id.as_ref(),
            segment_boundary.as_ref(),
        ) {
            Ok(pending_follow_on) => pending_follow_on,
            Err(err) => {
                self.mark_phase_end(PreparedTurn::RUNTIME_PHASE);
                return Err(err);
            }
        };
        let commit_effects = admissions.commit_effects(prepared.outcome(), pending_follow_on);
        let settlement_trace = self.shift_run.as_ref().map(|run| {
            commit_effects.ingress_settlement.clone().into_ingress(
                run.run().clone(),
                cancellation
                    .as_ref()
                    .map_or(crate::TurnCancelUndeliveredInputPolicy::Defer, |evidence| {
                        evidence.undelivered
                    }),
            )
        });
        // Under an admitted run, the commit presents the run's shift fence
        // and, when this turn ends the run, writes its terminal evidence
        // (FIG-3600 S7).
        let shift_commit = self.shift_run.as_ref().and_then(|run| {
            let owes_follow_on = commit_effects.pending_follow_on.is_some()
                || admissions.carries_follow_on_work(matches!(
                    prepared.outcome(),
                    TurnOutcome::Stopped(TurnStop::Cancelled { .. })
                ));
            run.commit_facts(&trace_turn_id, prepared.outcome(), owes_follow_on)
        });
        let writes_run_terminal = shift_commit
            .as_ref()
            .is_some_and(|commit| commit.terminal.is_some());
        let mut prepared = prepared;
        prepared.turn_pipeline.set_shift_commit(shift_commit);
        // The commit clears the park of the run the turn runs under, the
        // same run an abort of the turn parks (D2 §1.3 P3).
        prepared.turn_pipeline.set_park_run(self.park_run(
            scoped_effect_controller.execution_scope().logical_run(),
            &trace_turn_id,
        ));
        let committed = match Box::pin(
            prepared.commit(
                TurnCommitRequest {
                    session: self.session.as_mut(),
                    commit_effects,
                    trace_turn_id: &trace_turn_id,
                    recorded_attachment_intent_ids: self
                        .host
                        .core
                        .durability
                        .attachment_store
                        .recorded_execution_puts(
                            &scoped_effect_controller
                                .execution_scope()
                                .journal_identity()?,
                        ),
                    interrupted_turn,
                    turn_control_resolver,
                },
                TurnCommitAdmission {
                    effect_controller: scoped_effect_controller.controller(),
                    turn_phase_probe: self.turn_phase_probe.clone(),
                },
            ),
        )
        .await
        {
            Ok(committed) => {
                if writes_run_terminal && let Some(run) = self.shift_run.as_mut() {
                    run.mark_terminal_written();
                }
                committed
            }
            Err(err) => {
                // A store fault is this attempt's own evidence, whatever the
                // attempt replayed before it met the fault.
                crate::trace::emit_store_error(
                    &self
                        .host
                        .core
                        .tracing
                        .unreplayed(turn_trace.scope().cloned()),
                    lash_trace::TraceContext::default()
                        .for_session(self.state.session_id.clone())
                        .for_turn(trace_turn_id.clone()),
                    "turn_commit",
                    &err,
                );
                // Reported here, not inside the commit: the writer's identity
                // is already live in this future, so naming it costs nothing,
                // while carrying evidence through the commit await would grow
                // every turn future.
                trace_commit_cas_rejected(
                    &self.state.session_id,
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
        let mut delivery = committed.adopt(self, &trace_turn_id)?;
        self.mark_phase_end(CommittedTurn::RUNTIME_PHASE);
        self.mark_phase_begin(PostCommitDelivery::RUNTIME_PHASE);

        observer.release_terminal();
        emit_session_events(observer, delivery.events);
        observer.published().await;
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
            && let Err(err) = self.restore_protocol_session_after_frame_open().await
        {
            delivery.turn.errors.push(post_commit_delivery_issue(
                crate::TurnFailureCode::ProtocolRestoreSession.into(),
                err.to_string(),
            ));
            delivery.post_commit_delivery_failed = true;
        }
        if let Some(settlement) = settlement_trace.filter(|settlement| !settlement.is_empty()) {
            turn_trace.observe(|| {
                (
                    lash_trace::TraceContext::default()
                        .for_session(delivery.turn.state.session_id.clone())
                        .for_turn_index(delivery.turn.state.turn_index)
                        .for_turn(trace_turn_id.clone()),
                    lash_trace::TraceEvent::Custom {
                        name: "ingress.settled".to_string(),
                        payload: ingress_settled_trace_payload(&settlement),
                    },
                )
            });
        }
        // The commit's observers write under its shift's fence, a final
        // commit's included (FIG-4202): they run at the run's boundary, so a
        // write they make is the owner's own, never one outside the shift
        // that waits on the shift's settlement and deadlocks it. A later
        // admission that sealed since refuses such a write typed, with
        // nothing written.
        match self
            .emit_turn_persisted_event(
                &delivery.turn,
                scoped_effect_controller,
                &trace_turn_id,
                shift_fence,
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
                delivery.turn.errors.push(post_commit_delivery_issue(
                    crate::FailureCode::from(&err.code),
                    err.message,
                ));
                delivery.post_commit_delivery_failed = true;
                self.invalidate_resident_session_state();
            }
        }
        self.mark_phase_end(PostCommitDelivery::RUNTIME_PHASE);

        self.emit_completed_turn_trace(
            &turn_trace,
            &delivery.turn.state,
            &delivery.turn.outcome,
            &trace_turn_id,
        );
        Ok(PhysicalTurnExecution {
            turn: delivery.turn,
            post_commit_delivery_failed: delivery.post_commit_delivery_failed,
            withheld_terminal_work: None,
        })
    }

    pub(super) async fn finish_cancelled_turn_after_effect_abort(
        &mut self,
        context: CancelledTurnFinishContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let CancelledTurnFinishContext {
            driver,
            cancellation_messages,
            finish_scoped_effect_controller,
            shift_fence,
            turn_control,
            turn_index,
            trace_turn_id,
            observer,
        } = context;
        let TurnDriverRemainder {
            mut recorded_assembly,
            turn_pipeline,
            pending_queued,
            pending_turn_inputs,
            withheld_terminal_work,
            turn_cancel,
            ..
        } = driver;
        // Only a recorded cancellation reaches this finisher; lash's own
        // evidence stands in for none (FIG-3672 P9).
        let evidence = turn_cancel.unwrap_or_else(|| turn_control.internal_evidence(None));
        hold_terminal_sequence(
            &mut recorded_assembly,
            observer,
            &mut turn_observation_cursor(
                finish_scoped_effect_controller,
                &trace_turn_id,
                "terminal",
            ),
            None,
            TurnStop::Cancelled {
                evidence: evidence.clone(),
            },
        );
        // A cancelled turn starts no follow-on (FIG-3157). Its final commit
        // hands the work it withheld from its terminal checkpoint to the
        // cancellation, directly rather than inferred from the committed
        // outcome: input settles through the undelivered disposition
        // (FIG-3531), and wakes are released and recorded, never completed
        // (FIG-3543, ADR 0101 §10).
        let admissions = LogicalTurnAdmissions::new(pending_queued, pending_turn_inputs)
            .with_undelivered(withheld_terminal_work);
        Box::pin(self.finish_turn(TurnCommitContext {
            finish: TurnFinishInput {
                segment_boundary: None,
                turn_pipeline,
                recorded_assembly,
                new_messages: cancellation_messages,
                turn_index,
                trace_turn_id,
            },
            admissions: &admissions,
            scoped_effect_controller: finish_scoped_effect_controller,
            honoured_cancel: Some(evidence),
            shift_fence,
            turn_control,
            observer,
        }))
        .await
    }

    /// A shift that is replaying its journal reconstructs the turn's
    /// terminal and reports nothing. The commit reports no inserted-or-existing
    /// verdict yet, so the terminal is observed as the work of the attempt
    /// that first reaches it rather than as a logical transition.
    fn emit_completed_turn_trace(
        &self,
        turn_trace: &crate::trace::TraceStanding,
        state: &SessionSnapshot,
        outcome: &TurnOutcome,
        trace_turn_id: &TurnId,
    ) {
        let Some(trace_outcome) = trace_outcome(outcome) else {
            return;
        };
        turn_trace.observe(|| {
            (
                lash_trace::TraceContext::default()
                    .for_session(state.session_id.clone())
                    .for_turn_index(state.turn_index)
                    .for_turn(trace_turn_id.clone()),
                lash_trace::TraceEvent::TurnCompleted {
                    outcome: trace_outcome,
                },
            )
        });
        // The turn reached its end in this attempt: nothing it observed after
        // the last step its journal answered is a reconstruction.
        turn_trace.conclude();
    }

    pub(in crate::runtime) async fn finish_logical_turn_error(
        &mut self,
        context: LogicalTurnErrorContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let LogicalTurnErrorContext {
            code,
            message,
            trace_turn_id,
            delivered_task,
            sinks: TurnSinks { observer },
            scoped_effect_controller,
            admissions,
            shift_fence,
        } = context;
        // A recovered follow-on's terminal commits at the index its run's
        // decision recorded, on the head it adopted (FIG-4380).
        let admitted_turn_index = self.admitted_turn_index.take();
        let turn_control_host = Arc::clone(&self.host.core.control.effect_host);
        let turn_control_binding =
            turn_control_binding(turn_control_host.as_ref(), &scoped_effect_controller).await?;
        let turn_control_resolver = turn_control_binding.resolver();
        let turn_control = Arc::new(
            ActiveTurnControl::new(
                turn_control_resolver,
                TurnAddress::new(&self.state.session_id, &trace_turn_id),
            )
            .await?,
        );
        let mut recorded_assembly = RecordedTurnAssembly::default();
        hold_terminal_sequence(
            &mut recorded_assembly,
            observer,
            &mut turn_observation_cursor(&scoped_effect_controller, &trace_turn_id, "terminal"),
            Some(TerminalDiagnostic {
                kind: TerminalDiagnosticKind::Runtime,
                code: Some(code.into()),
                message,
                retryable: Some(false),
                activity: TerminalActivityTarget::ForTurn {
                    observer,
                    turn_id: &trace_turn_id,
                },
            }),
            TurnStop::RuntimeError,
        );

        let delivered = delivered_task
            .map(|task| {
                let id = format!("m_turn_{trace_turn_id}_input");
                Message {
                    parts: shared_parts(vec![Part::text(format!("{id}.p0"), task, None)]),
                    id,
                    role: MessageRole::User,
                    origin: Some(crate::MessageOrigin::TurnInput {
                        turn_id: trace_turn_id.clone(),
                        input_id: None,
                    }),
                    reply_marker: None,
                }
            })
            .into_iter()
            .collect();
        let messages = crate::MessageSequence::from_base_and_delta(
            self.state.read_model().messages,
            delivered,
        );
        // The terminal commits under its own turn's scope, so a follow-on's
        // terminal is that follow-on's commit and clears its fact.
        let mut turn_pipeline = TurnBoundary::from_state_with_clock(
            self.state.clone(),
            Arc::clone(&self.host.core.clock),
            self.state.turn_scope(&trace_turn_id),
            self.host.core.durability.commit_budget,
        )
        .with_definition_engines(self.host.core.process_engines.clone())
        .with_metrics(self.host.core.tracing.metrics().clone());
        turn_pipeline.apply_prepared_messages(&messages);
        Box::pin(self.finish_turn(TurnCommitContext {
            finish: TurnFinishInput {
                segment_boundary: None,
                turn_pipeline,
                recorded_assembly,
                new_messages: messages,
                // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                turn_index: admitted_turn_index.unwrap_or(self.state.turn_index + 1),
                trace_turn_id,
            },
            admissions: &admissions,
            scoped_effect_controller: &scoped_effect_controller,
            honoured_cancel: None,
            shift_fence,
            turn_control: &turn_control,
            observer,
        }))
        .await
    }
}
