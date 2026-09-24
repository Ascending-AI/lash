use super::*;
use crate::facade_support::RuntimeSessionStateFacadeOps;
use crate::runtime::effect::executor::{
    RuntimeEffectLocalRunner, sleep_duration, sleep_with_cancellation,
};

struct LocalTurnEffectRunner {
    driver: RuntimeTurnDriver<'static>,
    protocol_iteration: usize,
    /// The cell replay-key grammar the iteration's journaled sync named
    /// (FIG-3586), which a code cell must run under.
    cell_replay_grammar: Option<u32>,
    messages: crate::MessageSequence,
    event_tx: TurnObserver,
    cancellation: CancellationToken,
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
                let crate::runtime::RuntimeLlmCallOutcome {
                    result,
                    text_streamed,
                    call_record,
                    stream,
                } = runner
                    .driver
                    .run_llm_call(
                        Arc::new((*request).into_request(None, None)),
                        runner.protocol_iteration,
                        envelope.invocation.into_runtime_invocation(),
                        &runner.event_tx,
                        &runner.cancellation,
                    )
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
                        runner.cell_replay_grammar,
                        envelope.invocation.into_runtime_invocation(),
                        &runner.event_tx,
                        &runner.cancellation,
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
                let result = match runner
                    .driver
                    .refresh_execution_environment(runner.messages.clone())
                    .await
                {
                    Ok(sync) => Ok(sync),
                    Err(super::tool_catalog::SyncFailure::Recorded(message)) => Err(message),
                    Err(super::tool_catalog::SyncFailure::Live(error)) => {
                        return Err(RuntimeEffectControllerError::from(error)
                            .retryable_uncommitted_derivation());
                    }
                };
                // Every sync names the code executor's replay-key grammar,
                // the protocol-start one included: it is what the iteration's
                // cells run under on replay (FIG-3586).
                let cell_replay_grammar = result.is_ok().then(|| {
                    runner
                        .driver
                        .session
                        .plugins()
                        .code_executor()
                        .and_then(|executor| executor.replay_key_grammar())
                });
                Ok(RuntimeEffectOutcome::SyncExecutionEnvironment {
                    result,
                    cell_replay_grammar: cell_replay_grammar.flatten(),
                })
            }
            RuntimeEffectCommand::Sleep { spec } => {
                let clock = runner.driver.host.core.clock.as_ref();
                let duration_ms = sleep_duration(spec, clock.timestamp_ms());
                sleep_with_cancellation(duration_ms, &runner.cancellation, clock).await?;
                Ok(RuntimeEffectOutcome::Sleep)
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

pub(super) fn turn_effect_executor(
    driver: &mut RuntimeTurnDriver<'_>,
    machine: &crate::TurnMachine,
    event_tx: TurnObserver,
    cancellation: CancellationToken,
    scoped_effect_controller: ScopedEffectController<'static>,
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
        observes_durable_cancel_after_llm: driver.observes_durable_cancel_after_llm,
        protocol_reply: Default::default(),
        live_opener: std::sync::Mutex::new(None),
        opener_state: driver.opener_state.clone(),
        cooperative_cancel: CancellationToken::new(),
    };
    crate::RuntimeEffectLocalExecutor::owned_runner(
        Box::new(LocalTurnEffectRunner {
            driver: owned_driver,
            protocol_iteration: machine.protocol_iteration(),
            cell_replay_grammar: machine.synced_cell_replay_grammar(),
            messages: machine.message_sequence(),
            event_tx,
            cancellation,
        }),
        replay_trace,
    )
}
