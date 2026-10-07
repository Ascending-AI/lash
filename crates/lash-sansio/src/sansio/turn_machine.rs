use super::*;

impl<M: TurnProtocol> TurnMachine<M> {
    /// Restore only the Prompt View retained by the environment prelude.
    pub fn adopt_prepared_messages(&mut self, messages: crate::MessageSequence, first_sync: bool) {
        if first_sync {
            self.next_synthetic_message_id = self.messages.len() as u64;
        }
        self.prompt_messages = messages;
    }

    /// Adopt the real inputs and outputs retained by a recorded sync.
    pub fn adopt_committed_messages(&mut self, messages: crate::MessageSequence) {
        self.messages = messages;
    }

    /// The view used only to project model requests.
    pub fn prompt_message_sequence(&self) -> MessageSequence {
        self.prompt_messages.clone()
    }

    pub fn new(
        config: TurnMachineConfig<M>,
        messages: Vec<Message>,
        events: crate::AppendVec<SessionHistoryRecord<M::Event>>,
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
        events: crate::AppendVec<SessionHistoryRecord<M::Event>>,
        protocol_run_offset: usize,
    ) -> Self {
        Self::new_shared_with_turn_causes(config, messages, events, protocol_run_offset, Vec::new())
    }

    pub fn new_shared_with_turn_causes(
        config: TurnMachineConfig<M>,
        messages: MessageSequence,
        events: crate::AppendVec<SessionHistoryRecord<M::Event>>,
        protocol_run_offset: usize,
        turn_causes: Vec<TurnCause>,
    ) -> Self {
        let next_synthetic_message_id = messages.len() as u64;
        Self {
            config,
            state: MachineState::PreparingProtocol,
            side_effect_outbox: VecDeque::new(),
            next_effect_id: 1,
            next_synthetic_message_id,
            window: None,
            prompt_messages: messages.clone(),
            messages,
            progress_event_cursor: events.len(),
            events,
            turn_causes,
            protocol_iteration: protocol_run_offset,
            protocol_run_offset,
            cumulative_usage: TokenUsage::default(),
            environment: None,
            observed_cancellation: None,
            resume_work: None,
            run_abort: None,
        }
    }

    /// A machine for a turn that starts from the committed `window`
    /// (FIG-5206): its messages are `messages`, which lead with the window's
    /// (see [`TurnWindow::then`]), and its history is the window's records
    /// followed by `turn_events`. Its checkpoint names the window by its pin
    /// instead of holding it, and [`Self::restore_from_checkpoint`] is handed
    /// the same window again.
    pub fn in_window(
        config: TurnMachineConfig<M>,
        window: TurnWindow<M::Event>,
        messages: MessageSequence,
        turn_events: Vec<SessionHistoryRecord<M::Event>>,
        protocol_run_offset: usize,
        turn_causes: Vec<TurnCause>,
    ) -> Self {
        let mut events = window.events().clone();
        for event in turn_events {
            events.push(event);
        }
        let mut machine = Self::new_shared_with_turn_causes(
            config,
            messages,
            events,
            protocol_run_offset,
            turn_causes,
        );
        machine.window = Some(window);
        machine
    }

    /// Start this machine at `work` instead of at the driver's first step
    /// (FIG-4739): the turn continues a run whose earlier turn ended at a
    /// segment boundary while it waited on `work`, and issues it again. The
    /// machine syncs its environment first, as every turn does, and then
    /// waits on `work` where the driver would have prepared an iteration.
    pub fn resume_with(&mut self, work: PendingWork<M>) {
        self.resume_work = Some(work);
    }

    /// The code execution the machine waits on, if that is what it waits on:
    /// its language, its code and the driver state that answers it.
    pub fn waiting_exec(&self) -> Option<(&str, &str, &M::DriverState)> {
        match &self.state {
            MachineState::Waiting {
                work:
                    PendingWork::Exec {
                        language,
                        code,
                        driver_state,
                    },
                ..
            } => Some((language, code, driver_state)),
            _ => None,
        }
    }

