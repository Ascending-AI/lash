use super::*;
use crate::ActorContext;
use crate::facade_support::RuntimeSessionStateFacadeOps;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;

struct LocalTurnEffectRunner {
    driver: RuntimeTurnDriver<'static>,
    protocol_iteration: usize,
    messages: crate::MessageSequence,
    prompt_messages: crate::MessageSequence,
    active_events: lash_sansio::AppendVec<crate::SessionHistoryRecord>,
    event_tx: TurnObserver,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for LocalTurnEffectRunner {
    fn plugin_state_session(&self) -> Option<Arc<crate::PluginSession>> {
        Some(Arc::clone(self.driver.session.plugins()))
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
        self.driver.trace = self.driver.trace.in_body(&live);
    }

    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let mut runner = *self;
        match envelope.command {
            RuntimeEffectCommand::BeforeLlmCall { request } => {
                let decision = runner
                    .driver
                    .run_before_llm_call(runner.messages, runner.protocol_iteration, &request)
                    .await;
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
                let provider = runner.driver.policy.binding().bind_for_unjournaled_call()?;
                // The admitted call's exact body: the body sends it as it is.
                let body = runner.driver.admitted_body.take().ok_or_else(|| {
                    RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                        "a model call's effect runs only for an admitted call with its body",
                    )
                })?;
                // A cancellation the turn already honoured stops the model
                // call.
                let stop = runner.driver.children_stop.child_token();
                let request = Arc::new((*request).into_request(None, None));
                let invocation = envelope.invocation.into_runtime_invocation();
                let protocol_iteration = runner.protocol_iteration;
                let event_tx = runner.event_tx.clone();
                let driver = &mut runner.driver;
                let crate::runtime::RuntimeLlmCallOutcome {
                    result,
                    text_streamed,
                    call_record,
                    stream,
                } = Box::pin(driver.run_llm_call(
                    request,
                    &body,
                    protocol_iteration,
                    invocation,
                    &event_tx,
                    &stop,
                    super::streaming::LlmCallDispatch { provider },
                ))
                .await;
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
                stream_hook_states,
            } => runner
                .driver
                .run_assistant_response_hooks(*response, &plan, &stream_hook_states)
                .await
                .map(
                    |(response, events)| RuntimeEffectOutcome::AssistantResponseHooks {
                        response: Box::new(response),
                        events,
                    },
                ),
            RuntimeEffectCommand::ExecCode { code } => {
                let result = runner
                    .driver
                    .run_exec_code(
                        &code,
                        Arc::new(
                            crate::facade_support::ChronologicalProjection::from_turn_view(
                                runner.active_events.as_slice(),
                                &runner.messages,
                            ),
                        ),
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
                    .driver
                    .execute_checkpoint_locally(
                        runner.messages.clone(),
                        runner.protocol_iteration,
                        checkpoint,
                        &runner.event_tx,
                    )
                    .await
            }
            RuntimeEffectCommand::SyncExecutionEnvironment => {
                // A live fault rebuilding the environment (a store fault) is
                // not the sync's outcome: the step stays unrecorded and the
                // turn aborts, so a redrive rebuilds it rather than replaying
                // the fault as a failed turn.
                let (result, tool_surface) = match runner.driver.refresh_execution_environment() {
                    Ok((sync, tool_surface)) => (Ok(sync), tool_surface),
                    Err(super::tool_catalog::SyncFailure::Recorded(failure)) => {
                        (Err(failure), Vec::new())
                    }
                    Err(super::tool_catalog::SyncFailure::Live(error)) => {
                        return Err(RuntimeEffectControllerError::from(error)
                            .retryable_uncommitted_derivation());
                    }
                };
                let mut prelude = runner.driver.prelude.clone();
                prelude.history = runner.messages.clone();
                prelude.context.messages = runner.prompt_messages.clone();
                // The prelude is stored before this outcome completes, and
                // the outcome journals only its digest (FIG-5133).
                let prelude = prelude
                    .record(
                        runner
                            .driver
                            .host
                            .core
                            .durability
                            .turn_prelude_store
                            .as_ref(),
                        envelope.invocation.execution_scope(),
                    )
                    .await?;
                Ok(RuntimeEffectOutcome::SyncExecutionEnvironment {
                    prelude,
                    result: Box::new(result),
                    tool_surface,
                })
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
    let owned_driver = RuntimeTurnDriver {
        run: std::marker::PhantomData,
        // An effect body takes no boundary of its own, but a cell it runs
        // asks whether its turn may end at one inside it (FIG-4739).
        session: driver.session.clone_for_effect(),
        policy: driver.policy.clone(),
        prelude: driver.prelude.clone(),
        // A step body commits nothing: whatever it would record is dropped
        // with this copy, and the turn's content rides its recorded outcome.
        recorded_assembly: RecordedTurnAssembly::new(),
        host: driver.host.clone(),
        scoped_effect_controller,
        session_id: driver.session_id.clone(),
        turn_id: driver.turn_id.clone(),
        turn_index: driver.turn_index,
        turn_pipeline: crate::runtime::TurnBoundary::from_state_with_clock(
            driver.turn_pipeline.state().clone(),
            Arc::clone(&driver.host.core.clock),
            driver.turn_pipeline.state().turn_scope(&driver.turn_id),
            driver.host.core.durability.commit_budget,
        )
        .with_definition_engines(driver.host.core.process_engines.clone())
        .with_metrics(driver.host.core.tracing.metrics().clone())
        .with_trace(driver.trace.clone()),
        latest_prompt_usage: driver.latest_prompt_usage.clone(),
        llm_calls: Vec::new(),
        failure_evidence: Vec::new(),
        session_services: Arc::clone(&driver.session_services),
        // A step body runs no after-turn callback.
        after_turn_reads: None,
        protocol_turn_options: driver.protocol_turn_options.clone(),
        turn_context: driver.turn_context.clone(),
        turn_causes: driver.turn_causes.clone(),
        pending_queued: driver.pending_queued.clone(),
        pending_turn_inputs: driver.pending_turn_inputs.clone(),
        pending_checkpoint_turn_inputs: driver.pending_checkpoint_turn_inputs.clone(),
        turn_phase_probe: driver.turn_phase_probe.clone(),
        protocol_reply: Default::default(),
        opener_state: driver.opener_state.clone(),
        children_stop: driver.children_stop.clone(),
        // The body's own lane, keyed by the effect invocation it runs: the
        // turn cursor is the main driver's alone, and cloning it would put
        // body and driver emissions on colliding {key}#{ordinal} ids.
        turn_observations: body_observation_cursor(body_replay_key),
        trace: driver.trace.clone(),
        admitted_body: driver.admitted_body.take(),
    };
    lash_core_execution::core_internal::owned_runner_executor(
        Box::new(LocalTurnEffectRunner {
            driver: owned_driver,
            protocol_iteration: machine.protocol_iteration(),
            messages: machine.message_sequence(),
            prompt_messages: machine.prompt_message_sequence(),
            active_events: driver.turn_pipeline.active_events(),
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
                crate::SessionId::from("session"),
                crate::TurnId::from("turn"),
            ))
            .expect("admit the turn scope");
        let turn_id = crate::TurnId::from("turn");
        let session_id = crate::SessionId::from("session");

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
