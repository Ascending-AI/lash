//! The execute phase: run the prepared turn's effect loop to a terminal
//! outcome while its observations are published to the host sinks outside the
//! shift. Cancellation reaches the loop only as recorded facts: the journaled
//! gate peeks and the recorded outcomes of its steps (FIG-3672 P9).

use super::*;
use crate::ActorContext;

struct TurnDriverSessionLoan<'slot, 'run> {
    session: &'slot mut Option<Session>,
    driver: Option<Box<RuntimeTurnDriver<'run>>>,
}

pub(super) struct TurnDriverRemainder {
    pub(super) recorded_assembly: RecordedTurnAssembly,
    pub(super) turn_pipeline: TurnBoundary,
    pub(super) llm_calls: Vec<crate::LlmCallRecord>,
    pub(super) failure_evidence: Vec<crate::TurnFailureEvidence>,
    pub(super) pending_queued: Vec<crate::AdmittedQueuedWork>,
    pub(super) pending_turn_inputs: Vec<crate::AdmittedTurnInputs>,
    pub(super) withheld_terminal_work: crate::runtime::logical_turn::WithheldTerminalWork,
    /// The cancellation the turn recorded honouring, if any.
    pub(super) turn_cancel: Option<crate::TurnCancellationEvidence>,
    /// The protocol iterations the run has spent through this turn, when the
    /// turn ended at a segment boundary (FIG-4739).
    pub(super) segment_boundary: Option<Box<crate::runtime::turn_driver::BoundaryTaken>>,
}

/// Everything the execute phase needs to execute an already-prepared turn.
///
/// The prepare phase (and the host-prepared entry point) builds one of these;
/// the execute phase consumes it whole.
pub(in crate::runtime) struct PreparedTurnExecuteContext<'sinks, 'run> {
    pub(in crate::runtime) turn: PreparedLogicalTurn,
    pub(in crate::runtime) sinks: TurnSinks<'sinks>,
    pub(in crate::runtime) scoped_effect_controller: ActorContext,
    pub(in crate::runtime) local_stop: LocalTurnStop,
    pub(in crate::runtime) initial_admissions: LogicalTurnAdmissions,
    /// The lifetime this value is bound to; the context it carries is `'static`.
    pub(crate) run: std::marker::PhantomData<&'run ()>,
}

/// The preamble step of the execute phase: the plugin prepare-turn hooks and
/// the context transform that produce the message sequence the driver runs.
struct TurnPreambleContext<'preamble, 'run> {
    plugins: &'preamble Arc<crate::PluginSession>,
    scoped_effect_controller: &'preamble ActorContext,
    manager: &'preamble Arc<RuntimeSessionServices>,
    messages: crate::MessageSequence,
    turn_policy: &'preamble crate::SessionPolicy,
    effective_protocol_turn_options: &'preamble crate::ProtocolTurnOptions,
    turn_context: &'preamble crate::TurnContext,
    turn_scope_id: &'preamble str,
    /// The lifetime this value is bound to; the context it carries is `'static`.
    run: std::marker::PhantomData<&'run ()>,
}

/// The effect loop's own inputs: the driver, the observer it publishes
/// through, and the host-local stop its start gate reads.
struct TurnEffectLoopContext<'loop_run, 'run> {
    driver: &'loop_run mut RuntimeTurnDriver<'run>,
    messages: crate::MessageSequence,
    event_tx: TurnObserver,
    protocol_run_offset: usize,
    turn_control: LocalTurnStop,
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
            run: std::marker::PhantomData,
            session,
            recorded_assembly,
            turn_pipeline,
            llm_calls,
            failure_evidence,
            pending_queued,
            pending_turn_inputs,
            withheld_terminal_work,
            turn_cancel,
            segment,
            ..
        } = *self.driver.take().expect("turn driver loan is present");
        *self.session = Some(session);
        TurnDriverRemainder {
            recorded_assembly,
            turn_pipeline,
            llm_calls,
            failure_evidence,
            pending_queued,
            pending_turn_inputs,
            withheld_terminal_work,
            turn_cancel,
            segment_boundary: segment.taken,
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
    } = context;
    // The start gate: a stop already requested ends the turn before its
    // first effect.
    let pending_cancel = turn_control.honoured(&driver.turn_id, true);
    if let Some(evidence) = pending_cancel {
        driver.record_turn_cancel(evidence);
    }
    // Canonical future-size seam: `driver.run` is boxed exactly once here.
    // Driver growth is absorbed by this allocation instead of accreting
    // opportunistic boxes through the callers below.
    Box::pin(driver.run(messages, event_tx, protocol_run_offset)).await
}

