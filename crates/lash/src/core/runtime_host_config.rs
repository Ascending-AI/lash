use super::*;

impl LashCoreBuilder {
    /// Resolve the runtime host config, requiring the generic host-owned
    /// durability dependencies to have been named.
    pub(super) fn resolve_runtime_host_config(&mut self) -> Result<RuntimeHostConfig> {
        if let Some(base) = self.runtime_host_config.take() {
            return Ok(self.apply_core_overrides(base));
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
        let core = RuntimeHostConfig::new(
            effect_host,
            attachment_store,
            process_env_store,
            commit_budget,
        );
        Ok(self.apply_core_overrides(core))
    }

    /// Apply benign + still-set dependency overrides on top of a base core.
    fn apply_core_overrides(&mut self, mut core: RuntimeHostConfig) -> RuntimeHostConfig {
        if let Some(effect_host) = self.effect_host.take() {
            core.control.effect_host = effect_host;
        }
        if let Some(attachment_store) = self.attachment_store.take() {
            core.durability.attachment_store = Arc::new(
                facade_support::SessionAttachmentStore::ephemeral(attachment_store),
            );
        }
        if let Some(process_env_store) = self.process_env_store.take() {
            core.durability.process_env_store = process_env_store;
        }
        if let Some(commit_budget) = self.commit_budget.take() {
            core.durability.commit_budget = commit_budget;
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
        if let Some(wake_turn_policy) = self.wake_turn_policy.take() {
            core.control.wake_turn_policy = wake_turn_policy;
        }
        if let Some(filter) = self.process_tool_visibility_filter.take() {
            core.control.process_tool_visibility_filter = Some(filter);
        }
        core
    }
}