    pub fn settle_tool_dispatch(&mut self, state: serde_json::Value) -> bool {
        match &mut self.state {
            MachineState::Waiting {
                work: PendingWork::WaitingForToolResults { calls, settled, .. },
                delivery,
                ..
            } => {
                calls.clear();
                *settled = Some(state);
                *delivery = EffectDeliveryStatus::Pending;
                true
            }
            _ => false,
        }
    }

    /// The tool round the machine waits on, if that is what it waits on: its
    /// calls while the runtime has not admitted them, its settled dispatch
    /// state once it has, and how its slots fold back into the response.
    pub fn waiting_tool_round(
        &self,
    ) -> Option<(
        &[PendingToolCall],
        Option<&serde_json::Value>,
        &crate::sansio::ToolExpansionPlan,
    )> {
        match &self.state {
            MachineState::Waiting {
                work:
                    PendingWork::WaitingForToolResults {
                        calls,
                        settled,
                        expansion,
                    },
                ..
            } => Some((calls, settled.as_ref(), expansion)),
            _ => None,
        }
    }

    /// Record the cancellation request the host has observed for this turn.
    /// The first observation wins; later ones are ignored.
    pub fn record_cancellation_evidence(&mut self, evidence: crate::TurnCancellationEvidence) {
        if self.observed_cancellation.is_none() {
            self.observed_cancellation = Some(evidence);
        }
    }

    /// Evidence for a cancellation the machine is about to record, falling
    /// back to lash-internal evidence when the host observed no request (a
    /// provider-side abort classified as cancelled).
    fn cancellation_evidence(&self) -> crate::TurnCancellationEvidence {
        self.observed_cancellation.clone().unwrap_or_else(|| {
            crate::TurnCancellationEvidence::internal(format!(
                "provider-cancelled:{}",
                self.protocol_iteration
            ))
        })
    }

    pub fn is_done(&self) -> bool {
        matches!(self.state, MachineState::Finished)
    }

    pub fn messages(&self) -> crate::AppendVec<Message> {
        self.messages.shared()
    }

    pub fn events(&self) -> crate::AppendVec<SessionHistoryRecord<M::Event>> {
        self.events.clone()
    }

    /// The history records the machine has delivered through its progress
    /// boundaries: its history up to its progress cursor, the history it
    /// started from leading it. A restored machine's cursor is its
    /// checkpoint's, so these are the records an earlier owner consumed.
    pub fn progressed_events(&self) -> &[SessionHistoryRecord<M::Event>] {
        let events = self.events.as_slice();
        events.get(..self.progress_event_cursor).unwrap_or(events)
    }

    pub fn message_sequence(&self) -> MessageSequence {
        self.messages.clone()
    }

    pub fn protocol_iteration(&self) -> usize {
        self.protocol_iteration
    }

    /// The configuration the machine was built with, the one its checkpoint
    /// restores under ([`Self::restore_from_checkpoint`]).
    pub fn into_config(self) -> TurnMachineConfig<M> {
        self.config
    }

    /// The machine's bounded checkpoint, with the transcript content it
    /// names by digest: what the turn added to the window it started from.
    pub fn checkpoint(&self) -> SavedTurn<M> {
        let mut content = TurnCheckpointContent::default();
        let window = self.window.as_ref();
        let window_events = window.map_or(0, |window| window.events().len());
        let checkpoint = TurnCheckpoint {
            schema_version: TURN_CHECKPOINT_SCHEMA_VERSION,
            state: CheckpointState::record(&self.state, &mut content, window),
            pending_effects: self
                .side_effect_outbox
                .iter()
                .cloned()
                .map(|effect| match effect {
                    Effect::Emit(SessionStreamEvent::Error {
                        message: _,
                        envelope: Some(mut envelope),
                    }) if matches!(
                        envelope.kind,
                        crate::session_model::TurnFailureKind::LlmProvider
                    ) =>
                    {
                        envelope.raw = None;
                        envelope.user_message = "provider call failed".to_string();
                        Effect::Emit(SessionStreamEvent::Error {
                            message: envelope.user_message.clone(),
                            envelope: Some(envelope),
                        })
                    }
                    effect => effect,
                })
                .collect(),
            next_effect_id: self.next_effect_id,
            next_synthetic_message_id: self.next_synthetic_message_id,
            window: window.map(CheckpointWindow::of),
            messages: CheckpointMessages::record(&self.messages, &mut content, window),
            prompt_messages: CheckpointMessages::record(
                &self.prompt_messages,
                &mut content,
                window,
            ),
            // The machine only appends to its history, so the window's
            // records lead it.
            events: content.put_sequence(
                self.events
                    .as_slice()
                    .get(window_events..)
                    .unwrap_or_default(),
            ),
            turn_causes: self.turn_causes.clone(),
            progress_event_cursor: self.progress_event_cursor,
            protocol_iteration: self.protocol_iteration,
            protocol_run_offset: self.protocol_run_offset,
            cumulative_usage: self.cumulative_usage.clone(),
            environment: self.environment.clone(),
        };
        SavedTurn {
            checkpoint,
            content,
        }
    }