impl LashRuntime {
    /// Run the turn's before-turn callbacks as one recorded step: their
    /// decisions and the resolutions of their state commands are served from
    /// the journal on replay, and no callback runs again (K10).
    async fn prepare_turn_preamble(
        &mut self,
        context: TurnPreambleContext<'_, '_>,
    ) -> Result<crate::plugin::TurnPreparation, RuntimeError> {
        let TurnPreambleContext {
            run: std::marker::PhantomData,
            plugins,
            scoped_effect_controller,
            manager,
            messages,
            turn_policy,
            effective_protocol_turn_options,
            turn_context,
            turn_scope_id,
        } = context;
        self.mark_phase_begin(RuntimeTurnPhase::BeforeTurnHooks);
        let recorded = if plugins.has_before_turn_hooks() {
            let hook_context = crate::plugin::TurnHookContext {
                session_id: self.state.session_id.clone(),
                plugin_config: plugins.admitted_plugin_config(),
                state: crate::SessionReadView::from_runtime_state(
                    &self.state,
                    turn_policy.clone(),
                    effective_protocol_turn_options.clone(),
                ),
                sessions: manager.read_service(),
                turn_context: turn_context.clone(),
            };
            let callbacks = Arc::clone(plugins);
            let probe = self.turn_phase_probe.clone();
            let step = format!("plugin-callbacks:before-turn:{turn_scope_id}");
            let recorded = Box::pin(crate::plugin::record_plugin_callbacks(
                scoped_effect_controller,
                crate::RuntimeAttribution::for_session(self.state.session_id.clone()),
                step,
                crate::plugin::RecordedCallbackPhase::BeforeTurn,
                Arc::clone(plugins),
                Box::pin(async move {
                    callbacks
                        .dispatch(probe.as_ref())
                        .before_turn_decisions(hook_context)
                        .await
                }),
            ))
            .await
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
            recorded.map_err(|err| err.into_turn_failure(RuntimeErrorCode::PluginPrepareTurn))?
        } else {
            Vec::new()
        };
        let prepared = crate::PluginSession::apply_before_turn(recorded, messages, turn_scope_id);
        self.mark_phase_end(RuntimeTurnPhase::BeforeTurnHooks);
        Ok(prepared)
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
            run: std::marker::PhantomData,
            turn:
                PreparedLogicalTurn {
                    trace_metadata,
                    messages,
                    mut prelude,
                    previous_prompt_usage,
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
            mut initial_admissions,
        } = context;
        // A later physical turn runs under its Run's turn scope; its waits
        // race this turn's own cancellation gate, the one a cancel of the
        // Run resolves while it runs (an earlier turn sealed its own).
        let scoped_effect_controller =
            scoped_effect_controller.for_physical_turn(trace_turn_id.clone());
        let turn_observer = logical_observer.for_turn(&trace_turn_id);
        let observer = &turn_observer;
        let turn_control = local_stop;
        let turn_policy = self.state.effective_policy().clone();
        // The run's recorded view: its protocol turn options are a view of
        // the protocol namespace of the configuration it was admitted under.
        let effective_protocol_turn_options = self.state.effective_protocol_turn_options();
        let manager = self
            .runtime_session_services_for_turn(&turn_graph_appends)
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
        let follow_on_allowed = initial_admissions.follow_on_allowed();
        // Keep preparation and plugin-abort handling in separate async frames.
        // Their SessionReadView and abort-only locals are dropped before the
        // normal driver-construction frame clones state for the turn boundary.
        let history_len = messages.len();
        let mut prepared = self
            .prepare_turn_preamble(TurnPreambleContext {
                run: std::marker::PhantomData,
                plugins: &plugins,
                scoped_effect_controller: &scoped_effect_controller,
                manager: &manager,
                messages,
                turn_policy: &turn_policy,
                effective_protocol_turn_options: &effective_protocol_turn_options,
                turn_context: &turn_context,
                turn_scope_id: &trace_turn_id,
            })
            .await?;
        turn_graph_appends
            .apply_session_contributions(&self.state.session_id, &plugins, &prepared.session)
            .map_err(|err| err.into_turn_failure(RuntimeErrorCode::PluginPrepareTurn))?;
        // Before-turn contributions are real inputs. Append their recorded
        // suffix to the Prompt View without adopting any transform edits into
        // the history used by checkpoints and commits.
        if prepared.messages.len() > history_len {
            prelude
                .context
                .messages
                .make_mut()
                .extend(prepared.messages.iter().skip(history_len).cloned());
        }
        prelude.history = prepared.messages.clone();
        if plugins.has_before_turn_hooks() {
            prelude.before_turn = Some(
                crate::EffectAddress::new(
                    scoped_effect_controller.execution_scope().clone(),
                    format!("plugin-callbacks:before-turn:{trace_turn_id}"),
                )
                .map_err(RuntimeEffectControllerError::from)
                .map_err(RuntimeEffectControllerError::into_runtime_error)?,
            );
        }
        for event in &prepared.events {
            recorded_assembly.record(event);
        }
        emit_session_events(observer, std::mem::take(&mut prepared.events));
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
        )
        .with_fleet_format(self.fleet_format())
        .with_definition_engines(self.host.core.process_engines.clone())
        .with_metrics(self.host.core.tracing.metrics().clone())
        .with_trace_metadata(trace_metadata)
        .with_trace(self.host.core.tracing.shift(
            scoped_effect_controller.trace_scope().cloned(),
            &scoped_effect_controller,
        ));
        if let Err(error) = turn_pipeline
            .prepared_checkpoint(
                turn_policy.clone(),
                turn_index,
                &prepared.messages,
                self.session.as_mut(),
            )
            .await
        {
            return Err(super::runtime_error_from_store_commit(error));
        }
        // The model binding is the turn's recorded config (D3 §2.1). Nothing
        // binds it here: the body of an unjournaled model call does, so a
        // fully journaled replay never asks this worker's models (FIG-4404).
        let resolved_turn_policy = self
            .host
            .resolve_session_policy(&self.state.session_id, turn_policy.clone())
            .map_err(crate::runtime::turn_config::llm_profile_unconfigured)?;
        let manager = self
            .runtime_session_services_for_turn(&turn_graph_appends)
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::PluginSessionManager, err.to_string())
            })?;
        let finish_scoped_effect_controller = scoped_effect_controller.clone();
        // Work an earlier turn of the logical run withheld for the run's
        // follow-on: this turn's commit carries it on with its own, or hands
        // it to a cancellation (FIG-4044).
        let carried_withheld = initial_admissions.withheld_terminal_work.take();
        // The turn's part in its run's segment boundaries (FIG-4739). A
        // continuation counts on from the iterations its run already spent;
        // a turn outside a shift, or one carrying withheld work to the run's
        // follow-on, takes no boundary.
        let continuation = self
            .state
            .pending_follow_on
            .as_deref()
            .filter(|owed| owed.is_turn(&trace_turn_id))
            .and_then(|owed| owed.owes.continuation());
        let opener_state = continuation
            .map(|owed| {
                crate::session::OpenerState::from_snapshot_for(
                    owed.opener.clone(),
                    &crate::EffectOpener::turn(
                        self.state.session_id.clone(),
                        crate::store::PhysicalTurn::split_turn_id(&trace_turn_id).0,
                    ),
                )
            })
            .transpose()
            .map_err(crate::RuntimeEffectControllerError::into_runtime_error)?
            .unwrap_or_default();
        let segment = crate::runtime::turn_driver::TurnSegment::new(false, continuation);
        let session = self
            .session
            .take()
            .expect("lash runtime session must be available");
        let driver = Box::new(RuntimeTurnDriver {
            run: std::marker::PhantomData,
            tool_run_owner: None,
            session,
            policy: resolved_turn_policy,
            prelude,
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
            turn_context,
            turn_causes: initial_turn_causes,
            pending_queued: initial_admissions.queued,
            pending_turn_inputs: initial_admissions.turn_inputs,
            pending_checkpoint_turn_inputs: None,
            withheld_terminal_work: Default::default(),
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            turn_phase_probe: self.turn_phase_probe.clone(),
            turn_control: turn_control.clone(),
            protocol_reply: Default::default(),
            opener_state,
            turn_cancel: None,
            children_stop: CancellationToken::new(),
            turn_observations: super::turn_observation_cursor(
                &scoped_effect_controller,
                &trace_turn_id,
                "shift",
            ),
            trace: self
                .host
                .core
                .tracing
                .turn_execution(&scoped_effect_controller),
            segment,
        });
        let protocol_run_offset = 0;
        self.mark_phase_begin(RuntimeTurnPhase::EffectLoop);
        let mut driver = TurnDriverSessionLoan::new(&mut self.session, driver);
        let run_result = Box::pin(run_turn_effect_loop(TurnEffectLoopContext {
            driver: &mut driver,
            messages: prepared.messages,
            event_tx: observer.clone(),
            protocol_run_offset,
            turn_control: turn_control.clone(),
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
                let honoured = match driver.turn_cancel.clone() {
                    Some(evidence) => Some(evidence),
                    None if aborted_by_turn_cancel(&err.code) => {
                        turn_control.honoured(&trace_turn_id, true)
                    }
                    None => None,
                };
                if let Some(evidence) = honoured {
                    driver.record_turn_cancel(evidence);
                    let cancellation_messages = driver.turn_pipeline.message_sequence();
                    let opener = driver.take_opener_for_commit()?;
                    let mut driver = driver.reclaim();
                    driver
                        .withheld_terminal_work
                        .carry_earlier(carried_withheld);
                    self.mark_phase_end(RuntimeTurnPhase::EffectLoop);
                    return Box::pin(self.finish_cancelled_turn_after_effect_abort(
                        CancelledTurnFinishContext {
                            driver,
                            opener,
                            cancellation_messages,
                            finish_scoped_effect_controller: &finish_scoped_effect_controller,

                            turn_index,
                            trace_turn_id,
                            observer,
                        },
                    ))
                    .await;
                }
                // The rows the turn drove stay bound to its run: a redrive
                // executes them again, and the run's terminal releases them
                // (FIG-3927 §2.5).
                drop(driver.reclaim());
                self.mark_phase_end(RuntimeTurnPhase::EffectLoop);
                return Err(err);
            }
        };
        let opener = driver.take_opener_for_commit()?;
        let driver = driver.reclaim();
        self.mark_phase_end(RuntimeTurnPhase::EffectLoop);
        tracing::debug!(
            new_message_count = new_messages.len(),
            tool_call_count = driver.recorded_assembly.tool_calls.len(),
            "runtime post-run_task"
        );

        let TurnDriverRemainder {
            recorded_assembly,
            turn_pipeline,
            llm_calls,
            failure_evidence,
            pending_queued,
            pending_turn_inputs,
            mut withheld_terminal_work,
            turn_cancel,
            segment_boundary,
        } = driver;
        withheld_terminal_work.carry_earlier(carried_withheld);
        let mut pending_admissions =
            LogicalTurnAdmissions::new(pending_queued, pending_turn_inputs)
                .with_withheld_terminal_work(withheld_terminal_work.take_if_any())
                .with_follow_on_allowed(follow_on_allowed);
        let finish_result = Box::pin(
            self.finish_turn(TurnCommitContext {
                opener: Some(opener),
                finish: TurnFinishInput {
                    turn_pipeline,
                    recorded_assembly: recorded_assembly
                        .with_llm_calls(llm_calls)
                        .with_failure_evidence(failure_evidence),
                    new_messages,
                    turn_index,
                    trace_turn_id,
                    segment_boundary,
                },
                admissions: &pending_admissions,
                scoped_effect_controller: &finish_scoped_effect_controller,
                honoured_cancel: turn_cancel,

                observer,
            }),
        )
        .await;
        finish_result.map(|mut execution| {
            execution.withheld_terminal_work = pending_admissions.take_follow_on_work(matches!(
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
            | RuntimeErrorCode::RuntimeToolRunAwaitCancelled
            | RuntimeErrorCode::TurnControlWaitCancelled
    )
}
