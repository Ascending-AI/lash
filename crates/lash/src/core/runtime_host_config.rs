use super::*;

impl LashCoreBuilder {
    /// Assemble the runtime host config over the backend, which supplies its
    /// effect host, attachment store, process-env store and clock, then apply
    /// every runtime setting this builder carries over it.
    pub(super) fn resolve_runtime_host_config(&mut self) -> Result<RuntimeHostConfig> {
        let commit_budget = self
            .commit_budget
            .take()
            .ok_or(EmbedError::MissingCommitBudget)?;
        let queued_work_batching = self
            .queued_work_batching
            .take()
            .ok_or(EmbedError::MissingQueuedWorkBatching)?;
        let core =
            RuntimeHostConfig::new(self.backend.clone(), commit_budget, queued_work_batching);
        Ok(self.apply_core_overrides(core))
    }

    fn apply_core_overrides(&mut self, mut core: RuntimeHostConfig) -> RuntimeHostConfig {
        if let Some(max) = self.max_attachment_bytes.take() {
            core = core.with_max_attachment_bytes(max);
        }
        if let Some(policy) = self.process_wake_delivery_policy.take() {
            core.control.process_wake_delivery_policy = policy;
        }
        if let Some(prompt) = self.prompt.take() {
            core.prompt.prompt = prompt;
        }
        if let Some(sink) = self.trace_sink.take() {
            core.tracing.trace_sink = Some(sink);
        }
        if let Some(level) = self.trace_level.take() {
            core.tracing.trace_level = level;
        }
        if let Some(context) = self.trace_context.take() {
            core.tracing.trace_context = context;
        }
        if let Some(termination) = self.termination.take() {
            core.control.termination = termination;
        }
        if let Some(policy) = self.tool_source_policy.take() {
            core.control.tool_source_policy = policy;
        }
        if let Some(grace) = self.abort_drain_grace.take() {
            core.control.abort_drain_grace = grace;
        }
        if let Some(timings) = self.lease_timings.take() {
            core.control.lease_timings = timings;
        }
        if let Some(provider) = self.provider.clone() {
            core.providers.provider_resolver =
                Arc::new(facade_support::SingleProviderResolver::new(provider));
        }
        if let Some(filter) = self.process_tool_visibility_filter.take() {
            core.control.process_tool_visibility_filter = Some(filter);
        }
        core
    }
}