    /// Re-hydrate a machine from a checkpoint, the content it names and the
    /// committed window it started from ([`TurnCheckpoint::window_pin`]). The
    /// schema version, window and environment are validated before any
    /// content is read; content that is missing or not the bytes its digest
    /// names is refused.
    pub fn restore_from_checkpoint(
        config: TurnMachineConfig<M>,
        saved: SavedTurn<M>,
        window: Option<TurnWindow<M::Event>>,
    ) -> Result<Self, TurnCheckpointRestoreError> {
        let SavedTurn {
            checkpoint,
            content,
        } = saved;
        if checkpoint.schema_version != TURN_CHECKPOINT_SCHEMA_VERSION {
            return Err(TurnCheckpointRestoreError::IncompatibleSchemaVersion {
                actual: checkpoint.schema_version,
                expected: TURN_CHECKPOINT_SCHEMA_VERSION,
            });
        }
        // Only a machine that has yet to sync holds no environment: every
        // other wait was started by a driver that projected from one.
        if checkpoint.environment.is_none() && checkpoint.state.waits_on_driver_work() {
            return Err(TurnCheckpointRestoreError::IncompatibleFormat {
                message: "a checkpoint waiting on driver work records no execution environment"
                    .to_string(),
            });
        }
        CheckpointWindow::check(checkpoint.window.as_ref(), window.as_ref())?;
        let side_effect_outbox = checkpoint
            .pending_effects
            .into_iter()
            .collect::<VecDeque<_>>();
        let messages = checkpoint.messages.restore(&content, window.as_ref())?;
        let prompt_messages = checkpoint
            .prompt_messages
            .restore(&content, window.as_ref())?;
        let mut events = window
            .as_ref()
            .map(|window| window.events().clone())
            .unwrap_or_default();
        for event in content.sequence(&checkpoint.events)? {
            events.push(event);
        }
        Ok(Self {
            config,
            state: checkpoint.state.restore(&content, window.as_ref())?,
            side_effect_outbox,
            next_effect_id: checkpoint.next_effect_id,
            next_synthetic_message_id: checkpoint.next_synthetic_message_id,
            window,
            messages,
            prompt_messages,
            events,
            turn_causes: checkpoint.turn_causes,
            progress_event_cursor: checkpoint.progress_event_cursor,
            protocol_iteration: checkpoint.protocol_iteration,
            protocol_run_offset: checkpoint.protocol_run_offset,
            cumulative_usage: checkpoint.cumulative_usage,
            environment: checkpoint.environment,
            observed_cancellation: None,
            resume_work: None,
            run_abort: None,
        })
    }

