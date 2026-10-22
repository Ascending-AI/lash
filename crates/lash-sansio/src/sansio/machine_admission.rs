//! Source preparation at the machine admission boundary.
use super::*;

impl<M: TurnProtocol> TurnMachine<M> {
    /// Deliver an immutable effect source whose actual Arc retains its
    /// scalar allocation after the machine or its waiting state is dropped.
    /// Drain the next pending effect. Returns `None` when the host must call
    /// `handle_response()` before more effects become available.
    pub fn poll_effect(
        &mut self,
    ) -> Result<
        Option<super::super::RetainedEffect<M>>,
        lash_protocol_types::allocation::ProjectionError,
    > {
        if let Some(error) = self.projection_refusal.take() {
            return Err(error);
        }
        if self.side_effect_outbox.is_empty() {
            match self.state {
                MachineState::PreparingProtocol => self.prepare_protocol(),
                MachineState::PrepareIteration => self.prepare_protocol_iteration(),
                _ => {}
            }
        }
        if let Some(error) = self.projection_refusal.take() {
            return Err(error);
        }
        if let Some(effect) = self.side_effect_outbox.pop_front() {
            return Ok(Some(effect));
        }
        let MachineState::Waiting {
            work,
            delivery: EffectDeliveryStatus::Pending,
            ..
        } = &self.state
        else {
            return Ok(None);
        };
        let capacity = lash_protocol_types::allocation::payload_capacity(work, 256);
        let budget = self.config.allocation_budget.clone();
        let allocation = budget.reserve(capacity.saturating_mul(2))?;
        Ok(self.state.poll_outstanding_effect().map(|effect| {
            Arc::new(lash_protocol_types::allocation::Allocated::new(
                effect, allocation, budget,
            ))
        }))
    }

    pub(super) fn enqueue(&mut self, effect: Effect<M>) {
        let budget = &self.config.allocation_budget;
        if self.side_effect_outbox.len() == self.side_effect_outbox.capacity() {
            let capacity = self.side_effect_outbox.capacity().saturating_mul(2).max(4);
            let directory = match budget.reserve(
                (capacity as u64)
                    .saturating_mul(std::mem::size_of::<RetainedEffect<M>>() as u64)
                    .saturating_add(64),
            ) {
                Ok(hold) => hold,
                Err(error) => {
                    self.projection_refusal = Some(error.into());
                    return;
                }
            };
            let mut outbox = VecDeque::with_capacity(capacity);
            outbox.append(&mut self.side_effect_outbox);
            self.side_effect_outbox = outbox;
            self.retained.outbox = directory;
        }
        let allocation = match budget.reserve(
            lash_protocol_types::allocation::payload_capacity(&effect, 1024).saturating_mul(2),
        ) {
            Ok(hold) => hold,
            Err(error) => {
                self.projection_refusal = Some(error.into());
                return;
            }
        };
        match Self::adopt_effect(effect, budget) {
            Ok(effect) => self.side_effect_outbox.push_back(Arc::new(
                lash_protocol_types::allocation::Allocated::new(effect, allocation, budget.clone()),
            )),
            Err(error) => self.projection_refusal = Some(error.into()),
        }
    }

