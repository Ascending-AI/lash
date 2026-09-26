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

/// Select the exact closure operation a recovered turn must finish.
///
/// A successor lease may settle and consume this persisted operation, but may
/// not replace it with an authorization carrying its new fencing token. The
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
    queued_run: Option<Box<crate::store::QueuedRunCommit>>,
    session_execution_lease: Option<&'commit SessionExecutionLeaseGuard>,
    release_session_execution_lease: bool,
    trace_turn_id: &'commit TurnId,
    recorded_attachment_intent_ids: std::collections::BTreeSet<crate::AttachmentId>,
    interrupted_turn_input_cancellation: Option<crate::TurnCancellationEvidence>,
    interrupted_turn_cancel_intent: Option<crate::TurnCancelIntentSnapshot>,
    turn_cancel_closure_settlement: Option<crate::TurnCancelClosureSettlement>,
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

    fn final_operation(&self) -> crate::OperationId {
        self.turn_pipeline.final_operation()
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
            staged_usage,
            commit_effects,
            queued_run,
            session_execution_lease,
            release_session_execution_lease,
            trace_turn_id,
            recorded_attachment_intent_ids,
            interrupted_turn_input_cancellation,
            interrupted_turn_cancel_intent,
            turn_cancel_closure_settlement,
            turn_control_resolver,
        } = request;
        // The staged usage deltas ride the same atomic commit as the turn's
        // final operation, so `turn_is_committed` also proves whether those
        // deltas are durable. The store is captured before `final_commit`
        // consumes the session borrow, for the lost-reply branch below.
        let history_store = session.as_deref().and_then(Session::history_store);
        let accepted = Box::pin(
            self.turn_pipeline.final_commit(
                &mut self.turn,
                session,
                staged_usage.deltas(),
                commit_effects.claim_settlement,
                session_execution_lease.map(SessionExecutionLeaseGuard::fence),
                commit_effects.pending_follow_on,
                queued_run,
                // Any active-turn input that missed the turn's final
                // checkpoint must become the next ordinary user turn.
                Some(trace_turn_id.clone()),
                interrupted_turn_input_cancellation,
                interrupted_turn_cancel_intent,
                turn_cancel_closure_settlement,
                Some(turn_control_resolver),
                recorded_attachment_intent_ids,
                release_session_execution_lease
                    .then(|| session_execution_lease.map(SessionExecutionLeaseGuard::completion))
                    .flatten(),
            ),
        )
        .await;
        let accepted = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                // A reply lost after the store applied the commit leaves the
                // staged usage already durable: keeping the pending rows would
                // count this turn's usage twice once the resident ledger
                // reloads. Discard them only on a confirmed landing — on a
                // clean failure, or when the probe cannot answer, they stay
                // pending for the next boundary.
                if let Some(store) = history_store.as_deref() {
                    let operation = self.turn_pipeline.final_operation();
                    if let (Some(session_id), Some(turn_id)) =
                        (operation.scope.session_id(), operation.scope.turn_id())
                        && matches!(
                            store
                                .turn_is_committed(&crate::TurnAddress::new(
                                    session_id.clone(),
                                    turn_id.clone(),
                                ))
                                .await,
                            Ok(true)
                        )
                    {
                        staged_usage.discard_staged();
                    }
                }
                return Err(error);
            }
        };
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
        let confirmed_usage = self.accepted.into_confirmed_usage();
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
/// itself, the claims it must settle, and the lease and control handles the
/// settlement runs under.
pub(in crate::runtime) struct TurnCommitContext<'commit, 'run> {
    pub(in crate::runtime) finish: TurnFinishInput,
    pub(in crate::runtime) claims: &'commit LogicalTurnClaims,
    pub(in crate::runtime) scoped_effect_controller: &'commit ScopedEffectController<'run>,
    /// The cancellation the turn recorded honouring, if any: a journaled
    /// peek's answer, never a live token (FIG-3672 P9).
    pub(in crate::runtime) honoured_cancel: Option<crate::TurnCancellationEvidence>,
    pub(in crate::runtime) lease: TurnLeaseScope<'commit>,
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
    pub(super) lease: TurnLeaseScope<'cancel>,
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
            scoped_effect_controller,
            honoured_cancel,
            lease:
                TurnLeaseScope {
                    guard: session_execution_lease,
                    release_policy: session_execution_lease_release_policy,
                },
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
            policy,
            turn_index,
            trace_turn_id,
        } = finish;
        turn_pipeline.state_mut().policy = self.state.effective_policy().clone();
        turn_pipeline.state_mut().turn_index = turn_index;

        if !assembly.token_usage.is_zero() {
            session_manager::record_token_usage_shared(
                &self.shared_token_ledger,
                "turn",
                &policy.model.id,
                &assembly.token_usage,
            );
        }
        // The cumulative row above covers only counted responses. Every other
        // attempt that reported usage — a billed failed attempt, or every
        // attempt of a call that never completed — still gets its own delta.
        session_manager::record_attempt_usage_shared(
            &self.shared_token_ledger,
            "turn",
            &policy.model.id,
            &assembly.llm_calls,
            assembly.usage_counted_calls,
        );
        // ADR 0031: an attempt the host aborted or that failed before the
        // provider's usage arrived was still billed. Write the hole as a typed
        // unreported row (even at zero usage) and remember the attempt so a
        // host can reconcile it later; a turn with no interruption and no
        // usage still writes nothing.
        let unreported = unreported_usage_attempts(&assembly.llm_calls, &policy.model.id);
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
        let assembled_cancellation = match &assembly.outcome {
            Some(TurnOutcome::Stopped(TurnStop::Cancelled { evidence })) => Some(evidence.clone()),
            _ => None,
        };
        let lease_was_lost = session_execution_lease.is_some_and(|lease| lease.is_lost());
        // A lost lease never turns a recorded cancellation into a proposal of
        // this worker's: the commit's head CAS arbitrates the race.
        let honoured_cancel = honoured_cancel.filter(|_| !lease_was_lost);
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
            session_execution_lease,
            interrupted_turn_cancel_intent.clone(),
        ) {
            (Some(store), Some(lease), Some(observed)) => {
                let address = crate::TurnAddress::new(&self.state.session_id, &trace_turn_id);
                let admitted_scope = crate::runtime::effect::executor::admitted_turn_cancel_scope(
                    &address,
                    scoped_effect_controller.execution_scope(),
                    &turn_control_binding_id,
                );
                if let Some(authorization) = recovered_turn_cancel_closure(
                    store
                        // Finalization may outlive the advisory lease. The exact
                        // persisted operation is matched to this address, binding,
                        // and scope below; activation's live-lease check is separate.
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
                            &lease.fence(),
                            observed.clone(),
                            honoured_cancel.as_ref(),
                            assembled_cancellation.clone(),
                        )?;
                        match store
                            .authorize_turn_cancel_closure(&lease.fence(), &authorization)
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
        let turn_cancel_closure_settlement = match turn_cancel_closure_authorization.as_ref() {
            Some(authorization) => Some(
                turn_control
                    .settle_authorized(
                        turn_control_resolver,
                        authorization,
                        honoured_cancel.as_ref(),
                    )
                    .await?,
            ),
            None => None,
        };
        let cancellation = match turn_cancel_closure_settlement.as_ref() {
            Some(settlement) => settlement.effective_cancellation().cloned(),
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
        // cancel races lease loss, the final commit's head CAS and any claim
        // batch-ownership checks are the arbiters. Lease loss alone does not
        // reject a current-head commit.
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
        if !assembly.token_usage.is_zero() {
            turn_pipeline.state_mut().token_usage = assembly.token_usage.clone();
        }

        let last_prompt_usage = assembly
            .last_llm_usage()
            .filter(|usage| !usage.is_zero())
            .cloned();
        turn_pipeline.state_mut().last_prompt_usage = last_prompt_usage;
        let assembled_state = turn_pipeline.export_state_for_assembly();
        let assembled = assembly.finish(
            assembled_state,
            cancellation.clone(),
            None,
            &self.host.core.control.termination,
        );

        let Some(session) = self.session.as_ref() else {
            // A store-less session keeps the head's follow-on resident: the
            // same one path writes and clears it (ADR 0101 §3).
            let pending_follow_on = crate::runtime::logical_turn::follow_on_after_turn(
                &self.state,
                &assembled.outcome,
                &trace_turn_id,
            )?;
            self.state.apply_snapshot(&assembled.state);
            self.state.pending_follow_on = pending_follow_on.map(Box::new);
            let observation_revision =
                crate::runtime::observation::observation_revision(&self.state);
            self.resident_session
                .record_committed_observation_turn(observation_revision.as_u64(), &trace_turn_id);
            self.emit_completed_turn_trace(&assembled.state, &assembled.outcome, &trace_turn_id);
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
        let manager = match self.runtime_session_services_for_turn(
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
                manager.graph_service(),
                self.turn_phase_probe.clone(),
                &trace_turn_id,
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
        let release_session_execution_lease = session_execution_lease_release_policy
            .should_release(
                prepared.outcome(),
                claims.carries_follow_on_work(matches!(
                    prepared.outcome(),
                    TurnOutcome::Stopped(TurnStop::Cancelled { .. })
                )),
            );
        // The follow-on this turn's terminal commit leaves on the head: a frame
        // switch writes it, and any other outcome of the turn it names clears
        // it (ADR 0101 §3).
        let pending_follow_on = match crate::runtime::logical_turn::follow_on_after_turn(
            &self.state,
            prepared.outcome(),
            &trace_turn_id,
        ) {
            Ok(pending_follow_on) => pending_follow_on,
            Err(err) => {
                self.mark_phase_end(PreparedTurn::RUNTIME_PHASE);
                return Err(err);
            }
        };
        let commit_effects = claims.commit_effects(
            prepared.outcome(),
            &self.journaled_drive_claims,
            &self.queued_run_reacquired,
            pending_follow_on,
        );
        let queued_run = self
            .queued_run
            .as_ref()
            .map(|run| {
                use crate::store::{
                    QueuedRunCommit, QueuedRunMember, QueuedRunProgress, QueuedRunTerminal,
                };
                let mut withheld = run.withheld_members.clone();
                if let Some(work) = &claims.withheld_terminal_work {
                    withheld.extend(work.queued.iter().flat_map(|claim| {
                        claim
                            .batches
                            .iter()
                            .map(|batch| QueuedRunMember::Batch(batch.batch_id.clone()))
                    }));
                    withheld.extend(work.turn_inputs.iter().flat_map(|drive| {
                        drive
                            .completion()
                            .input_ids
                            .clone()
                            .into_iter()
                            .map(QueuedRunMember::Input)
                    }));
                }
                let mut seen = std::collections::HashSet::new();
                withheld.retain(|member| seen.insert(member.clone()));
                let switched = matches!(prepared.outcome(), TurnOutcome::AgentFrameSwitch { .. });
                let cancelled = matches!(
                    prepared.outcome(),
                    TurnOutcome::Stopped(crate::TurnStop::Cancelled { .. })
                );
                let progress = if switched || (!cancelled && !withheld.is_empty()) {
                    // A switch advances the run to its follow-on, whose input
                    // is the head's pending follow-on, not a member row.
                    QueuedRunProgress::Advance {
                        position: run.position.next(&run.scope)?,
                        members: if switched {
                            Vec::new()
                        } else {
                            withheld.clone()
                        },
                        withheld_members: if switched { withheld } else { Vec::new() },
                    }
                } else {
                    QueuedRunProgress::Settle {
                        terminal: QueuedRunTerminal::Completed {
                            turn_id: trace_turn_id.clone(),
                            outcome: prepared.outcome().clone(),
                        },
                    }
                };
                Ok::<_, crate::StoreError>(QueuedRunCommit {
                    scope: run.scope.clone(),
                    expected_revision: run.revision,
                    progress,
                })
            })
            .transpose()
            .map_err(runtime_error_from_store_commit)?
            .map(Box::new);
        let release_session_execution_lease =
            release_session_execution_lease && queued_run.is_none();
        // Under an admitted root, the commit presents the root's drive fence
        // and, when this turn ends the root, writes its terminal evidence
        // (FIG-3600 S7).
        let drive_commit = self.drive_root.as_ref().and_then(|root| {
            let ends = match queued_run.as_deref() {
                Some(run) => {
                    if matches!(
                        run.progress,
                        crate::store::QueuedRunProgress::Settle {
                            terminal: crate::store::QueuedRunTerminal::Completed { .. }
                        }
                    ) {
                        crate::runtime::drive::RootEnd::Settles
                    } else {
                        crate::runtime::drive::RootEnd::Continues
                    }
                }
                None => crate::runtime::drive::RootEnd::Unless {
                    owes_follow_on: commit_effects.pending_follow_on.is_some()
                        || claims.carries_follow_on_work(matches!(
                            prepared.outcome(),
                            TurnOutcome::Stopped(TurnStop::Cancelled { .. })
                        )),
                },
            };
            root.commit_facts(&trace_turn_id, prepared.outcome(), ends)
        });
        let writes_root_terminal = drive_commit
            .as_ref()
            .is_some_and(|(_, terminal)| terminal.is_some());
        let mut prepared = prepared;
        prepared.turn_pipeline.set_drive_commit(drive_commit);
        // The commit clears the park of the root the turn runs under, the
        // same root an abort of the turn parks (D2 §1.3 P3).
        prepared.turn_pipeline.set_park_root(self.park_root(
            scoped_effect_controller.execution_scope().logical_root(),
            &trace_turn_id,
        ));
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
                    queued_run,
                    session_execution_lease,
                    release_session_execution_lease,
                    trace_turn_id: &trace_turn_id,
                    recorded_attachment_intent_ids: self
                        .host
                        .core
                        .durability
                        .attachment_store
                        .recorded_turn_intent_ids(&trace_turn_id),
                    interrupted_turn_input_cancellation: cancellation.clone(),
                    interrupted_turn_cancel_intent,
                    turn_cancel_closure_settlement,
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
                if writes_root_terminal && let Some(root) = self.drive_root.as_mut() {
                    root.mark_terminal_written();
                }
                committed
            }
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
            && let Some(session) = self.session.as_mut()
        {
            let protocol_session = Arc::clone(session.plugins().protocol_session());
            let session_id = self.state.session_id.clone();
            let restore_result = match crate::plugin::ProtocolSessionRestoreView::new(&self.state) {
                Ok(view) => {
                    protocol_session
                        .restore_session(
                            crate::plugin::ProtocolSessionContext::new(session, &session_id),
                            view,
                        )
                        .await
                }
                Err(error) => Err(crate::SessionError::Protocol(error.to_string())),
            };
            if let Err(err) = restore_result {
                delivery.turn.errors.push(post_commit_delivery_issue(
                    crate::TurnFailureCode::ProtocolRestoreSession.into(),
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
            lease:
                TurnLeaseScope {
                    guard: session_execution_lease,
                    release_policy: session_execution_lease_release_policy,
                },
            turn_control,
            turn_index,
            trace_turn_id,
            observer,
        } = context;
        let TurnDriverRemainder {
            policy,
            mut recorded_assembly,
            turn_pipeline,
            mut pending_queue_claims,
            pending_turn_input_claims,
            withheld_terminal_work,
            turn_cancel,
            ..
        } = driver;
        // Only a recorded cancellation reaches this finisher; lash's own
        // evidence stands in for none (FIG-3672 P9).
        let evidence = turn_cancel.unwrap_or_else(|| turn_control.internal_evidence(None));
        emit_terminal_sequence(
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
        // settles withheld turn input through the cancellation's undelivered
        // disposition (FIG-3531), handed over directly rather than inferred
        // from the committed outcome; withheld queued work settles with the
        // turn, as it always has.
        let crate::runtime::logical_turn::WithheldTerminalWork {
            queued: withheld_queue_claims,
            turn_inputs: withheld_turn_inputs,
        } = withheld_terminal_work;
        pending_queue_claims.extend(withheld_queue_claims);
        let claims = LogicalTurnClaims::new(pending_queue_claims, pending_turn_input_claims)
            .with_undelivered_turn_inputs(withheld_turn_inputs);
        Box::pin(self.finish_turn(TurnCommitContext {
            finish: TurnFinishInput {
                turn_pipeline,
                recorded_assembly,
                new_messages: cancellation_messages,
                policy: policy.policy,
                turn_index,
                trace_turn_id,
            },
            claims: &claims,
            scoped_effect_controller: finish_scoped_effect_controller,
            honoured_cancel: Some(evidence),
            lease: TurnLeaseScope {
                guard: session_execution_lease,
                release_policy: session_execution_lease_release_policy,
            },
            turn_control,
            observer,
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

        let Some(trace_outcome) = trace_outcome(outcome) else {
            return;
        };
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
            code,
            message,
            trace_turn_id,
            delivered_task,
            sinks: TurnSinks { observer },
            scoped_effect_controller,
            claims,
            session_execution_lease,
        } = context;
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
        emit_terminal_sequence(
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
                }
            })
            .into_iter()
            .collect();
        let messages = crate::MessageSequence::from_base_and_delta(
            self.state
                .read_model()
                .map_err(|error| {
                    RuntimeError::new(RuntimeErrorCode::ContextPrepareTurn, error.to_string())
                })?
                .messages,
            delivered,
        );
        // The terminal commits under its own turn's scope, so a follow-on's
        // terminal is that follow-on's commit and clears its fact.
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
                recorded_assembly,
                new_messages: messages,
                policy: self.state.effective_policy().clone(),
                // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                turn_index: self.state.turn_index + 1,
                trace_turn_id,
            },
            claims: &claims,
            scoped_effect_controller: &scoped_effect_controller,
            honoured_cancel: None,
            lease: TurnLeaseScope {
                guard: session_execution_lease,
                release_policy: SessionExecutionLeaseReleasePolicy::KeepOnAgentFrameSwitch,
            },
            turn_control: &turn_control,
            observer,
        }))
        .await;
        if let Err(err) = &finish_result
            && self.queued_run.is_none()
        {
            self.abandon_queued_work_claims_after_local_abort(err, &claims.queued)
                .await;
            self.abandon_turn_input_claims_after_local_abort(err, &claims.turn_inputs)
                .await;
        }
        finish_result
    }
}