    /// Run one driver step over the synced environment and apply the
    /// actions it returns.
    fn shift(
        &mut self,
        step: impl FnOnce(
            &dyn ProtocolDriverHandle<M>,
            DriverContextView<'_, M>,
        ) -> Vec<DriverAction<M>>,
    ) {
        let driver = Arc::clone(&self.config.protocol_driver);
        let Some(environment) = self.environment.as_ref() else {
            // A fresh machine syncs before it prepares, and restore refuses a
            // checkpoint that waits on driver work with no environment.
            self.fail_turn(make_error_event(
                crate::session_model::TurnFailureKind::ExecutionEnvironment,
                Some(crate::session_model::TurnFailureCode::ReconfigureFailed.into()),
                "the turn has no synced execution environment",
                None,
            ));
            return;
        };
        let actions = step(
            driver.as_ref(),
            DriverContextView {
                config: &self.config,
                messages: &self.messages,
                prompt_messages: &self.prompt_messages,
                events: self.events.as_slice(),
                turn_causes: &self.turn_causes,
                protocol_iteration: self.protocol_iteration,
                protocol_run_offset: self.protocol_run_offset,
                observed_cancellation: self.observed_cancellation.as_ref(),
                environment: &environment.sync,
            },
        );
        self.apply_actions(actions);
    }

    fn next_id(&mut self) -> EffectId {
        let id = EffectId(self.next_effect_id);
        self.next_effect_id += 1;
        id
    }

    fn emit(&mut self, event: SessionStreamEvent) {
        self.side_effect_outbox.push_back(Effect::Emit(event));
    }

