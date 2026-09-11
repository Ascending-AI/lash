use super::*;
use std::collections::HashSet;

/// Names the assistant messages the protocol driver appends after its final
/// model call: the turn's reply as the protocol materialized it.
///
/// The snapshot of assistant ids is taken when a run starts and again at every
/// model call; whatever assistant messages exist beyond it when the machine
/// finishes were appended by the finishing step. Identity is by message id,
/// so a message appended later by a finalize-turn hook never displaces it.
#[derive(Debug, Default)]
pub(in crate::runtime) struct ProtocolReplyTracker {
    assistant_ids_at_last_model_call: Option<HashSet<String>>,
}

impl ProtocolReplyTracker {
    fn assistant_ids<'a>(messages: impl Iterator<Item = &'a Message>) -> HashSet<String> {
        messages
            .filter(|message| message.role == MessageRole::Assistant)
            .map(|message| message.id.clone())
            .collect()
    }

    /// Anchors the first run on the messages the turn started with; later runs
    /// keep the anchor from their predecessor's last model call.
    pub(super) fn mark_run_start<'a>(&mut self, messages: impl Iterator<Item = &'a Message>) {
        if self.assistant_ids_at_last_model_call.is_none() {
            self.assistant_ids_at_last_model_call = Some(Self::assistant_ids(messages));
        }
    }

    pub(super) fn mark_model_call<'a>(&mut self, messages: impl Iterator<Item = &'a Message>) {
        self.assistant_ids_at_last_model_call = Some(Self::assistant_ids(messages));
    }

    /// Ids of the assistant messages appended since the last model call.
    pub(super) fn terminal_output<'a>(
        &self,
        messages: impl Iterator<Item = &'a Message>,
    ) -> Vec<String> {
        let before = self.assistant_ids_at_last_model_call.as_ref();
        messages
            .filter(|message| message.role == MessageRole::Assistant)
            .filter(|message| !before.is_some_and(|before| before.contains(&message.id)))
            .map(|message| message.id.clone())
            .collect()
    }
}

impl RuntimeTurnDriver<'_> {
    pub(in crate::runtime) async fn run(
        &mut self,
        messages: crate::MessageSequence,
        event_tx: mpsc::Sender<RuntimeStreamEvent>,
        cancel: CancellationToken,
        run_offset: usize,
    ) -> Result<(crate::MessageSequence, usize, bool), RuntimeError> {
        self.protocol_reply.mark_run_start(messages.iter());
        let machine = match self
            .prepare_turn_machine(messages, &event_tx, run_offset)
            .await
        {
            Ok(prepared) => prepared,
            Err((messages, iteration)) => return Ok((messages, iteration, false)),
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
    ) -> Result<(crate::MessageSequence, usize, bool), RuntimeError> {
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
                    self.turn_pipeline.record_protocol_terminal_output(
                        self.protocol_reply.terminal_output(messages.iter()),
                    );
                    return Ok((
                        messages,
                        protocol_iteration,
                        machine.turn_limit_final_scheduled(),
                    ));
                }
                Effect::LlmCall { id, request } => {
                    self.protocol_reply
                        .mark_model_call(machine.messages().iter());
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
                Effect::ReportToolCalls { completed } => {
                    self.report_undispatched_turn_tool_calls(
                        completed,
                        machine.protocol_iteration(),
                        &event_tx,
                    )
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

        Ok((crate::MessageSequence::default(), run_offset, false))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, role: MessageRole) -> Message {
        Message {
            id: id.to_string(),
            role,
            parts: crate::shared_parts(vec![crate::Part::prose(
                format!("{id}.p0"),
                id.to_string(),
                None,
            )]),
            origin: None,
        }
    }

    #[test]
    fn terminal_output_is_the_assistant_tail_after_the_last_model_call() {
        let history = vec![
            message("a0", MessageRole::Assistant),
            message("u1", MessageRole::User),
        ];
        let mut tracker = ProtocolReplyTracker::default();
        tracker.mark_run_start(history.iter());

        let mut turn = history.clone();
        turn.push(message("retry", MessageRole::Assistant));
        turn.push(message("reminder", MessageRole::System));
        tracker.mark_model_call(turn.iter());
        turn.push(message("reply", MessageRole::Assistant));
        turn.push(message("enqueued", MessageRole::User));

        assert_eq!(
            tracker.terminal_output(turn.iter()),
            vec!["reply".to_string()]
        );
    }

    #[test]
    fn a_finish_without_a_new_assistant_message_names_nothing() {
        let mut turn = vec![message("u0", MessageRole::User)];
        let mut tracker = ProtocolReplyTracker::default();
        tracker.mark_run_start(turn.iter());
        turn.push(message("prose", MessageRole::Assistant));
        tracker.mark_model_call(turn.iter());

        assert!(tracker.terminal_output(turn.iter()).is_empty());
    }

    #[test]
    fn a_later_run_keeps_the_anchor_from_the_last_model_call() {
        let mut turn = vec![message("u0", MessageRole::User)];
        let mut tracker = ProtocolReplyTracker::default();
        tracker.mark_run_start(turn.iter());
        tracker.mark_model_call(turn.iter());
        turn.push(message("reply", MessageRole::Assistant));
        tracker.mark_run_start(turn.iter());

        assert_eq!(
            tracker.terminal_output(turn.iter()),
            vec!["reply".to_string()]
        );
    }
}
