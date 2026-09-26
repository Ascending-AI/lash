//! The execute phase: run the prepared turn's effect loop to a terminal
//! outcome while its observations are published to the host sinks outside the
//! drive. Cancellation reaches the loop only as recorded facts: the journaled
//! gate peeks and the recorded outcomes of its steps (FIG-3672 P9).

use super::*;
use crate::TurnId;

struct TurnDriverSessionLoan<'slot, 'run> {
    session: &'slot mut Option<Session>,
    driver: Option<Box<RuntimeTurnDriver<'run>>>,
}

pub(super) struct TurnDriverRemainder {
    pub(super) policy: RuntimeSessionPolicy,
    pub(super) recorded_assembly: RecordedTurnAssembly,
    pub(super) turn_pipeline: TurnBoundary,
    pub(super) llm_calls: Vec<crate::LlmCallRecord>,
    pub(super) failure_evidence: Vec<crate::TurnFailureEvidence>,
    pub(super) pending_queue_claims: Vec<crate::QueuedWorkClaim>,
    pub(super) pending_turn_input_claims: Vec<crate::TurnInputClaim>,
    pub(super) withheld_terminal_work: crate::runtime::logical_turn::WithheldTerminalWork,
    /// The cancellation the turn recorded honouring, if any.
    pub(super) turn_cancel: Option<crate::TurnCancellationEvidence>,
}

/// Everything the execute phase needs to drive an already-prepared turn.
///
/// The prepare phase (and the host-prepared entry point) builds one of these;
/// the execute phase consumes it whole.
pub(in crate::runtime) struct PreparedTurnExecuteContext<'sinks, 'run> {
    pub(in crate::runtime) turn: PreparedLogicalTurn,
    pub(in crate::runtime) sinks: TurnSinks<'sinks>,
    pub(in crate::runtime) scoped_effect_controller: ScopedEffectController<'run>,
    pub(in crate::runtime) local_stop: LocalTurnStop,
    pub(in crate::runtime) initial_queue_claims: Vec<crate::QueuedWorkClaim>,
    pub(in crate::runtime) initial_turn_input_claims: Vec<crate::TurnInputClaim>,
    pub(in crate::runtime) lease: TurnLeaseScope<'sinks>,
}

/// The preamble step of the execute phase: the plugin prepare-turn hooks and
/// the context transform that produce the message sequence the driver runs.
struct TurnPreambleContext<'preamble> {
    plugins: &'preamble crate::PluginSession,
    manager: &'preamble Arc<RuntimeSessionServices>,
    messages: crate::MessageSequence,
    turn_policy: &'preamble crate::SessionPolicy,
    effective_protocol_turn_options: &'preamble crate::ProtocolTurnOptions,
    turn_context: &'preamble crate::TurnContext,
    turn_scope_id: &'preamble str,
}

/// The state a plugin abort inherits from the preamble, so the abort commit can
/// run on its own async frame without carrying the preamble's read view.
struct PreparedTurnAbortContext<'abort, 'run> {
    prepared: crate::plugin::TurnPreparation,
    recorded_assembly: RecordedTurnAssembly,
    turn_index: usize,
    trace_turn_id: TurnId,
    claims: &'abort LogicalTurnClaims,
    scoped_effect_controller: &'abort ScopedEffectController<'run>,
    lease: TurnLeaseScope<'abort>,
    session_execution_fence: Option<crate::SessionExecutionLeaseAuthority>,
    turn_control: &'abort ActiveTurnControl,
    turn_graph_appends: TurnGraphAppendDraft,
    observer: &'abort TurnObserver,
}

/// The effect loop's own inputs: the driver, the observer it publishes
/// through, and the turn-control handle its start-gate peek runs against.
struct TurnEffectLoopContext<'loop_run, 'run> {
    driver: &'loop_run mut RuntimeTurnDriver<'run>,
    messages: crate::MessageSequence,
    event_tx: TurnObserver,
    protocol_run_offset: usize,
    turn_control: Arc<ActiveTurnControl>,
    cancel_controller: &'loop_run ScopedEffectController<'run>,
}