    fn emit_progress(&mut self) {
        let event_delta = self.next_event_delta();
        self.side_effect_outbox.push_back(Effect::Progress {
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

    /// Stop the Run for a tool check's abort: report the plugin's typed
    /// cause, then finish with the plugin-abort stop.
    fn finish_run_abort(&mut self, abort: RunAbort) {
        self.emit(make_error_event(
            crate::session_model::TurnFailureKind::Plugin,
            Some(abort.code),
            abort.message,
            None,
        ));
        self.finish(TurnOutcome::Stopped(TurnStop::PluginAbort));
    }

    fn finish(&mut self, outcome: TurnOutcome) {
        self.emit(SessionStreamEvent::TurnOutcome { outcome });
        self.emit(SessionStreamEvent::Done);
        let msgs = std::mem::take(&mut self.messages);
        let event_delta = self.next_event_delta();
        let protocol_iteration = self.protocol_iteration;
        self.state = MachineState::Finished;
        self.side_effect_outbox.push_back(Effect::Done {
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
        if let Some(effect) = self.poll_scheduled_effect() {
            return Some(effect);
        }

        match &self.state {
            MachineState::PreparingProtocol => {
                self.prepare_protocol();
                self.poll_scheduled_effect()
            }
            MachineState::PrepareIteration => {
                self.prepare_protocol_iteration();
                self.poll_scheduled_effect()
            }
            _ => None,
        }
    }

    fn poll_scheduled_effect(&mut self) -> Option<Effect<M>> {
        if let Some(effect) = self.side_effect_outbox.pop_front() {
            return Some(effect);
        }
        self.state.poll_outstanding_effect()
    }

    // ─── State transitions ───

    /// The protocol-start sync: the only way an environment reaches the
    /// machine.
    fn prepare_protocol(&mut self) {
        self.start(PendingWork::SyncExecutionEnvironment);
    }

    fn prepare_protocol_iteration(&mut self) {
        if self
            .config
            .turn_budget
            .max_turns()
            .is_some_and(|max_turns| {
                self.protocol_iteration
                    .saturating_sub(self.protocol_run_offset)
                    >= max_turns
            })
        {
            self.finish(TurnOutcome::Stopped(TurnStop::MaxTurns));
            return;
        }
        if self
            .environment
            .as_ref()
            .is_none_or(|environment| environment.protocol_iteration != self.protocol_iteration)
        {
            self.start(PendingWork::SyncExecutionEnvironment);
            return;
        }
        if let Some(work) = self.resume_work.take() {
            self.start(work);
            return;
        }
        self.shift(|driver, ctx| driver.prepare_protocol_iteration(ctx));
    }

    /// Wait on the host to fulfil `work`. Its effect is delivered by
    /// [`MachineState::poll_outstanding_effect`] after every side effect
    /// already queued, so the model call's `LlmRequest` emit and any pending
    /// progress reach the host first.
    fn start(&mut self, work: PendingWork<M>) {
        if matches!(work, PendingWork::Llm { .. }) {
            let tool_list = self
                .environment
                .iter()
                .flat_map(|environment| environment.sync.tool_specs.iter())
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            self.emit(SessionStreamEvent::LlmRequest {
                protocol_iteration: self.protocol_iteration,
                message_count: self.prompt_messages.len(),
                tool_list,
            });
        }
        let effect_id = self.next_id();
        self.state = MachineState::Waiting {
            effect_id,
            work,
            delivery: EffectDeliveryStatus::Pending,
        };
    }

    pub(super) fn append_event(&mut self, event: SessionHistoryRecord<M::Event>) {
        match event {
            SessionHistoryRecord::Conversation(record) => {
                self.events
                    .push(SessionHistoryRecord::Conversation(record.clone()));
                self.prompt_messages.push(record.to_message());
                self.messages.push(record.to_message());
            }
            SessionHistoryRecord::Protocol(protocol_event) => {
                self.events
                    .push(SessionHistoryRecord::Protocol(protocol_event));
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
                DriverAction::Start(work) => {
                    if let Some(abort) = self.run_abort.take() {
                        if progress_dirty {
                            self.emit_progress();
                            progress_dirty = false;
                        }
                        self.finish_run_abort(abort);
                        break;
                    }
                    self.start(work);
                }
                DriverAction::ReportToolCalls { completed } => {
                    let accounting = completed
                        .iter()
                        .map(|outcome| SessionStreamEvent::ToolCall {
                            call_id: outcome.call_id.clone(),
                            provider_call_id: outcome.provider_call_id.clone(),
                            name: outcome.tool_name.clone(),
                            args: outcome.args.clone(),
                            output: outcome.output.clone(),
                        })
                        .collect::<Vec<_>>();
                    self.side_effect_outbox
                        .push_back(Effect::ReportToolCalls { completed });
                    for event in accounting {
                        self.emit(event);
                    }
                }
                DriverAction::AdvanceProtocolIteration => {
                    self.protocol_iteration += 1;
                    progress_dirty = true;
                }
                DriverAction::FinishCancelled { evidence } => {
                    if progress_dirty {
                        self.emit_progress();
                        progress_dirty = false;
                    }
                    self.finish(TurnOutcome::Stopped(TurnStop::Cancelled { evidence }));
                    break;
                }
                DriverAction::Finish(outcome) => {
                    if progress_dirty {
                        self.emit_progress();
                        progress_dirty = false;
                    }
                    match self.run_abort.take() {
                        Some(abort) => self.finish_run_abort(abort),
                        None => self.finish(outcome),
                    }
                    break;
                }
            }
        }
        if progress_dirty {
            self.emit_progress();
        }
    }

    /// Feed a response to a previously emitted effect.
    pub fn handle_response(&mut self, response: Response<M::IntentOutcome>) {
        if let Err(overflow) = self.try_handle_response(response) {
            self.fail_turn(make_error_event(
                crate::session_model::TurnFailureKind::TokenUsageAccounting,
                Some(crate::session_model::TurnFailureCode::TokenUsageOverflow.into()),
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
    pub fn try_handle_response(
        &mut self,
        response: Response<M::IntentOutcome>,
    ) -> Result<(), TokenUsageOverflow> {
        let Some(answered) = self.state.take_waiting(response) else {
            return Ok(());
        };
        match answered {
            AnsweredWork::ExecutionEnvironmentSynced { result } => {
                self.handle_execution_environment_synced(result)
            }
            AnsweredWork::Llm {
                id,
                request,
                driver_state,
                result,
                text_streamed,
            } => self.handle_llm_complete(id, request, driver_state, result, text_streamed)?,
            AnsweredWork::Tools { expansion, results } => {
                self.handle_tool_results(&expansion, results);
            }
            AnsweredWork::Exec {
                driver_state,
                result,
            } => self.handle_exec_result(driver_state, result),
            AnsweredWork::Checkpoint {
                checkpoint,
                on_empty,
                delivery,
            } => self.handle_checkpoint(checkpoint, on_empty, delivery),
        }
        Ok(())
    }

    fn handle_execution_environment_synced(
        &mut self,
        result: Result<ExecutionEnvironmentSync, ExecutionEnvironmentSyncFailure>,
    ) {
        match result {
            Ok(sync) => {
                self.environment = Some(SyncedEnvironment {
                    protocol_iteration: self.protocol_iteration,
                    sync,
                });
                self.state = MachineState::PrepareIteration;
            }
            Err(failure) => {
                self.fail_turn(make_error_event(
                    crate::session_model::TurnFailureKind::ExecutionEnvironment,
                    Some(failure.code),
                    format!(
                        "Failed to refresh execution environment: {}",
                        failure.message
                    ),
                    Some(failure.message),
                ));
            }
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
            self.prompt_messages.push(cause.to_event_message());
            self.messages.push(cause.to_event_message());
            self.turn_causes.push(cause);
        }
    }

    fn handle_checkpoint(
        &mut self,
        checkpoint: CheckpointKind,
        on_empty: CheckpointResumeAction,
        delivery: CheckpointDelivery,
    ) {
        if !delivery.committed_user_messages.is_empty() || !delivery.turn_causes.is_empty() {
            self.prompt_messages
                .extend(delivery.committed_user_messages.clone());
            self.messages.extend(delivery.committed_user_messages);
            self.append_turn_causes(delivery.turn_causes);
            if matches!(checkpoint, CheckpointKind::BeforeCompletion) {
                self.protocol_iteration += 1;
                if self
                    .config
                    .turn_budget
                    .max_turns()
                    .is_some_and(|max_turns| {
                        self.protocol_iteration
                            .saturating_sub(self.protocol_run_offset)
                            >= max_turns
                    })
                {
                    self.emit_progress();
                    self.finish(TurnOutcome::Stopped(TurnStop::MaxTurns));
                    return;
                }
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

    fn handle_llm_complete(
        &mut self,
        id: EffectId,
        request: Arc<LlmRequest>,
        driver_state: Option<M::DriverState>,
        result: Result<LlmResponse, LlmCallError>,
        text_streamed: bool,
    ) -> Result<(), TokenUsageOverflow> {
        match result {
            Err(error) if error.terminal_reason == LlmTerminalReason::Cancelled => {
                self.finish(TurnOutcome::Stopped(TurnStop::Cancelled {
                    evidence: self.cancellation_evidence(),
                }));
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
                // reason executes the finish decision below.
                refine_terminal_reason_for_context_window(
                    &mut llm_response,
                    prompt_input_tokens,
                    Some(self.config.model.context_window_tokens()),
                );
                let response_text = llm_response.full_text();
                self.record_llm_usage(&llm_response, usage, &response_text)?;
                if self.handle_terminal_llm_response(&llm_response, text_streamed) {
                    return Ok(());
                }
                let calls = self
                    .config
                    .model_tool_calls
                    .response(self.protocol_iteration, id);
                self.shift(|driver, ctx| {
                    driver.handle_llm_success(
                        ctx,
                        request,
                        driver_state,
                        llm_response,
                        &calls,
                        text_streamed,
                    )
                });
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
            LlmTerminalReason::ContextOverflow => TurnOutcome::Stopped(TurnStop::ContextOverflow),
            LlmTerminalReason::ContentFilter => TurnOutcome::Stopped(TurnStop::ProviderError),
            LlmTerminalReason::ProviderError => TurnOutcome::Stopped(TurnStop::ProviderError),
            LlmTerminalReason::Cancelled => TurnOutcome::Stopped(TurnStop::Cancelled {
                evidence: self.cancellation_evidence(),
            }),
            LlmTerminalReason::Stop | LlmTerminalReason::ToolUse | LlmTerminalReason::Unknown => {
                return false;
            }
        };

        let response_text = llm_response.full_text();
        let visible_text = self
            .config
            .protocol_driver
            .project_visible_assistant_prose(&response_text);
        if !text_streamed && !visible_text.is_empty() {
            // Terminal-finish fallback: the projected remainder arrives as one
            // block — still a full Started/Delta/Completed lifecycle so hosts
            // never see an unpaired delta.
            let block = crate::llm::types::StreamBlockIdentity::new(
                format!("completed:{}:text", self.protocol_iteration),
                0,
            );
            self.emit(SessionStreamEvent::StreamBlockStarted {
                kind: crate::llm::types::StreamBlockKind::AssistantText,
                block: block.clone(),
            });
            self.emit(SessionStreamEvent::TextDelta {
                content: visible_text.clone(),
                block: block.clone(),
            });
            self.emit(SessionStreamEvent::StreamBlockCompleted {
                kind: crate::llm::types::StreamBlockKind::AssistantText,
                block,
                content: visible_text.clone(),
            });
        }
        self.emit(SessionStreamEvent::LlmResponse {
            protocol_iteration: self.protocol_iteration,
            content: visible_text,
        });
        let reason = llm_response.terminal_reason;
        let diagnostic = llm_response
            .terminal_diagnostic
            .clone()
            .unwrap_or_else(|| format!("Model call ended with terminal reason {reason:?}."));
        let mut envelope = crate::session_model::make_error_envelope(
            crate::session_model::TurnFailureKind::LlmProvider,
            Some(reason.into()),
            Some(reason),
            "provider call ended",
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
            self.side_effect_outbox.push_back(Effect::Log {
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
            self.side_effect_outbox.push_back(Effect::Log {
                event: LogEvent::LlmError {
                    session_id: self.config.session_id.clone(),
                    protocol_iteration: self.protocol_iteration,
                    request_body: error.request_body.clone(),
                    retryable: error.retryable,
                    code: error.code.clone(),
                    kind: error.kind,
                    terminal_reason: error.terminal_reason,
                },
            });
        }
    }

    fn emit_llm_error(&mut self, error: LlmCallError) {
        self.record_llm_error(&error);
        let mut envelope = crate::session_model::make_error_envelope(
            crate::session_model::TurnFailureKind::LlmProvider,
            error.code.clone(),
            Some(error.terminal_reason),
            "provider call failed",
            None,
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
        // A failed call whose terminal reason is a context-window overflow stops
        // as the overflow, not as an undifferentiated provider error: the two
        // classifier entry points (`is_context_overflow_text` and the OpenAI
        // `context_length_exceeded` code) both arrive here on the error path,
        // and a host that can recover from an overflow must be able to tell it
        // apart here exactly as it can on the Ok path. Every other terminal
        // reason keeps stopping as `ProviderError`.
        self.finish(TurnOutcome::Stopped(match error.terminal_reason {
            LlmTerminalReason::ContextOverflow => TurnStop::ContextOverflow,
            _ => TurnStop::ProviderError,
        }));
    }

    fn handle_tool_results(
        &mut self,
        expansion: &ToolExpansionPlan,
        completed: Vec<CompletedToolCall<M::IntentOutcome>>,
    ) {
        let completed = if expansion.is_empty() {
            completed
        } else {
            Arc::clone(&self.config.protocol_driver).fold_tool_results(expansion, completed)
        };
        for outcome in &completed {
            self.emit(SessionStreamEvent::ToolCall {
                call_id: outcome.call_id.clone(),
                provider_call_id: outcome.provider_call_id.clone(),
                name: outcome.tool_name.clone(),
                args: outcome.args.clone(),
                output: outcome.output.clone(),
            });
        }

        self.run_abort = RunAbort::first_in(completed.iter().map(|outcome| &outcome.output));
        self.shift(|driver, ctx| driver.handle_tool_results(ctx, completed));
        self.finish_pending_run_abort();
    }

    /// Finish for a Run abort the driver's actions did not reach.
    fn finish_pending_run_abort(&mut self) {
        if let Some(abort) = self.run_abort.take()
            && !matches!(self.state, MachineState::Finished)
        {
            self.finish_run_abort(abort);
        }
    }

    fn handle_exec_result(
        &mut self,
        driver_state: M::DriverState,
        result: Result<crate::ExecResponse, crate::ExecCodeFailure>,
    ) {
        self.run_abort = result.as_ref().ok().and_then(|response| {
            RunAbort::first_in(
                response
                    .calls
                    .iter()
                    .filter_map(|call| call.host_record.as_ref())
                    .map(|record| &record.output),
            )
        });
        self.shift(|driver, ctx| driver.handle_exec_result(ctx, driver_state, result));
        self.finish_pending_run_abort();
    }
}
