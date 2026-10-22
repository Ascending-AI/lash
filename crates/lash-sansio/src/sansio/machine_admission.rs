//! Source preparation at the machine admission boundary.
use super::*;

impl<M: TurnProtocol> TurnMachine<M> {
    pub(super) fn admit_growth(&mut self, value: &impl serde::Serialize) -> bool {
        match self.config.allocation_budget.reserve(
            lash_protocol_types::allocation::payload_capacity(value, 1024).saturating_mul(4),
        ) {
            Ok(hold) => {
                self.retained.growth.push(hold);
                true
            }
            Err(error) => {
                self.projection_refusal = Some(error.into());
                false
            }
        }
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
        let content = self.content.adopt_in(&budget)?;
        self.messages = messages;
        self.prompt_messages = prompt;
        self.events = events;
        self.environment = environment;
        self.content = content;
        self.retained.adoption = adoption;
        self.config.allocation_budget = budget;
        Ok(())
    }

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
}
