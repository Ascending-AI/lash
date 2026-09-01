use super::*;

impl LashCoreBuilder {
    /// Resolve the runtime host config, requiring the generic host-owned
    /// durability dependencies to have been named.
    pub(super) fn resolve_runtime_host_config(&mut self) -> Result<RuntimeHostConfig> {
        if let Some(base) = self.runtime_host_config.take() {
            self.reject_runtime_host_config_conflicts()?;
            return Ok(base);
        }
        let effect_host = self
            .effect_host
            .take()
            .ok_or(EmbedError::MissingEffectHost)?;
        let attachment_store = self
            .attachment_store
            .take()
            .ok_or(EmbedError::MissingAttachmentStore)?;
        let process_env_store = self
            .process_env_store
            .take()
            .ok_or(EmbedError::MissingProcessEnvStore)?;
        let commit_budget = self
            .commit_budget
            .take()
            .ok_or(EmbedError::MissingCommitBudget)?;
        let queued_work_batching = self
            .queued_work_batching
            .take()
            .ok_or(EmbedError::MissingQueuedWorkBatching)?;
        let core = RuntimeHostConfig::new(
            effect_host,
            attachment_store,
            process_env_store,
            commit_budget,
            queued_work_batching,
        );
        Ok(self.apply_core_overrides(core))
    }

    fn reject_runtime_host_config_conflicts(&self) -> Result<()> {
        for (configured, field) in [
            (self.effect_host.is_some(), "effect_host"),
            (self.attachment_store.is_some(), "attachment_store"),
            (self.process_env_store.is_some(), "process_env_store"),
            (self.commit_budget.is_some(), "commit_budget"),
            (self.max_attachment_bytes.is_some(), "max_attachment_bytes"),
            (self.queued_work_batching.is_some(), "queued_work_batching"),
            (
                self.process_wake_delivery_policy.is_some(),
                "process_wake_delivery_policy",
            ),
            (self.prompt.is_some(), "prompt"),
            (self.trace_sink.is_some(), "trace_sink"),
            (self.trace_level.is_some(), "trace_level"),
            (self.trace_context.is_some(), "trace_context"),
            (self.termination.is_some(), "termination"),
            (self.lease_timings.is_some(), "lease_timings"),
            (self.clock.is_some(), "clock"),
            (
                self.process_tool_visibility_filter.is_some(),
                "process_tool_visibility_filter",
            ),
        ] {
            if configured {
                return Err(EmbedError::RuntimeHostConfigConflict { field });
            }
        }
        Ok(())
    }

    /// Apply benign + still-set dependency overrides on top of a base core.
    fn apply_core_overrides(&mut self, mut core: RuntimeHostConfig) -> RuntimeHostConfig {
        if let Some(effect_host) = self.effect_host.take() {
            core.control.effect_host = effect_host;
        }
        if let Some(attachment_store) = self.attachment_store.take() {
            core.durability.attachment_store = Arc::new(
                facade_support::SessionAttachmentStore::ephemeral(attachment_store)
                    .with_max_attachment_bytes(
                        core.durability.attachment_store.max_attachment_bytes(),
                    ),
            );
        }
        if let Some(process_env_store) = self.process_env_store.take() {
            core.durability.process_env_store = process_env_store;
        }
        if let Some(commit_budget) = self.commit_budget.take() {
            core.durability.commit_budget = commit_budget;
        }
        if let Some(max_attachment_bytes) = self.max_attachment_bytes.take() {
            core = core.with_max_attachment_bytes(max_attachment_bytes);
        }
        if let Some(queued_work_batching) = self.queued_work_batching.take() {
            core.durability.queued_work_batching = queued_work_batching;
        }
        if let Some(process_wake_delivery_policy) = self.process_wake_delivery_policy.take() {
            core.control.process_wake_delivery_policy = process_wake_delivery_policy;
        }
        if let Some(prompt) = self.prompt.take() {
            core.prompt.prompt = prompt;
        }
        if let Some(trace_sink) = self.trace_sink.take() {
            core.tracing.trace_sink = Some(trace_sink);
        }
        if let Some(trace_level) = self.trace_level.take() {
            core.tracing.trace_level = trace_level;
        }
        if let Some(trace_context) = self.trace_context.take() {
            core.tracing.trace_context = trace_context;
        }
        if let Some(termination) = self.termination.take() {
            core.control.termination = termination;
        }
        if let Some(lease_timings) = self.lease_timings.take() {
            core.control.lease_timings = lease_timings;
        }
        if let Some(clock) = self.clock.take() {
            core.clock = clock;
        }
        if let Some(filter) = self.process_tool_visibility_filter.take() {
            core.control.process_tool_visibility_filter = Some(filter);
        }
        core
    }
}
