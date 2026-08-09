impl<M: TurnProtocol> TurnMachine<M> {
    /// Create a new machine in `PrepareIteration` state.
    pub fn new(
        config: TurnMachineConfig<M>,
        messages: Vec<Message>,
        events: Arc<Vec<SessionHistoryRecord<M::Event>>>,
        protocol_run_offset: usize,
    ) -> Self {
        Self::new_shared(
            config,
            MessageSequence::from_owned(messages),
            events,
            protocol_run_offset,
        )
    }

    pub fn new_shared(
        config: TurnMachineConfig<M>,
        messages: MessageSequence,
        events: Arc<Vec<SessionHistoryRecord<M::Event>>>,
        protocol_run_offset: usize,
    ) -> Self {
        Self::new_shared_with_turn_causes(config, messages, events, protocol_run_offset, Vec::new())
    }

    pub fn new_shared_with_turn_causes(
        config: TurnMachineConfig<M>,
        messages: MessageSequence,
        events: Arc<Vec<SessionHistoryRecord<M::Event>>>,
        protocol_run_offset: usize,
        turn_causes: Vec<TurnCause>,
    ) -> Self {
        let next_synthetic_message_id = messages.len() as u64;
        Self {
            config,
            state: MachineState::PreparingProtocol,
            pending_effects: VecDeque::new(),
            active_effect_redelivery: false,
            next_effect_id: 1,
            next_synthetic_message_id,
            messages,
            progress_event_cursor: events.len(),
            events,
            turn_causes,
            protocol_iteration: protocol_run_offset,
            protocol_run_offset,
            cumulative_usage: TokenUsage::default(),
            termination: TurnTerminationPolicyState::new(),
            synced_protocol_iteration: None,
        }
    }

    /// Whether the machine has finished.
    pub fn is_done(&self) -> bool {
        matches!(self.state, MachineState::Finished)
    }

    pub fn messages(&self) -> Arc<Vec<Message>> {
        self.messages.shared()
    }

    pub fn events(&self) -> Arc<Vec<SessionHistoryRecord<M::Event>>> {
        Arc::clone(&self.events)
    }

    pub fn message_sequence(&self) -> MessageSequence {
        self.messages.clone()
    }

    pub fn protocol_iteration(&self) -> usize {
        self.protocol_iteration
    }

    pub fn checkpoint(&self) -> TurnCheckpoint<M> {
        let active_effect_id = self.state.outstanding_effect_id();
        let pending_effects = self
            .pending_effects
            .iter()
            .filter(|effect| active_effect_id.is_none_or(|id| effect.id() != Some(id)))
            .cloned()
            .collect::<Vec<_>>();
        TurnCheckpoint {
            state: self.state.clone(),
            pending_effects,
            next_effect_id: self.next_effect_id,
            next_synthetic_message_id: self.next_synthetic_message_id,
            messages: self.messages.iter().cloned().collect(),
            events: self.events.as_ref().clone(),
            turn_causes: self.turn_causes.clone(),
            progress_event_cursor: self.progress_event_cursor,
            protocol_iteration: self.protocol_iteration,
            protocol_run_offset: self.protocol_run_offset,
            cumulative_usage: self.cumulative_usage.clone(),
            termination: self.termination.clone(),
            synced_protocol_iteration: self.synced_protocol_iteration,
        }
    }

    pub fn restore_from_checkpoint(
        config: TurnMachineConfig<M>,
        checkpoint: TurnCheckpoint<M>,
    ) -> Self {
        let active_effect_id = checkpoint.state.outstanding_effect_id();
        let pending_effects = checkpoint
            .pending_effects
            .into_iter()
            .collect::<VecDeque<_>>();
        let active_effect_redelivery = active_effect_id.is_some()
            && !pending_effects
                .iter()
                .any(|effect| effect.id() == active_effect_id);
        Self {
            config,
            state: checkpoint.state,
            pending_effects,
            active_effect_redelivery,
            next_effect_id: checkpoint.next_effect_id,
            next_synthetic_message_id: checkpoint.next_synthetic_message_id,
            messages: MessageSequence::from_owned(checkpoint.messages),
            events: Arc::new(checkpoint.events),
            turn_causes: checkpoint.turn_causes,
            progress_event_cursor: checkpoint.progress_event_cursor,
            protocol_iteration: checkpoint.protocol_iteration,
            protocol_run_offset: checkpoint.protocol_run_offset,
            cumulative_usage: checkpoint.cumulative_usage,
            termination: checkpoint.termination,
            synced_protocol_iteration: checkpoint.synced_protocol_iteration,
        }
    }

    fn driver_context(&self) -> DriverContextView<'_, M> {
        DriverContextView {
            config: &self.config,
            messages: &self.messages,
            events: self.events.as_slice(),
            turn_causes: &self.turn_causes,
            protocol_iteration: self.protocol_iteration,
            protocol_run_offset: self.protocol_run_offset,
            termination: &self.termination,
        }
    }

    fn next_id(&mut self) -> EffectId {
        let id = EffectId(self.next_effect_id);
        self.next_effect_id += 1;
        id
    }

    fn next_synthetic_message_id(&mut self, scope: &str) -> String {
        let id = format!(
            "m_sansio_{}_{}_{}",
            self.protocol_run_offset, scope, self.next_synthetic_message_id
        );
        self.next_synthetic_message_id += 1;
        id
    }

    fn emit(&mut self, event: SessionStreamEvent) {
        self.pending_effects.push_back(Effect::Emit(event));
    }

    fn emit_progress(&mut self) {
        let event_delta = self.next_event_delta();
        self.pending_effects.push_back(Effect::Progress {
            messages: self.messages.clone(),
            event_delta,
            protocol_iteration: self.protocol_iteration,
        });
    }

    pub fn fail_turn(&mut self, event: SessionStreamEvent) {
        self.emit(event);
        self.finish(TurnOutcome::Stopped(TurnStop::RuntimeError));
    }

    pub fn finish_with_outcome(&mut self, outcome: TurnOutcome) {
        self.finish(outcome);
    }

    fn finish(&mut self, outcome: TurnOutcome) {
        self.emit(SessionStreamEvent::TurnOutcome { outcome });
        self.emit(SessionStreamEvent::Done);
        let msgs = std::mem::take(&mut self.messages);
        let event_delta = self.next_event_delta();
        let protocol_iteration = self.protocol_iteration;
        self.state = MachineState::Finished;
        self.pending_effects.push_back(Effect::Done {
            messages: msgs,
            event_delta,
            protocol_iteration,
        });
    }

    fn next_event_delta(&mut self) -> Vec<SessionHistoryRecord<M::Event>> {
        if self.progress_event_cursor >= self.events.len() {
            self.progress_event_cursor = self.events.len();
            return Vec::new();
        }
        let delta = self.events[self.progress_event_cursor..].to_vec();
        self.progress_event_cursor = self.events.len();
        delta
    }

    /// Drain the next pending effect. Returns `None` when the host must call
    /// `handle_response()` before more effects become available.
    pub fn poll_effect(&mut self) -> Option<Effect<M>> {
        if let Some(effect) = self.pending_effects.pop_front() {
            return Some(effect);
        }
        if self.active_effect_redelivery {
            self.active_effect_redelivery = false;
            if let Some(effect) = self.state.outstanding_effect() {
                return Some(effect);
            }
        }

        match &self.state {
            MachineState::PreparingProtocol => {
                self.prepare_protocol();
                self.pending_effects.pop_front()
            }
            MachineState::PrepareIteration => {
                self.prepare_protocol_iteration();
                self.pending_effects.pop_front()
            }
            _ => None,
        }
    }

    // ─── State transitions ───

    fn prepare_protocol(&mut self) {
        if self.config.sync_execution_environment {
            let id = self.next_id();
            self.state = MachineState::WaitingExecutionEnvironment {
                effect_id: id,
                update_machine_config: false,
            };
            self.pending_effects
                .push_back(Effect::SyncExecutionEnvironment {
                    id,
                    update_machine_config: false,
                });
            return;
        }

        self.prepare_protocol_iteration();
    }

    fn prepare_protocol_iteration(&mut self) {
        if self.config.sync_execution_environment
            && self.synced_protocol_iteration != Some(self.protocol_iteration)
        {
            let id = self.next_id();
            self.state = MachineState::WaitingExecutionEnvironment {
                effect_id: id,
                update_machine_config: true,
            };
            self.pending_effects
                .push_back(Effect::SyncExecutionEnvironment {
                    id,
                    update_machine_config: true,
                });
            return;
        }
        let actions = {
            let driver = Arc::clone(&self.config.protocol_driver);
            let ctx = self.driver_context();
            driver.prepare_protocol_iteration(ctx)
        };
        self.apply_actions(actions);
    }

    fn start_llm_request(
        &mut self,
        request: Arc<LlmRequest>,
        driver_state: Option<M::DriverState>,
    ) {
        let tool_list = self
            .config
            .tool_specs
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        self.emit(SessionStreamEvent::LlmRequest {
            protocol_iteration: self.protocol_iteration,
            message_count: self.messages.len(),
            tool_list,
        });

        let id = self.next_id();
        self.state = MachineState::WaitingLlm {
            effect_id: id,
            request: Arc::clone(&request),
            driver_state,
        };
        self.pending_effects
            .push_back(Effect::LlmCall { id, request });
    }

    fn start_tool_calls(&mut self, calls: Vec<PendingToolCall>) {
        let effect_id = self.next_id();
        self.state = MachineState::WaitingTools {
            effect_id,
            calls: calls.clone(),
        };
        self.pending_effects.push_back(Effect::ToolCalls {
            id: effect_id,
            calls,
        });
    }

    fn start_exec(&mut self, language: String, code: String, driver_state: M::DriverState) {
        let effect_id = self.next_id();
        self.state = MachineState::WaitingExec {
            effect_id,
            language: language.clone(),
            code: code.clone(),
            driver_state,
        };
        self.pending_effects.push_back(Effect::ExecCode {
            id: effect_id,
            language,
            code,
        });
    }

    fn schedule_turn_limit_final(&mut self, message: Message) -> bool {
        let Some(_max_turns) = self.termination.turn_limit_final_to_schedule(
            self.protocol_iteration,
            self.protocol_run_offset,
            self.config.turn_budget,
        ) else {
            return false;
        };
        self.termination.mark_turn_limit_final_scheduled();
        self.messages.push(message);
        true
    }

    fn schedule_configured_turn_limit_final(&mut self) -> bool {
        let Some(max_turns) = self.termination.turn_limit_final_to_schedule(
            self.protocol_iteration,
            self.protocol_run_offset,
            self.config.turn_budget,
        ) else {
            return false;
        };
        let message_id = self.next_synthetic_message_id("turn_limit");
        let message = (self.config.turn_limit_final_message)(message_id, max_turns);
        self.termination.mark_turn_limit_final_scheduled();
        self.messages.push(message);
        true
    }

    fn append_event(&mut self, event: SessionHistoryRecord<M::Event>) {
        match event {
            SessionHistoryRecord::Conversation(record) => {
                Arc::make_mut(&mut self.events)
                    .push(SessionHistoryRecord::Conversation(record.clone()));
                self.messages.push(record.to_message());
            }
            SessionHistoryRecord::Protocol(protocol_event) => {
                Arc::make_mut(&mut self.events).push(SessionHistoryRecord::Protocol(protocol_event));
            }
        }
    }

    pub fn apply_actions(&mut self, actions: Vec<DriverAction<M>>) {
        let mut progress_dirty = false;
        for action in actions {
            match action {
                DriverAction::Emit(event) => self.emit(event),
                DriverAction::AppendEvents(events) => {
                    if !events.is_empty() {
                        for event in events {
                            self.append_event(event);
                        }
                        progress_dirty = true;
                    }
                }
                DriverAction::StartLlm {
                    request,
                    driver_state,
                } => self.start_llm_request(request, driver_state),
                DriverAction::StartTools { calls } => self.start_tool_calls(calls),
                DriverAction::StartExec {
                    language,
                    code,
                    driver_state,
                } => self.start_exec(language, code, driver_state),
                DriverAction::StartCheckpoint {
                    checkpoint,
                    on_empty,
                } => self.request_checkpoint(checkpoint, on_empty),
                DriverAction::AdvanceProtocolIteration => {
                    self.protocol_iteration += 1;
                    self.synced_protocol_iteration = None;
                    progress_dirty = true;
                }
                DriverAction::ScheduleTurnLimitFinal { message } => {
                    if self.schedule_turn_limit_final(message) {
                        progress_dirty = true;
                    }
                }
                DriverAction::Finish(outcome) => {
                    if progress_dirty {
                        self.emit_progress();
                        progress_dirty = false;
                    }
                    self.finish(outcome);
                    break;
                }
            }
        }
        if progress_dirty {
            self.emit_progress();
        }
    }

    /// Feed a response to a previously emitted effect.
    pub fn handle_response(&mut self, response: Response) {
        if let Err(overflow) = self.try_handle_response(response) {
            self.fail_turn(make_error_event(
                "token_usage_accounting",
                Some("token_usage_overflow"),
                format!(
                    "token usage counter `{}` overflowed while accumulating turn usage",
                    overflow.counter()
                ),
                None,
            ));
        }
    }

    /// Fallible host seam for delivering a response whose usage must remain
    /// suitable for durable accumulation.
    #[doc(hidden)]
    pub fn try_handle_response(
        &mut self,
        response: Response,
    ) -> Result<(), TokenUsageOverflow> {
        self.active_effect_redelivery = false;
        match response {
            Response::ExecutionEnvironmentSynced { id, result } => {
                self.handle_execution_environment_synced(id, result);
            }
            Response::LlmComplete {
                id,
                result,
                text_streamed,
            } => self.handle_llm_complete(id, result, text_streamed)?,
            Response::ToolResults { id, results } => self.handle_tool_results(id, results),
            Response::ExecResult { id, result } => self.handle_exec_result(id, result),
            Response::Checkpoint { id, delivery } => self.handle_checkpoint(id, delivery),
        }
        Ok(())
    }

    fn request_checkpoint(&mut self, checkpoint: CheckpointKind, on_empty: CheckpointResumeAction) {
        let id = self.next_id();
        self.state = MachineState::WaitingCheckpoint {
            effect_id: id,
            checkpoint,
            on_empty,
        };
        self.pending_effects
            .push_back(Effect::Checkpoint { id, checkpoint });
    }

    fn handle_execution_environment_synced(
        &mut self,
        id: EffectId,
        result: Result<Option<ExecutionEnvironmentSync>, String>,
    ) {
        let (waiting_id, waiting_update_machine_config) =
            match std::mem::replace(&mut self.state, MachineState::Finished) {
                MachineState::WaitingExecutionEnvironment {
                    effect_id,
                    update_machine_config,
                } => (effect_id, update_machine_config),
                other => {
                    self.state = other;
                    return;
                }
            };
        if waiting_id != id {
            self.state = MachineState::WaitingExecutionEnvironment {
                effect_id: waiting_id,
                update_machine_config: waiting_update_machine_config,
            };
            return;
        }

        match result {
            Ok(update) => {
                if let Some(update) = update {
                    self.config.system_prompt = update.system_prompt;
                    self.config.tool_specs = update.tool_specs;
                }
                self.synced_protocol_iteration = Some(self.protocol_iteration);
                self.state = MachineState::PrepareIteration;
            }
            Err(error) => {
                self.fail_turn(make_error_event(
                    "execution_environment",
                    Some("reconfigure_failed"),
                    format!("Failed to refresh execution environment: {error}"),
                    Some(error),
                ));
            }
        }
    }

    fn append_checkpoint_messages(&mut self, plugin_messages: &[PluginMessage], transient: bool) {
        let mut appended = Vec::new();
        for message in plugin_messages
            .iter()
            .filter(|message| matches!(message.role, MessageRole::User | MessageRole::System))
        {
            let message_id = self.next_synthetic_message_id("checkpoint");
            let mut parts = if message.parts.is_empty() && !message.content.is_empty() {
                vec![Part::text(format!("{message_id}.p0"), message.content.clone(), None)]
            } else {
                message.parts.clone()
            };
            parts.extend(message.attachments.iter().cloned().map(|source| Part::attachment_part(String::new(), String::new(), Some(crate::PartAttachment { source }))));
            reassign_part_ids(&message_id, &mut parts);
            appended.push(Message {
                id: message_id.clone(),
                role: message.role,
                parts: Arc::new(parts),
                origin: message.origin.clone().or_else(|| {
                    Some(MessageOrigin::Plugin {
                        plugin_id: "plugin".to_string(),
                        transient,
                    })
                }),
            });
        }
        if !appended.is_empty() {
            self.messages.extend(appended);
        }
    }

    fn append_turn_causes(&mut self, causes: Vec<TurnCause>) {
        if causes.is_empty() {
            return;
        }
        let mut existing_ids = self
            .turn_causes
            .iter()
            .map(|cause| cause.id.clone())
            .collect::<HashSet<_>>();
        for cause in causes {
            if !existing_ids.insert(cause.id.clone()) {
                continue;
            }
            self.messages.push(cause.to_event_message());
            self.turn_causes.push(cause);
        }
    }

    fn handle_checkpoint(&mut self, id: EffectId, delivery: CheckpointDelivery) {
        let (effect_id, checkpoint, on_empty) =
            match std::mem::replace(&mut self.state, MachineState::Finished) {
                MachineState::WaitingCheckpoint {
                    effect_id,
                    checkpoint,
                    on_empty,
                } => (effect_id, checkpoint, on_empty),
                other => {
                    self.state = other;
                    return;
                }
            };
        if effect_id != id {
            self.state = MachineState::WaitingCheckpoint {
                effect_id,
                checkpoint,
                on_empty,
            };
            return;
        }

        if !delivery.committed_user_messages.is_empty()
            || !delivery.messages.is_empty()
            || !delivery.transient_messages.is_empty()
            || !delivery.turn_causes.is_empty()
        {
            self.messages.extend(delivery.committed_user_messages);
            self.append_checkpoint_messages(&delivery.messages, false);
            self.append_checkpoint_messages(&delivery.transient_messages, true);
            self.append_turn_causes(delivery.turn_causes);
            if matches!(checkpoint, CheckpointKind::BeforeCompletion) {
                self.protocol_iteration += 1;
                if self.termination.should_force_exit_after_grace_turn() {
                    self.emit_progress();
                    self.finish(TurnOutcome::Stopped(TurnStop::MaxTurns));
                    return;
                }
                self.schedule_configured_turn_limit_final();
            }
            self.state = MachineState::PrepareIteration;
            self.emit_progress();
            return;
        }

        match on_empty {
            CheckpointResumeAction::PrepareIteration => {
                self.state = MachineState::PrepareIteration;
            }
            CheckpointResumeAction::Finish(outcome) => self.finish(outcome),
        }
    }

    fn take_waiting_llm_state(&mut self, id: EffectId) -> Option<WaitingLlmState<M>> {
        match std::mem::replace(&mut self.state, MachineState::Finished) {
            MachineState::WaitingLlm {
                effect_id,
                request,
                driver_state,
            } if effect_id == id => Some(WaitingLlmState {
                request,
                driver_state,
            }),
            other => {
                self.state = other;
                None
            }
        }
    }

    fn handle_llm_complete(
        &mut self,
        id: EffectId,
        result: Result<LlmResponse, LlmCallError>,
        text_streamed: bool,
    ) -> Result<(), TokenUsageOverflow> {
        let Some(waiting) = self.take_waiting_llm_state(id) else {
            return Ok(());
        };
        match result {
            Err(error) if error.terminal_reason == LlmTerminalReason::Cancelled => {
                self.finish(TurnOutcome::Stopped(TurnStop::Cancelled));
            }
            Err(error) => {
                self.emit_llm_error(error);
            }
            Ok(mut llm_response) => {
                // Admit the provider's raw counters once, before any consumer
                // aggregates them.
                let (usage, prompt_input_tokens) =
                    checked_turn_usage_from_llm_usage(&llm_response.usage)?;
                // Reclassify a zero-output `OutputLimit` as `ContextOverflow`
                // when the prompt nearly filled the window, before the terminal
                // reason drives the finish decision below.
                refine_terminal_reason_for_context_window(
                    &mut llm_response,
                    prompt_input_tokens,
                    self.config.max_context_tokens,
                );
                self.record_llm_usage(
                    &llm_response,
                    usage,
                    self.llm_response_text(&llm_response),
                )?;
                if self.handle_terminal_llm_response(&llm_response, text_streamed) {
                    return Ok(());
                }
                let actions = {
                    let driver = Arc::clone(&self.config.protocol_driver);
                    let ctx = self.driver_context();
                    driver.handle_llm_success(ctx, waiting, llm_response, text_streamed)
                };
                self.apply_actions(actions);
            }
        }
        Ok(())
    }

    fn handle_terminal_llm_response(
        &mut self,
        llm_response: &LlmResponse,
        text_streamed: bool,
    ) -> bool {
        let outcome = match llm_response.terminal_reason {
            LlmTerminalReason::OutputLimit
                if self.config.protocol_driver.handles_output_limit_response() =>
            {
                return false;
            }
            LlmTerminalReason::OutputLimit => TurnOutcome::Stopped(TurnStop::Incomplete),
            LlmTerminalReason::ContextOverflow => TurnOutcome::Stopped(TurnStop::ProviderError),
            LlmTerminalReason::ContentFilter => TurnOutcome::Stopped(TurnStop::ProviderError),
            LlmTerminalReason::ProviderError => TurnOutcome::Stopped(TurnStop::ProviderError),
            LlmTerminalReason::Cancelled => TurnOutcome::Stopped(TurnStop::Cancelled),
            LlmTerminalReason::Stop | LlmTerminalReason::ToolUse | LlmTerminalReason::Unknown => {
                return false;
            }
        };

        let visible_text = self
            .config
            .protocol_driver
            .project_visible_assistant_prose(&llm_response.full_text);
        if !text_streamed && !visible_text.is_empty() {
            self.emit(SessionStreamEvent::TextDelta {
                content: visible_text.clone(),
            });
        }
        self.emit(SessionStreamEvent::LlmResponse {
            protocol_iteration: self.protocol_iteration,
            content: visible_text,
            duration_ms: 0,
        });
        let reason = llm_response.terminal_reason;
        let diagnostic = llm_response
            .terminal_diagnostic
            .clone()
            .unwrap_or_else(|| format!("Model call ended with terminal reason {reason:?}."));
        let mut envelope = crate::session_model::make_error_envelope(
            "llm_provider",
            Some(reason.code()),
            Some(reason),
            diagnostic.clone(),
            None,
        );
        // A terminal reason is a deterministic outcome of a completed call
        // (overflow, filter, cancellation): replaying the identical request
        // reproduces it, so the source knows it is not retryable.
        envelope.retryable = Some(false);
        self.emit(SessionStreamEvent::Error {
            message: diagnostic,
            envelope: Some(envelope),
        });
        self.finish(outcome);
        true
    }

    fn llm_response_text<'a>(&self, llm_response: &'a LlmResponse) -> &'a str {
        &llm_response.full_text
    }

    fn llm_response_debug_parts(&self, llm_response: &LlmResponse) -> Option<Value> {
        let parts = llm_response
            .parts
            .iter()
            .filter_map(|part| match part {
                LlmOutputPart::Text { text, .. } if !text.is_empty() => Some(serde_json::json!({
                    "type": "text",
                    "text": text,
                })),
                LlmOutputPart::Text { .. } => None,
                LlmOutputPart::Reasoning {
                    text,
                    replay,
                } => Some(serde_json::json!({
                    "type": "reasoning",
                    "id": replay.as_ref().and_then(|meta| meta.item_id.as_ref()),
                    "summary": replay.as_ref().map(|meta| &meta.summary),
                    "text": text,
                    "has_encrypted": replay.as_ref().is_some_and(|meta| meta.encrypted_content.is_some() || meta.signature.is_some()),
                    "redacted": replay.as_ref().is_some_and(|meta| meta.redacted),
                })),
                LlmOutputPart::ToolCall {
                    call_id,
                    tool_name,
                    input_json,
                    replay,
                } => Some(serde_json::json!({
                    "type": "tool_call",
                    "call_id": call_id,
                    "tool_name": tool_name,
                    "input_json": input_json,
                    "id": replay.as_ref().and_then(|meta| meta.item_id.as_ref()),
                    "has_opaque": replay.as_ref().is_some_and(|meta| meta.opaque.is_some()),
                })),
            })
            .collect::<Vec<_>>();
        (!parts.is_empty()).then_some(Value::Array(parts))
    }

    /// Accumulates the turn's usage from counters already admitted by
    /// [`checked_turn_usage_from_llm_usage`].
    fn record_llm_usage(
        &mut self,
        llm_response: &LlmResponse,
        usage: TokenUsage,
        response_text: &str,
    ) -> Result<(), TokenUsageOverflow> {
        self.cumulative_usage = self.cumulative_usage.checked_add(&usage)?;
        self.emit(SessionStreamEvent::TokenUsage {
            protocol_iteration: self.protocol_iteration,
            usage: usage.clone(),
            cumulative: self.cumulative_usage.clone(),
        });
        if self.config.emit_llm_trace {
            let response_parts = self.llm_response_debug_parts(llm_response);
            self.pending_effects.push_back(Effect::Log {
                event: LogEvent::LlmDebug {
                    session_id: self.config.session_id.clone(),
                    protocol_iteration: self.protocol_iteration,
                    usage,
                    provider_usage: llm_response.provider_usage.clone(),
                    request_body: llm_response.request_body.clone(),
                    response_text: response_text.to_string(),
                    response_parts,
                },
            });
        }
        Ok(())
    }

    fn record_llm_error(&mut self, error: &LlmCallError) {
        if self.config.emit_llm_trace {
            self.pending_effects.push_back(Effect::Log {
                event: LogEvent::LlmError {
                    session_id: self.config.session_id.clone(),
                    protocol_iteration: self.protocol_iteration,
                    request_body: error.request_body.clone(),
                    message: error.message.clone(),
                    retryable: error.retryable,
                    raw: error.raw.clone(),
                    code: error.code.clone(),
                    terminal_reason: error.terminal_reason,
                },
            });
        }
    }

    fn emit_llm_error(&mut self, error: LlmCallError) {
        self.record_llm_error(&error);
        let mut envelope = crate::session_model::make_error_envelope(
            "llm_provider",
            error.code.as_deref(),
            Some(error.terminal_reason),
            format!("LLM error: {}", error.message),
            error.raw.clone(),
        );
        // Carry the transport's typed signals through to the envelope (and
        // from there to `TurnIssue`): retryability is always classified, the
        // failure kind only when the source knew it (`Unknown` stays absent).
        envelope.retryable = Some(error.retryable);
        envelope.provider_failure_kind =
            (error.kind != crate::llm::types::ProviderFailureKind::Unknown).then_some(error.kind);
        self.emit(SessionStreamEvent::Error {
            message: format!("LLM error: {}", error.message),
            envelope: Some(envelope),
        });
        self.finish(TurnOutcome::Stopped(TurnStop::ProviderError));
    }

    fn handle_tool_results(&mut self, id: EffectId, completed: Vec<CompletedToolCall>) {
        let (waiting_effect_id, waiting_calls) =
            match std::mem::replace(&mut self.state, MachineState::Finished) {
                MachineState::WaitingTools { effect_id, calls } => (effect_id, calls),
                other => {
                    self.state = other;
                    return;
                }
            };

        if waiting_effect_id != id {
            self.state = MachineState::WaitingTools {
                effect_id: waiting_effect_id,
                calls: waiting_calls,
            };
            return;
        }

        for outcome in &completed {
            self.emit(SessionStreamEvent::ToolCall {
                call_id: Some(outcome.call_id.clone()),
                name: outcome.tool_name.clone(),
                args: outcome.args.clone(),
                output: outcome.output.clone(),
                duration_ms: outcome.duration_ms,
            });
        }

        let actions = {
            let driver = Arc::clone(&self.config.protocol_driver);
            let ctx = self.driver_context();
            driver.handle_tool_results(ctx, completed)
        };
        self.apply_actions(actions);
    }

    fn take_waiting_exec_state(&mut self, id: EffectId) -> Option<WaitingExecState<M>> {
        match std::mem::replace(&mut self.state, MachineState::Finished) {
            MachineState::WaitingExec {
                effect_id,
                code: _,
                driver_state,
                ..
            } if effect_id == id => Some(WaitingExecState { driver_state }),
            other => {
                self.state = other;
                None
            }
        }
    }

    fn handle_exec_result(&mut self, id: EffectId, result: Result<crate::ExecResponse, String>) {
        let Some(waiting) = self.take_waiting_exec_state(id) else {
            return;
        };
        let actions = {
            let driver = Arc::clone(&self.config.protocol_driver);
            let ctx = self.driver_context();
            driver.handle_exec_result(ctx, waiting, result)
        };
        self.apply_actions(actions);
    }
}
