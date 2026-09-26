use super::*;
use crate::facade_support::RuntimeSessionStateFacadeOps;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;

struct LocalTurnEffectRunner {
    driver: RuntimeTurnDriver<'static>,
    protocol_iteration: usize,
    messages: crate::MessageSequence,
    event_tx: TurnObserver,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for LocalTurnEffectRunner {
    fn uses_task_boundary(&self, command: &RuntimeEffectCommand) -> bool {
        matches!(
            command,
            RuntimeEffectCommand::LlmCall { .. }
                | RuntimeEffectCommand::AssistantResponseHooks { .. }
                | RuntimeEffectCommand::ExecCode { .. }
        )
    }

    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let mut runner = *self;
        match envelope.command {
            RuntimeEffectCommand::LlmCall {
                provider_id: _,
                request,
            } => {
                // The recorded body races the model call against the turn's
                // gate itself: this is the engine's cooperative cancel for a
                // step it cannot select away, and what the body saw is its
                // recorded outcome (ADR 0105 §3, FIG-3672 P9). A watch that
                // gave up is a live fault the engine never records.
                let control = Arc::clone(&runner.driver.turn_control);
                let host = Arc::clone(&runner.driver.host.core.control.effect_host);
                let honoured = runner.driver.turn_cancel.is_some();
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
                } = Box::pin(control.run_step_body(&host, honoured, |stop| async move {
                    driver
                        .run_llm_call(request, protocol_iteration, invocation, &event_tx, &stop)
                        .await
                }))
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
                stream_hook_states,
            } => runner
                .driver
                .run_assistant_response_hooks(*response, &stream_hook_states)
                .await
                .map(
                    |(response, events)| RuntimeEffectOutcome::AssistantResponseHooks {
                        response: Box::new(response),
                        events,
                    },
                ),
            RuntimeEffectCommand::ExecCode { language, code } => {
                let result = runner
                    .driver
                    .run_exec_code(
                        language,
                        &code,
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
            RuntimeEffectCommand::Checkpoint { checkpoint } => Ok(runner
                .driver
                .execute_checkpoint_locally(
                    runner.messages.clone(),
                    runner.protocol_iteration,
                    checkpoint,
                    &runner.event_tx,
                )
                .await),
            RuntimeEffectCommand::SyncExecutionEnvironment => {
                // A live fault rebuilding the environment (a store or lease
                // fault) is not the sync's outcome: the claim is released
                // unsealed and the turn aborts, so a redrive rebuilds it
                // rather than replaying the fault as a failed turn.
                let (result, tool_surface) = match runner
                    .driver
                    .refresh_execution_environment(
                        runner.messages.clone(),
                        runner.protocol_iteration,
                    )
                    .await
                {
                    Ok((sync, tool_surface)) => (Ok(Some(sync)), tool_surface),
                    Err(super::tool_catalog::SyncFailure::Recorded(message)) => {
                        (Err(message), Vec::new())
                    }
                    Err(super::tool_catalog::SyncFailure::Live(error)) => {
                        return Err(RuntimeEffectControllerError::from(error)
                            .retryable_uncommitted_derivation());
                    }
                };
                Ok(RuntimeEffectOutcome::SyncExecutionEnvironment {
                    result,
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
    scoped_effect_controller: ScopedEffectController<'static>,
    body_replay_key: &str,
) -> crate::RuntimeEffectLocalExecutor<'static> {
    let replay_trace = crate::runtime::effect::RuntimeEffectReplayTrace::for_divergence(
        driver.host.core.tracing.trace_sink.as_ref(),
        driver.host.core.tracing.trace_context.clone(),
        driver.trace_context(machine.protocol_iteration()),
        Arc::clone(&driver.host.core.clock),
    );
    let owned_driver = RuntimeTurnDriver {
        session: driver.session.clone_for_effect(),
        policy: driver.policy.clone(),
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
        ),
        latest_prompt_usage: driver.latest_prompt_usage.clone(),
        llm_calls: Vec::new(),
        failure_evidence: Vec::new(),
        session_services: Arc::clone(&driver.session_services),
        protocol_turn_options: driver.protocol_turn_options.clone(),
        protocol_extension: driver.protocol_extension.clone(),
        turn_context: driver.turn_context.clone(),
        turn_causes: driver.turn_causes.clone(),
        pending_queue_claims: driver.pending_queue_claims.clone(),
        pending_turn_input_claims: driver.pending_turn_input_claims.clone(),
        pending_checkpoint_turn_input_claim: driver.pending_checkpoint_turn_input_claim.clone(),
        // Work this executor withholds from a terminal checkpoint travels
        // back on the journalled claim set, not on the driver copy.
        withheld_terminal_work: Default::default(),
        checkpoint_messages: driver.checkpoint_messages.clone(),
        session_execution_lease: driver.session_execution_lease.clone(),
        runtime_lease_owner: driver.runtime_lease_owner.clone(),
        turn_phase_probe: driver.turn_phase_probe.clone(),
        turn_control: Arc::clone(&driver.turn_control),
        protocol_reply: Default::default(),
        live_opener: std::sync::Mutex::new(None),
        opener_state: driver.opener_state.clone(),
        turn_cancel: driver.turn_cancel.clone(),
        children_stop: driver.children_stop.clone(),
        // The body's own lane, keyed by the effect invocation it runs: the
        // turn cursor is the main driver's alone, and cloning it would put
        // body and driver emissions on colliding {key}#{ordinal} ids.
        turn_observations: body_observation_cursor(body_replay_key),
    };
    crate::RuntimeEffectLocalExecutor::owned_runner(
        Box::new(LocalTurnEffectRunner {
            driver: owned_driver,
            protocol_iteration: machine.protocol_iteration(),
            messages: machine.message_sequence(),
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
        fn observe(&self, observation: crate::engine::DriveObservation) {
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
        let backend = crate::testing::memory_backend().await;
        let scoped = backend
            .effect_host()
            .scoped_static(crate::AdmittedScope::turn(
                crate::SessionId::from("session"),
                crate::TurnId::from("turn"),
            ))
            .expect("admit the turn scope")
            .expect("the backend host lends a static controller");
        let turn_id = crate::TurnId::from("turn");
        let session_id = crate::SessionId::from("session");

        let mut driver_cursor =
            crate::runtime::turn_loop::turn_observation_cursor(&scoped, &turn_id, "drive");
        let body_invocation = crate::runtime::causal::turn_effect_invocation(
            scoped.execution_scope(),
            &session_id,
            &turn_id,
            0,
            0,
            crate::sansio::EffectId(0),
            RuntimeEffectKind::Checkpoint,
        );
        let mut body_cursor = body_observation_cursor(body_invocation.replay_key());

        let sink = ObservationIds::default();
        let event = || {
            crate::engine::ObservedEvent::Session(crate::SessionStreamEvent::Message {
                text: "marker".to_string(),
                kind: "test".to_string(),
            })
        };
        body_cursor.observe(&sink, event());
        driver_cursor.observe(&sink, event());

        let ids = sink.0.lock().expect("observation ids").clone();
        assert_eq!(ids.len(), 2);
        assert_eq!(
            ids[0],
            format!("{}:body#0", body_invocation.replay_key()),
            "the body's lane is keyed by its own effect invocation"
        );
        assert_ne!(
            ids[0], ids[1],
            "a body emission and a driver emission at ordinal 0 would collide if the body cloned the turn cursor"
        );
    }
}