    fn adopt_effect(
        mut effect: Effect<M>,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<Effect<M>, lash_protocol_types::allocation::AllocationError> {
        match &mut effect {
            Effect::LlmCall { request, .. } => *request = Arc::new(request.adopt_in(budget)?),
            Effect::Progress {
                messages,
                event_delta,
                ..
            }
            | Effect::Done {
                messages,
                event_delta,
                ..
            } => {
                *messages = messages.clone_in(budget)?;
                for event in event_delta {
                    *event = Self::adopt_history_record(event, budget)?.0;
                }
            }
            Effect::ReportToolCalls { completed, .. } => {
                for completed in completed {
                    if let Some(lash_protocol_types::ToolControl::Turn { control }) =
                        &mut completed.output.control
                    {
                        Self::adopt_control(control, budget)?;
                    }
                }
            }
            Effect::Emit(SessionStreamEvent::ToolCall { output, .. }) => {
                if let Some(lash_protocol_types::ToolControl::Turn { control }) =
                    &mut output.control
                {
                    Self::adopt_control(control, budget)?;
                }
            }
            _ => {}
        }
        Ok(effect)
    }

    pub(super) fn adopt_work(
        work: &PendingWork<M>,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<PendingWork<M>, lash_protocol_types::allocation::AllocationError> {
        let mut work = work.clone();
        match &mut work {
            PendingWork::Llm { request, .. } => *request = Arc::new(request.adopt_in(budget)?),
            PendingWork::Checkpoint {
                on_empty:
                    CheckpointResumeAction::Complete {
                        candidate,
                        superseded,
                    },
                ..
            } => {
                Self::adopt_control(&mut candidate.control, budget)?;
                for message in superseded {
                    message.parts =
                        lash_protocol_types::MessageParts::adopt_in(&message.parts, budget)?;
                }
            }
            PendingWork::WaitingForToolResults { wave, .. } => {
                if let ToolWave::Control { before, after, .. } = wave.as_mut() {
                    for completed in before.iter_mut().chain(after) {
                        if let Some(lash_protocol_types::ToolControl::Turn { control }) =
                            &mut completed.output.control
                        {
                            Self::adopt_control(control, budget)?;
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(work)
    }

    fn adopt_control(
        control: &mut lash_protocol_types::TurnControl,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<(), lash_protocol_types::allocation::AllocationError> {
        match control {
            lash_protocol_types::TurnControl::Finish {
                value_schema: Some(schema),
                ..
            } => *schema = schema.adopt_in(budget)?,
            lash_protocol_types::TurnControl::SwitchAgentFrame { initial_nodes, .. } => {
                for node in initial_nodes {
                    match node {
                        lash_protocol_types::SessionAppendNode::Message { .. } => {}
                        lash_protocol_types::SessionAppendNode::ProtocolEvent { event } => {
                            event.payload = event.payload.adopt_in(budget)?
                        }
                        lash_protocol_types::SessionAppendNode::Plugin { .. } => {}
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn decide_completion(
        &mut self,
        mut candidate: lash_protocol_types::CompletionCandidate,
        disposition: lash_protocol_types::CompletionDisposition,
    ) -> bool {
        let capacity = lash_protocol_types::allocation::payload_capacity(
            &(&self.decided_completions, &candidate),
            1024,
        )
        .saturating_mul(2);
        let allocation = match self.config.allocation_budget.reserve(capacity) {
            Ok(hold) => hold,
            Err(error) => {
                self.projection_refusal = Some(error.into());
                return false;
            }
        };
        if let Err(error) =
            Self::adopt_control(&mut candidate.control, &self.config.allocation_budget)
        {
            self.projection_refusal = Some(error.into());
            return false;
        }
        self.decided_completions
            .push(lash_protocol_types::DecidedCompletion {
                candidate,
                disposition,
            });
        self.retained.completions = allocation;
        true
    }

    pub(super) fn admit_environment(
        sync: &ExecutionEnvironmentSync,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<ExecutionEnvironmentSync, lash_protocol_types::allocation::AllocationError> {
        let tool_specs = lash_protocol_types::llm::types::LlmToolSpecs::adopt_tools_in(
            &sync.tool_specs,
            budget,
        )?;
        let mut turn_controls = std::collections::BTreeMap::new();
        for (name, controls) in &sync.turn_controls {
            let mut adopted = controls.clone();
            if let Some(finish) = controls.finish_declaration() {
                adopted = adopted.with_finish(finish.value_schema.adopt_in(budget)?);
            }
            turn_controls.insert(name.clone(), adopted);
        }
        Ok(ExecutionEnvironmentSync {
            tool_specs,
            turn_controls,
            truncated_names: sync.truncated_names.clone(),
        })
    }

    pub fn set_allocation_budget(
        &mut self,
        budget: lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<(), lash_protocol_types::allocation::AllocationError> {
        let owned = (
            &self.state,
            &self.side_effect_outbox,
            &self.resume_work,
            &self.observed_cancellation,
            &self.environment,
            &self.decided_completions,
            &self.progress_boundaries,
            (
                &self.config.model,
                &self.config.generation,
                &self.config.session_id,
                &self.config.agent_frame_id,
                &self.config.turn_id,
            ),
        );
        let adoption = budget.reserve(
            lash_protocol_types::allocation::payload_capacity(&owned, 1024).saturating_mul(2),
        )?;
        let same = lash_protocol_types::AppendVec::ptr_eq(
            &self.messages.shared(),
            &self.prompt_messages.shared(),
        );
        let messages = self.messages.clone_in(&budget)?;
        let prompt = if same {
            messages.clone()
        } else {
            self.prompt_messages.clone_in(&budget)?
        };
        let events = if self.events.allocation_budget().identity() == budget.identity()
            && self.events.sources_admitted()
        {
            self.events.clone()
        } else {
            let mut events =
                lash_protocol_types::AppendVec::try_with_capacity(self.events.len(), &budget)?;
            for event in self.events.iter() {
                let (event, _copy) = Self::adopt_history_record(event, &budget)?;
                events.push(event)?;
            }
            events
        };
        let environment = self
            .environment
            .as_ref()
            .map(|environment| {
                Ok::<_, lash_protocol_types::allocation::AllocationError>(SyncedEnvironment {
                    protocol_iteration: environment.protocol_iteration,
                    sync: Self::admit_environment(&environment.sync, &budget)?,
                })
            })
            .transpose()?;
        let attachment_acceptance = lash_protocol_types::allocation::Allocated::adopt_in(
            &self.config.attachment_acceptance,
            &budget,
        )?;
        let content = self.content.adopt_in(&budget)?;
        let mut outbox = VecDeque::with_capacity(self.side_effect_outbox.len());
        for effect in &self.side_effect_outbox {
            let effect = Self::adopt_effect(effect.as_ref().as_ref().clone(), &budget)?;
            outbox.push_back(Arc::new(lash_protocol_types::allocation::Allocated::new(
                effect,
                adoption.clone(),
                budget.clone(),
            )));
        }
        let state = match &self.state {
            MachineState::Waiting {
                effect_id,
                work,
                delivery,
            } => MachineState::Waiting {
                effect_id: *effect_id,
                work: Self::adopt_work(work, &budget)?,
                delivery: *delivery,
            },
            state => state.clone(),
        };
        let resume = self
            .resume_work
            .as_ref()
            .map(|work| Self::adopt_work(work, &budget))
            .transpose()?;
        let mut completions = self.decided_completions.clone();
        for completion in &mut completions {
            Self::adopt_control(&mut completion.candidate.control, &budget)?;
        }
        self.side_effect_outbox = outbox;
        self.state = state;
        self.resume_work = resume;
        self.decided_completions = completions;
        self.messages = messages;
        self.prompt_messages = prompt;
        self.events = events;
        self.environment = environment;
        self.content = content;
        self.retained.adoption = adoption;
        self.config.attachment_acceptance = attachment_acceptance;
        self.config.allocation_budget = budget;
        Ok(())
    }

    /// Prepare the actual parts/payload carrier and scalar copy before the
    /// history buffer claims it. The temporary obligation lives until that
    /// buffer has admitted its own copy; escaped sources own their own holds.
    /// A conversation also prepares its two message projections before their
    /// buffers claim them, so admit all three scalar copies up front.
    pub(super) fn adopt_history_record(
        event: &SessionHistoryRecord<M::Event>,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<
        (
            SessionHistoryRecord<M::Event>,
            lash_protocol_types::allocation::AllocationHold,
        ),
        lash_protocol_types::allocation::AllocationError,
    > {
        use lash_protocol_types::allocation::SourceCapacity;
        let copy = budget.reserve(
            event
                .owned_capacity()
                .saturating_add(std::mem::size_of_val(event) as u64)
                .saturating_mul(3),
        )?;
        let event = match event {
            SessionHistoryRecord::Conversation(record) => {
                let parts = lash_protocol_types::MessageParts::adopt_in(&record.parts, budget)?;
                let mut record = record.clone();
                record.parts = parts;
                SessionHistoryRecord::Conversation(record)
            }
            SessionHistoryRecord::Protocol(event) => {
                SessionHistoryRecord::Protocol(M::adopt_event(event, budget)?)
            }
        };
        Ok((event, copy))
    }
    /// Start this machine at `work` instead of at the driver's first step
    /// (FIG-4739): the turn continues a run whose earlier turn ended at a
    /// segment boundary while it waited on `work`, and issues it again. The
    /// machine syncs its environment first, as every turn does, and then
    /// waits on `work` where the driver would have prepared an iteration.
    pub fn resume_with(&mut self, work: PendingWork<M>) {
        let hold = match self.config.allocation_budget.reserve(
            lash_protocol_types::allocation::payload_capacity(&work, 1024).saturating_mul(2),
        ) {
            Ok(hold) => hold,
            Err(error) => {
                self.projection_refusal = Some(error.into());
                return;
            }
        };
        let work = match Self::adopt_work(&work, &self.config.allocation_budget) {
            Ok(work) => work,
            Err(error) => {
                self.projection_refusal = Some(error.into());
                return;
            }
        };
        self.resume_work = Some(work);
        self.retained.resume = hold;
    }
    /// Record the cancellation request the host has observed for this turn.
    /// The first observation wins; later ones are ignored.
    pub fn record_cancellation_evidence(
        &mut self,
        evidence: lash_protocol_types::TurnCancellationEvidence,
    ) {
        if self.observed_cancellation.is_none() {
            let hold = match self.config.allocation_budget.reserve(
                lash_protocol_types::allocation::payload_capacity(&evidence, 128),
            ) {
                Ok(hold) => hold,
                Err(error) => {
                    self.projection_refusal = Some(error.into());
                    return;
                }
            };
            self.observed_cancellation = Some(evidence);
            self.retained.cancellation = hold;
        }
    }
}
