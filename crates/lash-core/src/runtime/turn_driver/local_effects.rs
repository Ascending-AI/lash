use super::*;
use crate::ActorContext;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;

/// The owned capabilities of one effect body. The turn driver alone owns
/// admission, cell consumption, commit publication and after-turn state.
pub(super) struct TurnEffectContext {
    pub(super) session: Session,
    pub(super) policy: RuntimeSessionPolicy,
    pub(super) host: RuntimeHost,
    pub(super) scoped_effect_controller: ActorContext,
    pub(super) session_id: SessionId,
    pub(super) turn_id: lash_vocabulary::identity::TurnId,
    pub(super) turn_index: usize,
    read_view: EffectReadView,
    pub(super) execution_frame: Option<crate::FrameNodeId>,
    pub(super) session_services: Arc<RuntimeSessionServices>,
    pub(super) turn_context: crate::TurnContext,
    pub(super) pending_turn_inputs: Vec<crate::AdmittedTurnInputs>,
    pub(super) pending_checkpoint_turn_inputs: Option<crate::AdmittedTurnInputs>,
    pub(super) turn_phase_probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
    pub(super) opener_state: crate::session::OpenerState,
    pub(super) children_stop: CancellationToken,
    pub(super) cell_end: crate::CellEndHandoff,
    pub(super) session_pages: Option<crate::plugin::SessionPages>,
    pub(super) held_response: Option<crate::HeldResponse>,
    pub(super) model_stop_error: Arc<std::sync::Mutex<Option<crate::LlmCallError>>>,
    pub(super) model_limit: Option<crate::ExecutionLimit>,
    pub(super) call_drains: streaming::CallDrains,
    pub(super) turn_observations: crate::engine::ObservationCursor,
    pub(super) trace: crate::trace::TraceStanding,
    pub(super) admitted_body: Option<lash_protocol_types::llm::types::AdmittedSend>,
    pub(super) model_attempt: u32,
    pub(super) model_call_address: Option<crate::plugin::ModelCallAddress>,
    pub(super) model_call_ordinal: u32,
    pub(super) run_config: crate::runtime::durable::run_config::RecordedRunConfig,
}

/// Captured read inputs; the callback materializes its message view when it runs.
struct EffectReadView {
    state: crate::RuntimeSessionState,
    graph: Arc<crate::SessionGraph>,
    protocol_turn_options: crate::ProtocolTurnOptions,
}

impl TurnEffectContext {
    pub(super) fn checkpoint_state_view(
        &self,
        messages: crate::MessageSequence,
    ) -> crate::SessionReadView {
        crate::SessionReadView::derived_from_persisted_state(
            &self.read_view.state,
            self.policy.policy.clone(),
            self.turn_index,
            self.read_view.protocol_turn_options.clone(),
            Arc::clone(&self.read_view.graph),
            messages,
        )
    }
}

