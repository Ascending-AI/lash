//! Preadmitted immutable checkpoint sources.
use super::*;

impl<M: TurnProtocol> TurnMachine<M> {
    /// The checkpoint over `base`, the chains a committed checkpoint stored,
    /// whose sequences are unchanged through `floors`; its content every
    /// blob it did not keep from `base`, the chains it is stored as and how
    /// many items it encoded.
    pub(in crate::sansio) fn record(
        &self,
        base: Option<&StoredChains>,
        floors: Floors,
    ) -> Result<
        (
            TurnCheckpoint<M>,
            TurnCheckpointContent,
            StoredChains,
            usize,
        ),
        lash_protocol_types::allocation::AllocationError,
    > {
        // Constant-space borrowed preflight precedes encoding, cloning or
        // allocating any retained checkpoint source. Eight representations
        // cover encodings, chain tails, sparse blob directories and scratch.
        let source = (
            &self.state,
            &self.side_effect_outbox,
            self.messages.as_slice(),
            self.prompt_messages.as_slice(),
            self.events.as_slice(),
            &self.decided_completions,
            &self.environment,
            &self.progress_boundaries,
        );
        let capacity = lash_protocol_types::allocation::payload_capacity(
            &source,
            std::mem::size_of::<CheckpointData<M>>() + 4096,
        )
        .saturating_mul(8);
        let allocation = self.config.allocation_budget.reserve(capacity)?;
        let mut content = TurnCheckpointContent::admitted(
            allocation.clone(),
            self.config.allocation_budget.clone(),
        );
        let mut encoded = 0;
        let window = self.window.as_ref();
        let window_events = window.map_or(0, |window| window.events().len());
        let (state, request) = CheckpointState::record(
            &self.state,
            &mut content,
            window,
            base.and_then(|base| base.request.as_ref()),
            &mut encoded,
        )?;
        let (messages, stored_messages) = CheckpointMessages::record(
            &self.messages,
            window,
            base.map(|base| &base.messages),
            floors.messages,
            &mut content,
            &mut encoded,
        )?;
        let (prompt_messages, stored_prompt) = CheckpointPrompt::record(
            &self.prompt_messages,
            &self.messages,
            window,
            base.map(|base| &base.prompt),
            floors.prompt,
            &mut content,
            &mut encoded,
        )?;
        // The machine only appends to its history, so the window's records
        // lead it.
        let turn_events = self
            .events
            .as_slice()
            .get(window_events..)
            .unwrap_or_default();
        let events = Chain::of(
            turn_events.iter(),
            turn_events.len(),
            base.map(|base| &base.events),
            floors.events.saturating_sub(window_events),
            &mut content,
            &mut encoded,
        )?;
        let completions = Chain::of(
            self.decided_completions.iter(),
            self.decided_completions.len(),
            base.map(|base| &base.completions),
            usize::MAX,
            &mut content,
            &mut encoded,
        )?;
        let checkpoint = TurnCheckpoint(Arc::new(CheckpointData {
            allocation,
            schema_version: TURN_CHECKPOINT_SCHEMA_VERSION,
            state,
            pending_effects: self
                .side_effect_outbox
                .iter()
                .map(|effect| effect.as_ref().as_ref().clone())
                .map(|effect| match effect {
                    Effect::Emit(SessionStreamEvent::Error(ReportedFailure {
                        message: _,
                        envelope: Some(mut envelope),
                    })) if matches!(
                        envelope.kind,
                        lash_protocol_types::session_model::TurnFailureKind::LlmProvider
                    ) =>
                    {
                        envelope.raw = None;
                        envelope.user_message = "provider call failed".to_string();
                        Effect::Emit(SessionStreamEvent::Error(ReportedFailure {
                            message: envelope.user_message.clone(),
                            envelope: Some(envelope),
                        }))
                    }
                    effect => effect,
                })
                .collect(),
            next_effect_id: self.next_effect_id,
            window: window.map(CheckpointWindow::of),
            messages,
            prompt_messages,
            events: events.head().clone(),
            progress_event_cursor: self.progress_event_cursor,
            progress_boundaries: self.progress_boundaries.clone(),
            protocol_iteration: self.protocol_iteration,
            protocol_run_offset: self.protocol_run_offset,
            cumulative_usage: self.cumulative_usage.clone(),
            last_call_usage: self.last_call_usage.clone(),
            environment: self.environment.clone(),
            decided_completions: completions.head().clone(),
        }));
        let chains = StoredChains {
            messages: stored_messages,
            prompt: stored_prompt,
            events,
            completions,
            // A turn between model calls keeps its last request stored: the
            // next one builds on it.
            request: request.or_else(|| base.and_then(|base| base.request.clone())),
        };
        Ok((checkpoint, content, chains, encoded))
    }
}