impl<'slot, 'run> TurnDriverSessionLoan<'slot, 'run> {
    fn new(session: &'slot mut Option<Session>, driver: Box<RuntimeTurnDriver<'run>>) -> Self {
        Self {
            session,
            driver: Some(driver),
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "the turn driver loan is present for the loan's lifetime"
    )]
    fn reclaim(mut self) -> TurnDriverRemainder {
        let RuntimeTurnDriver {
            session,
            policy,
            recorded_assembly,
            turn_pipeline,
            llm_calls,
            failure_evidence,
            pending_queue_claims,
            pending_turn_input_claims,
            withheld_terminal_work,
            turn_cancel,
            ..
        } = *self.driver.take().expect("turn driver loan is present");
        *self.session = Some(session);
        TurnDriverRemainder {
            policy,
            recorded_assembly,
            turn_pipeline,
            llm_calls,
            failure_evidence,
            pending_queue_claims,
            pending_turn_input_claims,
            withheld_terminal_work,
            turn_cancel,
        }
    }
}

impl<'slot, 'run> std::ops::Deref for TurnDriverSessionLoan<'slot, 'run> {
    type Target = RuntimeTurnDriver<'run>;

    #[expect(
        clippy::expect_used,
        reason = "the turn driver loan is present for the loan's lifetime"
    )]
    fn deref(&self) -> &Self::Target {
        self.driver.as_deref().expect("turn driver loan is present")
    }
}

impl std::ops::DerefMut for TurnDriverSessionLoan<'_, '_> {
    #[expect(
        clippy::expect_used,
        reason = "the turn driver loan is present for the loan's lifetime"
    )]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.driver
            .as_deref_mut()
            .expect("turn driver loan is present")
    }
}

impl Drop for TurnDriverSessionLoan<'_, '_> {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.take() {
            // Re-arm the still-owned driver as a loan and consume it through
            // reclaim(); the inner loan's Drop is a no-op because reclaim()
            // takes the driver.
            let loan = TurnDriverSessionLoan::new(&mut *self.session, driver);
            drop(loan.reclaim());
        }
    }
}

async fn run_turn_effect_loop(
    context: TurnEffectLoopContext<'_, '_>,
) -> Result<(crate::MessageSequence, usize), RuntimeError> {
    let TurnEffectLoopContext {
        driver,
        messages,
        event_tx,
        protocol_run_offset,
        turn_control,
        cancel_controller,
    } = context;
    // The start gate can change the handler's control flow before its first
    // effect, so it is observed through the handler-scoped controller, which
    // journals the observation and replays the same answer after an owner
    // crash. It is the loop's only look at the gate before its steps: from
    // here on a cancellation reaches the loop only through its steps'
    // recorded outcomes and its journaled boundary peeks (FIG-3672 P9).
    //
    // The observation is attempted once (FIG-3647): an error it returns is
    // final. On Restate the peek is a call that surfaces only terminal
    // errors, because Restate retries transient failures itself, and each
    // further call would be a fresh peek of the same terminal cause. On the
    // SQL engines the failure is sealed under the gate's replay key, so a
    // second attempt would replay it. A fault outside the journal aborts the
    // turn for the substrate to re-drive.
    let start_gate = crate::runtime::RuntimeNamedPhase::begin(
        driver.turn_phase_probe.clone(),
        "turn_cancel.start_gate",
    );
    let pending_cancel = turn_control
        .observe_pending_cancel(
            cancel_controller,
            crate::runtime::turn_control::TurnCancelPeekIdentity::StartGate,
        )
        .await?;
    drop(start_gate);
    if let Some(evidence) = pending_cancel {
        driver.record_turn_cancel(evidence);
    }
    // Canonical future-size seam: `driver.run` is boxed exactly once here.
    // Driver growth is absorbed by this allocation instead of accreting
    // opportunistic boxes through the callers below.
    Box::pin(driver.run(messages, event_tx, protocol_run_offset)).await
}

