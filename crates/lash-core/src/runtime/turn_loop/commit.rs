//! The commit phase: finalize the assembled turn and execute the head-advancing
//! commit that makes the turn durable. Model calls retain reported usage
//! in their journaled results, independently of this commit (ADR 0127).
//!
//! The phase types are consumed in sequence and each transition takes the
//! previous one by value, so a committed turn cannot be adopted twice and
//! post-commit delivery cannot start before adoption.

use super::*;
use crate::ActorContext;
use crate::TurnId;
use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;

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

fn opener_messages(turn: &TurnId, facts: Vec<crate::PluginMessage>) -> Vec<Message> {
    let mut appended = Vec::new();
    for (ordinal, fact) in facts.into_iter().enumerate() {
        if !matches!(fact.role, MessageRole::User | MessageRole::System) {
            continue;
        }
        let id = format!("{turn}:opener-end:{ordinal}");
        let mut parts = fact.parts;
        reassign_part_ids(&id, &mut parts);
        appended.push(Message {
            reply_marker: None,
            id,
            role: fact.role,
            parts: shared_parts(parts),
            origin: fact.origin.or_else(|| {
                Some(crate::MessageOrigin::Plugin {
                    plugin_id: "plugin".to_string(),
                    transient: false,
                })
            }),
        });
    }
    appended
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
    pub(super) segment_boundary: Option<Box<crate::runtime::turn_driver::BoundaryTaken>>,
}

struct PreparedTurn {
    turn_pipeline: TurnBoundary,
    turn: AssembledTurn,
    events: Vec<SessionStreamEvent>,
}

/// What the final commit writes: the session it advances and the ingress
/// settlement it carries under its run's shift fence.
struct TurnCommitRequest<'commit, 'run> {
    session: Option<&'commit mut Session>,
    commit_effects: super::logical_turn::LogicalTurnCommitEffects,
    trace_turn_id: &'commit TurnId,
    recorded_attachment_intent_ids: std::collections::BTreeSet<crate::AttachmentId>,
    interrupted_turn: Option<crate::store::InterruptedTurnClosure>,
    turn_control_resolver: &'commit ActorContext,
    admissions: &'commit LogicalTurnAdmissions,
    opener: Option<crate::runtime::turn_driver::OpenerForCommit<'run>>,
    attachment_store: &'commit crate::RuntimeAttachmentStore,
    attachment_source_policy: &'commit dyn crate::AttachmentSourcePolicy,
}

/// The local commit-admission handles: only the head-advancing attempt uses
/// them, and they are dropped when the store needs no admission.
struct TurnCommitAdmission {
    turn_phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
}

impl PreparedTurn {
    fn outcome(&self) -> &TurnOutcome {
        &self.turn.outcome
    }

    async fn commit(
        self,
        request: TurnCommitRequest<'_, '_>,
        admission: TurnCommitAdmission,
    ) -> Result<CommittedTurn, crate::StoreError> {
        let TurnCommitAdmission { turn_phase_probe } = admission;
        let has_durable_store = request
            .session
            .as_deref()
            .and_then(Session::history_store)
            .is_some();
        if !has_durable_store {
            return Box::pin(self.commit_after_admission(request)).await;
        }
        let session_id = self.turn_pipeline.state().session_id.clone();
        let work_identity = request.trace_turn_id.to_string();
        Box::pin(super::run_head_advancing_commit_attempt(
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
        ))
        .await
    }

