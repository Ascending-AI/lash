use super::*;

impl RuntimeTurnDriver<'_> {
    pub(in crate::runtime) async fn run(
        &mut self,
        messages: crate::MessageSequence,
        event_tx: mpsc::Sender<RuntimeStreamEvent>,
        cancel: CancellationToken,
        run_offset: usize,
    ) -> Result<(crate::MessageSequence, usize), RuntimeError> {
        let machine = match self
            .prepare_turn_machine(messages, &event_tx, run_offset)
            .await
        {
            Ok(prepared) => prepared,
            Err(result) => return Ok(result),
        };
        self.run_machine(machine, event_tx, cancel, run_offset)
            .await
    }

    async fn run_machine(
        &mut self,
        mut machine: TurnMachine,
        event_tx: mpsc::Sender<RuntimeStreamEvent>,
        cancel: CancellationToken,
        run_offset: usize,
    ) -> Result<(crate::MessageSequence, usize), RuntimeError> {
        macro_rules! emit {
            ($event:expr) => {
                send_session_event(&event_tx, $event).await
            };
        }
        loop {
            let Some(effect) = machine.poll_effect() else {
                break;
            };
            match effect {
                Effect::Emit(event) => {
                    if let SessionStreamEvent::TokenUsage {
                        usage, cumulative, ..
                    } = &event
                    {
                        self.turn_pipeline.state_mut().token_usage = cumulative.clone();
                        self.turn_pipeline.state_mut().last_prompt_usage =
                            normalize_prompt_usage(usage);
                    }
                    emit!(event)
                }
                Effect::Progress {
                    messages,
                    event_delta,
                    protocol_iteration,
                } => {
                    self.apply_progress_boundary(messages, event_delta, protocol_iteration)
                        .await?
                }
                Effect::Done {
                    messages,
                    event_delta,
                    protocol_iteration,
                } => {
                    self.turn_pipeline.apply_event_delta(event_delta);
                    return Ok((messages, protocol_iteration));
                }
                Effect::LlmCall { id, request } => {
                    self.handle_llm_call_effect(&mut machine, id, request, &event_tx, &cancel)
                        .await?;
                }
                Effect::Checkpoint { id, checkpoint } => {
                    self.handle_checkpoint_effect(&mut machine, id, checkpoint, &event_tx, &cancel)
                        .await?;
                }
                Effect::SyncExecutionEnvironment {
                    id,
                    update_machine_config,
                } => {
                    self.handle_execution_environment_sync_effect(
                        &mut machine,
                        id,
                        update_machine_config,
                        &event_tx,
                        &cancel,
                    )
                    .await?;
                }
                Effect::ToolCalls { id, calls } => {
                    self.handle_tool_calls_effect(&mut machine, id, calls, &event_tx, &cancel)
                        .await?;
                }
                Effect::Log { event } => self.handle_log_event(event),
                Effect::ExecCode { id, language, code } => {
                    self.handle_exec_code_effect(
                        &mut machine,
                        id,
                        language,
                        code,
                        &event_tx,
                        &cancel,
                    )
                    .await?;
                }
            }
        }

        Ok((crate::MessageSequence::default(), run_offset))
    }

    async fn apply_progress_boundary(
        &mut self,
        messages: crate::MessageSequence,
        event_delta: Vec<crate::SessionHistoryRecord>,
        protocol_iteration: usize,
    ) -> Result<(), RuntimeError> {
        if !crate::messages_are_prompt_resume_safe(messages.iter()) {
            return Ok(());
        }
        let boundary = self
            .turn_pipeline
            .progress_boundary(
                &mut self.session,
                self.policy.policy.clone(),
                self.turn_index,
                messages,
                event_delta,
            )
            .await?;
        for event in &boundary.protocol_events {
            self.emit_trace(protocol_iteration, protocol_step_trace_event(event));
        }
        Ok(())
    }
}