impl LashRuntime {
    async fn prepare_turn_preamble(
        &mut self,
        context: TurnPreambleContext<'_>,
    ) -> Result<crate::plugin::TurnPreparation, RuntimeError> {
        let TurnPreambleContext {
            plugins,
            manager,
            messages,
            turn_policy,
            effective_protocol_turn_options,
            turn_context,
            turn_scope_id,
        } = context;
        self.mark_phase_begin(RuntimeTurnPhase::BeforeTurnHooks);
        let prepare_turn = plugins.prepare_turn_with_phase_probe(
            PrepareTurnRequest {
                session_id: self.state.session_id.clone(),
                state: crate::SessionReadView::from_runtime_state(
                    &self.state,
                    turn_policy.clone(),
                    effective_protocol_turn_options.clone(),
                )
                .map_err(|error| {
                    RuntimeError::new(RuntimeErrorCode::ContextPrepareTurn, error.to_string())
                })?,
                messages,
                sessions: manager.state_service(),
                session_graph: manager.graph_service(),
                turn_context: turn_context.clone(),
            },
            self.turn_phase_probe.clone(),
            turn_scope_id,
        );
        let prepared = Box::pin(prepare_turn)
            .await
            .map_err(|err| err.into_turn_failure(RuntimeErrorCode::PluginPrepareTurn))?;
        self.mark_phase_end(RuntimeTurnPhase::BeforeTurnHooks);
        Ok(prepared)
    }

    async fn finish_prepared_turn_abort(
        &mut self,
        context: PreparedTurnAbortContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let PreparedTurnAbortContext {
            prepared,
            mut recorded_assembly,
            turn_index,
            trace_turn_id,
            claims,
            scoped_effect_controller,
            lease:
                TurnLeaseScope {
                    guard: session_execution_lease,
                    release_policy: session_execution_lease_release_policy,
                },
            session_execution_fence: _session_execution_fence,
            turn_control,
            turn_graph_appends,
            observer,
        } = context;
        let Some(abort) = prepared.abort else {
            unreachable!("abort finisher requires a prepared plugin abort");
        };

        // The preparation future and its SessionReadView are gone before this state clone.
        // That keeps the graph from being held twice while the turn boundary takes ownership
        // of its working state.
        let mut turn_pipeline = TurnBoundary::from_state_with_graph_appends(
            self.state.clone(),
            Arc::clone(&self.host.core.clock),
            self.state.turn_scope(&trace_turn_id),
            self.host.core.durability.commit_budget,
            turn_graph_appends,
        );
        turn_pipeline.apply_prepared_messages(&prepared.messages);
        emit_terminal_sequence(
            &mut recorded_assembly,
            observer,
            &mut turn_observation_cursor(scoped_effect_controller, &trace_turn_id, "terminal"),
            Some(TerminalDiagnostic {
                kind: TerminalDiagnosticKind::Plugin,
                code: Some(abort.code),
                message: abort.message,
                retryable: None,
                activity: TerminalActivityTarget::TurnScoped(observer),
            }),
            TurnStop::PluginAbort,
        );
        Box::pin(self.finish_turn(TurnCommitContext {
            finish: TurnFinishInput {
                turn_pipeline,
                recorded_assembly,
                new_messages: prepared.messages,
                policy: self.state.effective_policy().clone(),
                turn_index,
                trace_turn_id,
            },
            claims,
            scoped_effect_controller,
            honoured_cancel: None,
            lease: TurnLeaseScope {
                guard: session_execution_lease,
                release_policy: session_execution_lease_release_policy,
            },
            turn_control,
            observer,
        }))
        .await
    }

    pub(in crate::runtime) async fn stream_prepared_turn_inner(
        &mut self,
        context: PreparedTurnExecuteContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        // Host-prepared turns ran no prepare-turn hooks, so no in-turn graph
        // append can predate this draft.
        let turn_graph_appends = TurnGraphAppendDraft::from_resident_state(
            &self.state,
            Arc::clone(&self.host.core.clock),
        );
        self.stream_prepared_turn_inner_with_graph_appends(context, turn_graph_appends)
            .await
    }