    async fn commit_after_admission(
        mut self,
        request: TurnCommitRequest<'_, '_>,
    ) -> Result<CommittedTurn, crate::StoreError> {
        let TurnCommitRequest {
            mut session,
            mut commit_effects,
            trace_turn_id: _,
            recorded_attachment_intent_ids,
            mut interrupted_turn,
            turn_control_resolver,
            admissions,
            mut opener,
            attachment_store,
            attachment_source_policy,
        } = request;
        let work_remaining = loop {
            match Box::pin(self.turn_pipeline.final_commit(
                &mut self.turn,
                session.as_deref_mut(),
                commit_effects.ingress_settlement.clone(),
                commit_effects.pending_follow_on.clone(),
                interrupted_turn.clone(),
                Some(turn_control_resolver),
                recorded_attachment_intent_ids.clone(),
            ))
            .await
            {
                Ok(remaining) => break remaining,
                Err(error @ crate::StoreError::TurnCancelIntentChanged { .. }) => {
                    let Some(interrupted) = interrupted_turn.as_mut() else {
                        return Err(error);
                    };
                    let Some(store) = session.as_deref().and_then(Session::history_store) else {
                        return Err(error);
                    };
                    self.turn_pipeline
                        .refresh_cancelled_commit(&mut self.turn, interrupted, &store)
                        .await?;
                    if let Some(opener) = opener.take() {
                        let mut facts = opener.close().await.map_err(|error| {
                            crate::StoreError::TurnOutcomeMaterializationRefused {
                                error: Box::new(error),
                            }
                        })?;
                        crate::runtime::turn_driver::normalize_plugin_message_attachments(
                            &mut facts,
                            attachment_store,
                            attachment_source_policy,
                        )
                        .await
                        .map_err(|error| {
                            crate::StoreError::TurnOutcomeMaterializationRefused {
                                error: Box::new(error),
                            }
                        })?;
                        self.turn_pipeline
                            .append_cancelled_opener_messages(&opener_messages(
                                interrupted.turn_id(),
                                facts,
                            ));
                        self.turn.state = self.turn_pipeline.state().to_snapshot();
                    }
                    commit_effects = admissions.commit_effects(&self.turn.outcome, None);
                }
                Err(error) => return Err(error),
            }
        };
        Ok(CommittedTurn {
            turn: self.turn,
            events: self.events,
            resident_state: self.turn_pipeline.into_final_state(),
            work_remaining,
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
    work_remaining: bool,
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
        if let Some(run) = runtime.shift_run.as_mut() {
            run.work_remaining = self.work_remaining;
        }
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
    pub(in crate::runtime) opener: Option<crate::runtime::turn_driver::OpenerForCommit<'run>>,
    pub(in crate::runtime) admissions: &'commit LogicalTurnAdmissions,
    pub(in crate::runtime) scoped_effect_controller: &'commit ActorContext,
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
    pub(super) opener: crate::runtime::turn_driver::OpenerForCommit<'run>,
    pub(super) cancellation_messages: crate::MessageSequence,
    pub(super) finish_scoped_effect_controller: &'cancel ActorContext,
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
    pub(in crate::runtime) scoped_effect_controller: ActorContext,
    pub(in crate::runtime) admissions: LogicalTurnAdmissions,
    pub(in crate::runtime) shift_fence: Option<&'error ShiftFence>,
    /// The lifetime this value is bound to; the context it carries is `'static`.
    pub(crate) run: std::marker::PhantomData<&'run ()>,
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
        let binding = turn_control_binding(&controller).await?;
        let control = ActiveTurnControl::new(
            binding.resolver(),
            TurnAddress::new(&self.state.session_id, &run),
        )
        .await?;
        let (observer, _observations) = TurnObserver::open(
            opts.events_or_noop(),
            opts.turn_events_or_noop(),
            self.delta_framing(),
        );
        let admissions = LogicalTurnAdmissions::new(Vec::new(), Vec::new());
        let pipeline = TurnBoundary::from_state_with_clock(
            self.state.clone(),
            Arc::clone(&self.host.core.clock),
            self.state.turn_scope(&run),
            self.host.core.durability.commit_budget,
        )
        .with_fleet_format(self.fleet_format())
        .with_metrics(self.host.core.tracing.metrics().clone())
        .with_trace(
            self.host
                .core
                .tracing
                .shift(controller.trace_scope().cloned(), &controller),
        );
        self.finish_turn(TurnCommitContext {
            opener: None,
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

    fn recover_terminal_opener<'run>(
        &self,
        pipeline: &TurnBoundary,
        turn: &TurnId,
        controller: &ActorContext,
        fence: Option<&ShiftFence>,
    ) -> Result<Option<crate::runtime::turn_driver::OpenerForCommit<'run>>, RuntimeError> {
        let Some(continuation) = pipeline
            .state()
            .pending_follow_on
            .as_deref()
            .filter(|owed| owed.is_turn(turn))
            .and_then(|owed| owed.owes.continuation())
        else {
            return Ok(None);
        };
        let opener = crate::session::OpenerState::from_snapshot_for(
            continuation.opener.clone(),
            &crate::EffectOpener::turn(
                self.state.session_id.clone(),
                crate::store::PhysicalTurn::split_turn_id(turn).0,
            ),
        )
        .map_err(crate::RuntimeEffectControllerError::into_runtime_error)?;
        if !opener.holds_tool_run() {
            return Ok(None);
        }
        let services = self
            .runtime_session_services_for_turn(fence, pipeline.graph_appends())
            .map_err(|error| {
                RuntimeError::new(RuntimeErrorCode::PluginSessionManager, error.to_string())
            })?;
        let session = self.session.as_ref().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::ExecutionStateCaptureFailed,
                "terminal opener has no session",
            )
        })?;
        let frame = pipeline
            .state()
            .current_frame_node_id
            .clone()
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::ExecutionStateCaptureFailed,
                    "terminal opener has no agent frame",
                )
            })?;
        let messages = crate::tool_dispatch::CheckpointMessageBuffer::default();
        let context = session
            .code_execution_context(
                &self.state.session_id,
                frame,
                services.state_service(),
                services.lifecycle_service(),
                services.graph_service(),
                services.model_tool_process_service(),
                controller.clone(),
                services.direct_completion_client(controller.clone(), Some(turn.clone())),
                services.trigger_router(),
                services.process_engines().clone(),
                crate::engine::NullObservationSink::arc(),
                Arc::new(crate::ChronologicalProjection::default()),
                crate::TurnContext::default(),
                pipeline
                    .state()
                    .process_execution_env_spec(pipeline.state().effective_policy()),
                messages.clone(),
                Arc::clone(&self.host.core.attachment_source_policy),
            )
            .map_err(|error| {
                RuntimeError::new(
                    RuntimeErrorCode::ToolCatalogResolutionFailed,
                    error.to_string(),
                )
            })?
            .with_opener_state(opener)
            .with_turn_cancel_scope(crate::ExecutionScope::turn(
                self.state.session_id.clone(),
                turn.clone(),
            ));
        Ok(Some(crate::runtime::turn_driver::OpenerForCommit {
            context: Some(context),
            messages,
        }))
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
            mut opener,
            admissions,
            scoped_effect_controller,
            honoured_cancel,
            shift_fence,
            turn_control,
            observer,
        } = context;
        let turn_control_binding = turn_control_binding(scoped_effect_controller).await?;
        let turn_control_resolver = turn_control_binding.resolver();
        let turn_control_binding_id = turn_control_binding.binding_id().to_string();
        let TurnFinishInput {
            mut turn_pipeline,
            recorded_assembly: assembly,
            mut new_messages,
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
        let interrupted_turn_cancel_intent =
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
        let admitted_cancel_intent = self
            .shift_run
            .as_ref()
            .and_then(|execution| execution.cancel_intent.clone());
        let turn_cancel_closure_authorization = match (
            self.session.as_ref().and_then(Session::history_store),
            shift_fence,
            interrupted_turn_cancel_intent.clone(),
        ) {
            (Some(_store), Some(fence), Some(observed)) => {
                let address = crate::TurnAddress::new(&self.state.session_id, &trace_turn_id);
                let admitted_scope = crate::runtime::effect::executor::admitted_turn_cancel_scope(
                    &address,
                    scoped_effect_controller.execution_scope(),
                    &turn_control_binding_id,
                );
                if admitted_cancel_intent.is_none() {
                    return Err(runtime_error_from_store_commit(
                        crate::StoreError::TurnCancelClosureAuthorizationMismatch {
                            session_id: address.session_id.clone(),
                            turn_id: address.turn_id.clone(),
                        },
                    ));
                }
                let durable = observed
                    .request()
                    .map(crate::TurnCancelRequest::evidence)
                    .or_else(|| assembled_cancellation.clone());
                Some(turn_control.closure_authorization(
                    &turn_control_binding_id,
                    admitted_scope,
                    fence,
                    observed,
                    honoured_cancel.as_ref(),
                    durable,
                )?)
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
                        .settle_admitted_intent(authorization.clone(), honoured_cancel.as_ref()),
                    observed_intent,
                    admitted_intent: admitted_cancel_intent.clone(),
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
        // Snapshot-bearing admissions derive interruption from durable intent.
        // The final transaction validates that intent with the fence and head CAS.
        let interrupted = cancellation.is_some();
        if segment_boundary.is_none() || interrupted {
            let opener = match opener {
                Some(_) => opener.take(),
                None => self.recover_terminal_opener(
                    &turn_pipeline,
                    &trace_turn_id,
                    scoped_effect_controller,
                    shift_fence,
                )?,
            };
            if let Some(opener) = opener {
                let mut facts = opener.close().await?;
                crate::runtime::turn_driver::normalize_plugin_message_attachments(
                    &mut facts,
                    self.host.core.durability.attachment_store.as_ref(),
                    self.host.core.attachment_source_policy.as_ref(),
                )
                .await?;
                if !facts.is_empty() && new_messages.is_empty() {
                    new_messages = turn_pipeline.message_sequence();
                }
                new_messages.extend(opener_messages(&trace_turn_id, facts));
            }
        }

        turn_pipeline.finalize_turn_read_state(new_messages, interrupted);
        let turn_trace = self
            .host
            .core
            .tracing
            .turn_execution(scoped_effect_controller);
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
                segment_boundary.as_deref(),
            )?;
            self.state.adopt_snapshot(assembled.state.clone());
            self.state.pending_follow_on = pending_follow_on.map(Box::new);
            let observation_revision =
                crate::runtime::observation::observation_revision(&self.state);
            self.resident_session
                .record_committed_observation_turn(observation_revision.as_u64(), &trace_turn_id);
            turn_trace.conclude();
            observer.release_terminal();
            observer.published().await;
            publish_terminal_after_commit(
                turn_control,
                turn_control_resolver,
                &TurnTerminal::committed(&assembled.outcome),
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
        // The after-turn callbacks run as one recorded step: their decisions
        // and the resolutions of their state commands are served from the
        // journal on replay, and no callback runs again (K10).
        let recorded = if plugins.has_after_turn_hooks() {
            let hook_context = crate::plugin::TurnResultHookContext {
                writer_formats: Arc::new(crate::protocol_build::FleetWriterFormats(
                    self.fleet_format(),
                )),
                session_id: assembled.state.session_id.clone(),
                plugin_config: plugins.admitted_plugin_config(),
                turn: Arc::new(crate::plugin::TurnHookReport::from_assembled(&assembled)),
                sessions: manager.read_service(),
            };
            let callbacks = Arc::clone(&plugins);
            let probe = self.turn_phase_probe.clone();
            let recorded = Box::pin(crate::plugin::record_plugin_callbacks(
                scoped_effect_controller,
                crate::RuntimeAttribution::for_session(assembled.state.session_id.clone()),
                format!("plugin-callbacks:after-turn:{trace_turn_id}"),
                crate::plugin::RecordedCallbackPhase::AfterTurn,
                Arc::clone(&plugins),
                Box::pin(async move {
                    callbacks
                        .dispatch(probe.as_ref())
                        .after_turn_decisions(hook_context)
                        .await
                }),
            ))
            .await;
            match recorded {
                Ok(Ok(recorded)) => recorded,
                Ok(Err(err)) => {
                    self.mark_phase_end(PreparedTurn::RUNTIME_PHASE);
                    return Err(err.into_turn_failure(RuntimeErrorCode::PluginFinalizeTurn));
                }
                Err(err) => {
                    self.mark_phase_end(PreparedTurn::RUNTIME_PHASE);
                    return Err(err.into_runtime_error());
                }
            }
        } else {
            Vec::new()
        };
        let session_contributions = recorded
            .iter()
            .map(|item| item.session.clone())
            .collect::<Vec<_>>();
        turn_pipeline
            .graph_appends()
            .apply_session_contributions(
                &assembled.state.session_id,
                &plugins,
                &session_contributions,
            )
            .map_err(|err| err.into_turn_failure(RuntimeErrorCode::PluginFinalizeTurn))?;
        let finalized = plugins
            .dispatch(self.turn_phase_probe.as_ref())
            .finalize_turn(
                assembled,
                recorded,
                &trace_turn_id,
                self.services.clock.as_ref(),
            )
            .await;
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
            segment_boundary.as_deref(),
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
                    admissions,
                    opener,
                    attachment_store: self.host.core.durability.attachment_store.as_ref(),
                    attachment_source_policy: self.host.core.attachment_source_policy.as_ref(),
                },
                TurnCommitAdmission {
                    turn_phase_probe: self.turn_phase_probe.clone(),
                },
            ),
        )
        .await
        {
            Ok(committed) => {
                if (writes_run_terminal
                    || matches!(
                        committed.turn.outcome,
                        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
                    ))
                    && let Some(run) = self.shift_run.as_mut()
                {
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
        let cancellation = match &committed.turn.outcome {
            TurnOutcome::Stopped(TurnStop::Cancelled { evidence }) => Some(evidence.clone()),
            _ => cancellation,
        };
        let mut delivery = committed.adopt(self, &trace_turn_id)?;
        self.mark_phase_end(CommittedTurn::RUNTIME_PHASE);
        self.mark_phase_begin(PostCommitDelivery::RUNTIME_PHASE);

        observer.release_terminal();
        emit_session_events(observer, delivery.events);
        observer.published().await;
        if admitted_cancel_intent.is_some()
            && let Err(error) = turn_control
                .notify_committed_cancellation(turn_control_resolver, cancellation.clone())
                .await
        {
            delivery.turn.errors.push(post_commit_delivery_issue(
                (&error.code).into(),
                error.to_string(),
            ));
        }
        publish_terminal_after_commit(
            turn_control,
            turn_control_resolver,
            &TurnTerminal::committed(&delivery.turn.outcome),
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
        match self
            .emit_turn_persisted_event(&delivery.turn, shift_fence)
            .await
        {
            Ok(Some(error)) => {
                let mut issue = crate::plugin::plugin_lifecycle_hook_issue(error);
                issue.retryable = Some(false);
                delivery.turn.errors.push(issue);
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

        turn_trace.conclude();
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
            opener,
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
            opener: Some(opener),
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

    pub(in crate::runtime) async fn finish_logical_turn_error(
        &mut self,
        context: LogicalTurnErrorContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let LogicalTurnErrorContext {
            run: std::marker::PhantomData,
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
        let turn_control_binding = turn_control_binding(&scoped_effect_controller).await?;
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
                activity: TerminalActivityTarget {
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
        .with_fleet_format(self.fleet_format())
        .with_definition_engines(self.host.core.process_engines.clone())
        .with_metrics(self.host.core.tracing.metrics().clone())
        .with_trace(self.host.core.tracing.shift(
            scoped_effect_controller.trace_scope().cloned(),
            &scoped_effect_controller,
        ));
        turn_pipeline.apply_prepared_messages(&messages);
        Box::pin(self.finish_turn(TurnCommitContext {
            opener: None,
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