struct LocalTurnEffectRunner {
    context: TurnEffectContext,
    protocol_iteration: usize,
    messages: crate::MessageSequence,
    /// The machine's admitted last-call usage at this effect's boundary.
    last_call_usage: Option<crate::LlmUsage>,
    active_events: lash_protocol_types::AppendVec<crate::SessionHistoryRecord>,
    /// The machine waits on a checkpoint that decides a control call's
    /// candidate, which input addressed to the turn supersedes.
    decides_candidate: bool,
    event_tx: TurnObserver,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for LocalTurnEffectRunner {
    fn execution_state(&self) -> Option<Arc<dyn crate::plugin::ExecutionState>> {
        Some(self.context.session.plugins().clone())
    }

    fn uses_task_boundary(&self, command: &RuntimeEffectCommand) -> bool {
        matches!(
            command,
            RuntimeEffectCommand::BeforeLlmCall { .. }
                | RuntimeEffectCommand::LlmCall { .. }
                | RuntimeEffectCommand::AssistantResponseHooks { .. }
                | RuntimeEffectCommand::ExecCode { .. }
        )
    }

    /// A recorded step's body observes as that body's live step. A body that
    /// replays by re-execution is bound none, and keeps the turn's standing.
    fn bind_live_step(&mut self, live: Arc<crate::trace::LiveStep>) {
        self.context.trace = self.context.trace.in_body(&live);
    }

    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let mut runner = *self;
        match envelope.command {
            RuntimeEffectCommand::BeforeLlmCall { request } => {
                let decision = if runner
                    .context
                    .session
                    .plugins()
                    .protocol_session()
                    .has_before_llm_call()
                {
                    let flat = request.materialize();
                    runner
                        .context
                        .run_before_llm_call(runner.messages, &flat, runner.last_call_usage)
                        .await
                } else {
                    Ok(None)
                };
                if let Err(error) = &decision {
                    let failure = error
                        .clone()
                        .into_turn_failure(crate::RuntimeErrorCode::ProtocolBeforeLlmCall);
                    if matches!(
                        failure.turn_failure_cause(),
                        crate::TurnFailureCause::LiveFault
                    ) && !failure.is_session_retirement()
                    {
                        return Err(RuntimeEffectControllerError::from(failure)
                            .retryable_uncommitted_derivation());
                    }
                }
                Ok(RuntimeEffectOutcome::BeforeLlmCall { decision })
            }
            RuntimeEffectCommand::LlmCall { request } => {
                // This body runs only for an unjournaled call, so this is
                // where the recorded model is bound (FIG-4404). A refusal is
                // this deployment's fault: it leaves the step unsealed and
                // the engine runs it again, and it is never the call's
                // recorded result.
                let provider = runner
                    .context
                    .policy
                    .binding()
                    .bind_for_unjournaled_call()?;
                // The admitted call's exact body: the body sends it as it is.
                let body = runner.context.admitted_body.take().ok_or_else(|| {
                    RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                        "a model call's effect runs only for an admitted call with its body",
                    )
                })?;
                // A cancellation the turn already honoured stops the model
                // call.
                let stop = runner.context.children_stop.child_token();
                let request = Arc::new((*request).into_retained());
                let invocation = envelope.invocation.into_runtime_invocation();
                let protocol_iteration = runner.protocol_iteration;
                let event_tx = runner.event_tx.clone();
                let context = &mut runner.context;
                let crate::runtime::RuntimeLlmCallOutcome {
                    result,
                    text_streamed,
                    call_record,
                    stream,
                } = Box::pin(context.run_llm_call(
                    request,
                    &body,
                    protocol_iteration,
                    invocation,
                    &event_tx,
                    &stop,
                    super::streaming::LlmCallDispatch { provider },
                ))
                .await?;
                Ok(RuntimeEffectOutcome::LlmCall {
                    result: Box::new(result),
                    text_streamed,
                    call_record,
                    stream: Box::new(stream),
                })
            }
            RuntimeEffectCommand::AssistantResponseHooks {
                response,
                plan,
                capsule,
            } => runner
                .context
                .run_assistant_response_hooks(*response, &plan, &capsule)
                .await
                .map(
                    |(response, events)| RuntimeEffectOutcome::AssistantResponseHooks {
                        response: Box::new(response),
                        events,
                    },
                ),
            RuntimeEffectCommand::ExecCode { code } => {
                let result = runner
                    .context
                    .run_exec_code(
                        &code,
                        runner.active_events.clone(),
                        runner.messages.clone(),
                        runner.protocol_iteration,
                        envelope.invocation.into_runtime_invocation(),
                        &runner.event_tx,
                    )
                    .await?;
                Ok(RuntimeEffectOutcome::ExecCode {
                    result: Box::new(result),
                })
            }
            RuntimeEffectCommand::Checkpoint { checkpoint } => {
                runner
                    .context
                    .execute_checkpoint_locally(
                        runner.messages,
                        runner.protocol_iteration,
                        checkpoint,
                        runner.decides_candidate,
                        &runner.event_tx,
                    )
                    .await
            }
            command => Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "local turn executor cannot execute {} command",
                    command.kind().as_str()
                ),
            )),
        }
    }
}

/// The observation cursor an effect body emits through: keyed by the body's
/// own invocation, under the `:body` lane, so its ids can never collide with
/// the main driver's turn cursor (ADR 0105 §1).
fn body_observation_cursor(body_replay_key: &str) -> crate::engine::ObservationCursor {
    crate::engine::ObservationCursor::new(crate::engine::ReplayKey::new(format!(
        "{body_replay_key}:body"
    )))
}