    /// Run one prepared physical turn. Everything it publishes goes through
    /// the logical turn's observer, addressed to this turn.
    #[expect(
        clippy::expect_used,
        reason = "the runtime session is installed for the whole turn"
    )]
    pub(super) async fn stream_prepared_turn_inner_with_graph_appends(
        &mut self,
        context: PreparedTurnExecuteContext<'_, '_>,
        turn_graph_appends: TurnGraphAppendDraft,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let PreparedTurnExecuteContext {
            turn:
                PreparedLogicalTurn {
                    messages,
                    previous_prompt_usage,
                    protocol_turn_options,
                    protocol_extension,
                    turn_context,
                    initial_turn_causes,
                    trace_turn_id,
                    turn_index,
                },
            sinks: TurnSinks {
                observer: logical_observer,
            },
            scoped_effect_controller,
            local_stop,
            initial_queue_claims,
            initial_turn_input_claims,
            lease:
                TurnLeaseScope {
                    guard: session_execution_lease,
                    release_policy: session_execution_lease_release_policy,
                },
        } = context;
        let turn_observer = logical_observer.for_turn(&trace_turn_id);
        let observer = &turn_observer;
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
        // Host glue, not drive code: a host-local stop becomes a durable
        // request on this turn's gate for as long as the turn runs, and the
        // turn sees it only where it sees any request — its journaled peeks
        // and its steps' recorded outcomes.
        let _local_stop_forwarding = local_stop
            .forward_to(Arc::clone(&turn_control), Arc::clone(&turn_control_host))
            .await;
        let session_execution_fence =
            session_execution_lease.map(SessionExecutionLeaseGuard::fence);
        let turn_policy = self.state.effective_policy().clone();
        let session_protocol_turn_options = self.state.effective_protocol_turn_options().clone();
        let effective_protocol_turn_options = protocol_turn_options
            .clone()
            .map(|options| session_protocol_turn_options.merged_with_override(&options))
            .unwrap_or(session_protocol_turn_options);
        let manager = self
            .runtime_session_services_for_turn(session_execution_lease, &turn_graph_appends)
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::PluginSessionManager, err.to_string())
            })?;
        let plugins = {
            let session = self
                .session
                .as_ref()
                .expect("lash runtime session must be available");
            Arc::clone(session.plugins())
        };
        let mut recorded_assembly = RecordedTurnAssembly::new();
        let initial_claims =
            LogicalTurnClaims::new(initial_queue_claims, initial_turn_input_claims);
        // Keep preparation and plugin-abort handling in separate async frames.
        // Their SessionReadView and abort-only locals are dropped before the
        // normal driver-construction frame clones state for the turn boundary.
        let mut prepared = self
            .prepare_turn_preamble(TurnPreambleContext {
                plugins: plugins.as_ref(),
                manager: &manager,
                messages,
                turn_policy: &turn_policy,
                effective_protocol_turn_options: &effective_protocol_turn_options,
                turn_context: &turn_context,
                turn_scope_id: &trace_turn_id,
            })
            .await?;
        for event in &prepared.events {
            recorded_assembly.record(event);
        }
        emit_session_events(observer, std::mem::take(&mut prepared.events));
        if prepared.abort.is_some() {
            return Box::pin(self.finish_prepared_turn_abort(PreparedTurnAbortContext {
                prepared,
                recorded_assembly,
                turn_index,
                trace_turn_id,
                claims: &initial_claims,
                scoped_effect_controller: &scoped_effect_controller,
                lease: TurnLeaseScope {
                    guard: session_execution_lease,
                    release_policy: session_execution_lease_release_policy,
                },
                session_execution_fence,
                turn_control: turn_control.as_ref(),
                turn_graph_appends,
                observer,
            }))
            .await;
        }
        // `prepare_turn_preamble` has returned and dropped its read-view frame
        // before this clone, avoiding a transient second graph owner.
        // Restore the basis captured before preparation cleared the resident
        // value. TurnBoundary is the state persisted by a host continuation;
        // keeping this value stable prevents a mid-turn call from changing the
        // standard-compaction projection when the logical turn is redriven.
        self.state.last_prompt_usage = previous_prompt_usage;
        let mut turn_pipeline = TurnBoundary::from_state_with_graph_appends(
            self.state.clone(),
            Arc::clone(&self.host.core.clock),
            self.state.turn_scope(&trace_turn_id),
            self.host.core.durability.commit_budget,
            turn_graph_appends.clone(),
        );
        turn_pipeline
            .prepared_checkpoint(
                turn_policy.clone(),
                turn_index,
                &prepared.messages,
                self.session.as_mut(),
            )
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        // The route is the turn's recorded config (D3 §2.1); it was validated
        // when it was set, so a route this worker cannot bind is its
        // deployment, retried and never the turn's outcome (Q3).
        let resolved_turn_policy = self
            .host
            .resolve_session_policy(&self.state.session_id, turn_policy.clone())
            .map_err(crate::runtime::drive::provider_binding_unavailable)?;
        let manager = self
            .runtime_session_services_for_turn(session_execution_lease, &turn_graph_appends)
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::PluginSessionManager, err.to_string())
            })?;
        let finish_scoped_effect_controller = scoped_effect_controller.clone();
        let turn_cancel_peek_controller = &finish_scoped_effect_controller;
        let session = self
            .session
            .take()
            .expect("lash runtime session must be available");
        let driver = Box::new(RuntimeTurnDriver {
            session,
            policy: resolved_turn_policy,
            recorded_assembly,
            host: self.host.clone(),
            turn_id: trace_turn_id.clone(),
            scoped_effect_controller: scoped_effect_controller.clone(),
            session_id: self.state.session_id.clone(),
            turn_index,
            turn_pipeline,
            latest_prompt_usage: None,
            llm_calls: Vec::new(),
            failure_evidence: Vec::new(),
            session_services: manager,
            protocol_turn_options: effective_protocol_turn_options,
            protocol_extension,
            turn_context,
            turn_causes: initial_turn_causes,
            pending_queue_claims: initial_claims.queued,
            pending_turn_input_claims: initial_claims.turn_inputs,
            pending_checkpoint_turn_input_claim: None,
            withheld_terminal_work: Default::default(),
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            session_execution_lease: session_execution_fence,
            runtime_lease_owner: self.runtime_lease_owner.clone(),
            turn_phase_probe: self.turn_phase_probe.clone(),
            turn_control: Arc::clone(&turn_control),
            protocol_reply: Default::default(),
            live_opener: std::sync::Mutex::new(None),
            opener_state: crate::session::OpenerState::new(
                self.host.core.control.opener_work_bound,
            ),
            turn_cancel: None,
            children_stop: CancellationToken::new(),
            turn_observations: super::turn_observation_cursor(
                &scoped_effect_controller,
                &trace_turn_id,
                "drive",
            ),
        });
        let protocol_run_offset = 0;
        self.mark_phase_begin(RuntimeTurnPhase::EffectLoop);
        let mut driver = TurnDriverSessionLoan::new(&mut self.session, driver);
        let run_result = Box::pin(run_turn_effect_loop(TurnEffectLoopContext {
            driver: &mut driver,
            messages: prepared.messages,
            event_tx: observer.clone(),
            protocol_run_offset,
            turn_control: Arc::clone(&turn_control),
            cancel_controller: turn_cancel_peek_controller,
        }))
        .await;
        let (new_messages, _new_protocol_iteration) = match run_result {
            Ok(result) => result,
            Err(err) => {
                // The loop aborted. Whether the abort is the turn's
                // cancellation is decided from recorded facts only: the
                // cancellation the loop already recorded honouring, or — for
                // an abort a step's recorded outcome typed as a cancellation —
                // the journaled post-abort peek.
                let honoured =
                    match driver.turn_cancel.clone() {
                        Some(evidence) => Some(evidence),
                        None if aborted_by_turn_cancel(&err.code) => turn_control
                            .observe_pending_cancel(
                                turn_cancel_peek_controller,
                                crate::runtime::turn_control::TurnCancelPeekIdentity::PostAbortGate,
                            )
                            .await?,
                        None => None,
                    };
                if let Some(evidence) = honoured {
                    driver.record_turn_cancel(evidence);
                    let cancellation_messages = driver.turn_pipeline.message_sequence();
                    let driver = driver.reclaim();
                    self.mark_phase_end(RuntimeTurnPhase::EffectLoop);
                    return Box::pin(self.finish_cancelled_turn_after_effect_abort(
                        CancelledTurnFinishContext {
                            driver,
                            cancellation_messages,
                            finish_scoped_effect_controller: &finish_scoped_effect_controller,
                            lease: TurnLeaseScope {
                                guard: session_execution_lease,
                                release_policy: session_execution_lease_release_policy,
                            },
                            turn_control: turn_control.as_ref(),
                            turn_index,
                            trace_turn_id,
                            observer,
                        },
                    ))
                    .await;
                }
                let driver = driver.reclaim();
                self.mark_phase_end(RuntimeTurnPhase::EffectLoop);
                let TurnDriverRemainder {
                    mut pending_queue_claims,
                    mut pending_turn_input_claims,
                    withheld_terminal_work,
                    ..
                } = driver;
                // An aborted turn drives no follow-on, so work withheld from
                // its terminal checkpoint hands back with everything else.
                pending_queue_claims.extend(withheld_terminal_work.queued);
                pending_turn_input_claims.extend(withheld_terminal_work.turn_inputs);
                self.abandon_queued_work_claims_after_local_abort(&err, &pending_queue_claims)
                    .await;
                self.abandon_turn_input_claims_after_local_abort(&err, &pending_turn_input_claims)
                    .await;
                Box::pin(self.record_turn_park_after_abort(&err, &trace_turn_id)).await;
                return Err(err);
            }
        };
        let driver = driver.reclaim();
        self.mark_phase_end(RuntimeTurnPhase::EffectLoop);
        tracing::debug!(
            new_message_count = new_messages.len(),
            tool_call_count = driver.recorded_assembly.tool_calls.len(),
            "runtime post-run_task"
        );

        let TurnDriverRemainder {
            policy,
            recorded_assembly,
            turn_pipeline,
            llm_calls,
            failure_evidence,
            pending_queue_claims,
            pending_turn_input_claims,
            mut withheld_terminal_work,
            turn_cancel,
        } = driver;
        let pending_claims =
            LogicalTurnClaims::new(pending_queue_claims, pending_turn_input_claims)
                .with_withheld_terminal_work(withheld_terminal_work.take_if_any());
        let finish_result = Box::pin(
            self.finish_turn(TurnCommitContext {
                finish: TurnFinishInput {
                    turn_pipeline,
                    recorded_assembly: recorded_assembly
                        .with_llm_calls(llm_calls)
                        .with_failure_evidence(failure_evidence),
                    new_messages,
                    policy: policy.policy,
                    turn_index,
                    trace_turn_id,
                },
                claims: &pending_claims,
                scoped_effect_controller: &finish_scoped_effect_controller,
                honoured_cancel: turn_cancel,
                lease: TurnLeaseScope {
                    guard: session_execution_lease,
                    release_policy: session_execution_lease_release_policy,
                },
                turn_control: turn_control.as_ref(),
                observer,
            }),
        )
        .await;
        let mut pending_claims = pending_claims;
        if let Err(err) = &finish_result {
            if let Some(withheld) = pending_claims.withheld_terminal_work.take() {
                pending_claims.queued.extend(withheld.queued);
                pending_claims.turn_inputs.extend(withheld.turn_inputs);
            }
            self.abandon_queued_work_claims_after_local_abort(err, &pending_claims.queued)
                .await;
            self.abandon_turn_input_claims_after_local_abort(err, &pending_claims.turn_inputs)
                .await;
        }
        finish_result.map(|mut execution| {
            execution.withheld_terminal_work = pending_claims.take_follow_on_work(matches!(
                execution.turn.outcome,
                TurnOutcome::Stopped(TurnStop::Cancelled { .. })
            ));
            execution
        })
    }
}

/// Whether a loop abort is one a step's recorded outcome typed as the turn's
/// cancellation: a wait that lost to the turn's gate. Only such an abort asks
/// the gate whether the turn is cancelled (FIG-3672 P9).
fn aborted_by_turn_cancel(code: &RuntimeErrorCode) -> bool {
    matches!(
        code,
        RuntimeErrorCode::RuntimeEffectSleepCancelled
            | RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled
            | RuntimeErrorCode::TurnControlWaitCancelled
    )
}