pub(super) fn turn_effect_executor(
    driver: &mut RuntimeTurnDriver<'_>,
    machine: &crate::TurnMachine,
    event_tx: TurnObserver,
    scoped_effect_controller: ActorContext,
    body_replay_key: &str,
) -> crate::RuntimeEffectLocalExecutor<'static> {
    let replay_trace = crate::runtime::effect::RuntimeEffectReplayTrace::for_divergence(
        &driver.host.core.tracing,
        driver.trace.scope().cloned(),
        driver.trace_context(machine.protocol_iteration()),
    );
    // Preserve the body's read cut without giving it a turn commit owner.
    // Materializing an owned message sequence belongs to the callback body,
    // not to constructing an executor for an effect that may never read it.
    let mut read_state = driver.turn_pipeline.state().clone();
    read_state.ensure_agent_frame_initialized_with_clock(driver.host.core.clock.as_ref());
    let execution_frame = read_state.current_frame_node_id.clone();
    let read_graph = Arc::new(std::mem::take(&mut read_state.session_graph));
    let context = TurnEffectContext {
        session: driver.session.clone_for_effect(),
        policy: driver.policy.clone(),
        host: driver.host.clone(),
        scoped_effect_controller,
        session_id: driver.session_id.clone(),
        turn_id: driver.turn_id.clone(),
        turn_index: driver.turn_index,
        read_view: EffectReadView {
            state: read_state,
            graph: read_graph,
            protocol_turn_options: driver.protocol_turn_options.clone(),
        },
        execution_frame,
        session_services: Arc::clone(&driver.session_services),
        turn_context: driver.turn_context.clone(),
        pending_turn_inputs: driver.pending_turn_inputs.clone(),
        pending_checkpoint_turn_inputs: driver.pending_checkpoint_turn_inputs.clone(),
        turn_phase_probe: driver.turn_phase_probe.clone(),
        opener_state: driver.opener_state.clone(),
        children_stop: driver.children_stop.clone(),
        cell_end: driver.cell_end.clone(),
        session_pages: driver.session_pages.clone(),
        held_response: driver.held_response.clone(),
        model_stop_error: Arc::clone(&driver.model_stop_error),
        model_limit: driver.model_limit,
        call_drains: driver.call_drains.clone(),
        // Body and driver emissions have distinct observation identities.
        turn_observations: body_observation_cursor(body_replay_key),
        trace: driver.trace.clone(),
        admitted_body: driver.admitted_body.take(),
        model_attempt: driver.model_attempt,
        model_call_address: driver.model_call_address.clone(),
        model_call_ordinal: driver.model_call_ordinal,
        run_config: driver.run_config.clone(),
    };
    lash_core_execution::core_internal::owned_runner_executor(
        Box::new(LocalTurnEffectRunner {
            context,
            protocol_iteration: machine.protocol_iteration(),
            messages: machine.message_sequence(),
            last_call_usage: machine.last_call_usage().cloned().and_then(nonzero_usage),
            active_events: driver.turn_pipeline.active_events(),
            decides_candidate: machine.awaits_completion_decision(),
            event_tx,
        }),
        replay_trace,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records the `(key, ordinal)` identity of every emitted observation.
    #[derive(Default)]
    struct ObservationIds(std::sync::Mutex<Vec<String>>);

    impl crate::engine::ObservationSink for ObservationIds {
        fn observe(&self, observation: crate::engine::ShiftObservation) {
            self.0
                .lock()
                .expect("observation ids")
                .push(format!("{}#{}", observation.key, observation.ordinal));
        }
    }

    /// A checkpoint body emits on its own `{invocation replay key}:body` lane
    /// while the main driver keeps the turn cursor: a body emission followed
    /// by a driver emission must mint distinct ids even at the same ordinal
    /// (ADR 0105 §1).
    #[tokio::test]
    async fn a_checkpoint_body_and_the_driver_mint_distinct_observation_ids() {
        let backend = crate::testing::sqlite_recording_backend().await;
        let scoped = crate::ActorContext::detached(backend.clone())
            .scoped(crate::AdmittedScope::turn(
                lash_kernel_value::identity::SessionId::from("session"),
                lash_vocabulary::identity::TurnId::from("turn"),
            ))
            .expect("admit the turn scope");
        let turn_id = lash_vocabulary::identity::TurnId::from("turn");
        let session_id = lash_kernel_value::identity::SessionId::from("session");

        let mut driver_cursor =
            crate::runtime::turn_loop::turn_observation_cursor(&scoped, &turn_id, "shift");
        let body_invocation = crate::runtime::causal::turn_effect_invocation(
            scoped.execution_scope(),
            &session_id,
            &turn_id,
            0,
            0,
            crate::sansio::EffectId(0),
            RuntimeEffectKind::Checkpoint,
        );
        let mut body_cursor = body_observation_cursor(body_invocation.effect_replay_key());

        let sink = ObservationIds::default();
        let event = || {
            crate::engine::ObservedEvent::Session(crate::SessionStreamEvent::Message {
                text: "marker".to_string(),
                kind: crate::StreamMessageKind::Code,
            })
        };
        body_cursor.observe(&sink, event());
        driver_cursor.observe(&sink, event());

        let ids = sink.0.lock().expect("observation ids").clone();
        assert_eq!(ids.len(), 2);
        assert_eq!(
            ids[0],
            format!("{}:body#0", body_invocation.effect_replay_key()),
            "the body's lane is keyed by its own effect invocation"
        );
        assert_ne!(
            ids[0], ids[1],
            "a body emission and a driver emission at ordinal 0 would collide if the body cloned the turn cursor"
        );
    }
}
