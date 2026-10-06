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
        let mut core =
            RuntimeHostConfig::new(self.backend.clone(), commit_budget, queued_work_batching);
        // The backend's process registry owns the lifetime scopes the shift
        // closes: a run's end and a session's close write its scope-close
        // rows (FIG-3607 item 7) and, over the backend's process port, apply
        // the plan each row records (FIG-3822).
        core.control.scope_close = Arc::new(
            lash_core::RegistryScopeClose::with_delivery(
                self.backend.process_registry(),
                Arc::new(lash_core::DurableProcessWork::new(self.backend.clone())),
                self.backend.clock(),
            )
            .with_effect_host(lash_core::ActorContext::detached(self.backend.clone()))
            .with_session_store_factory(self.backend.session_store_factory()),
        );
        Ok(self.apply_core_overrides(core))
    }

    fn apply_core_overrides(&mut self, mut core: RuntimeHostConfig) -> RuntimeHostConfig {
        if let Some(max) = self.max_attachment_bytes.take() {
            core = core.with_max_attachment_bytes(max);
        }
        if let Some(policy) = self.attachment_read_policy.take() {
            core = core.with_attachment_read_policy(policy);
        }
        if let Some(expiry) = self.attachment_upload_expiry.take() {
            core = core.with_attachment_upload_expiry_ms(
                u64::try_from(expiry.as_millis()).unwrap_or(u64::MAX),
            );
        }
        if let Some(policy) = self.output_retention.take() {
            core = core.with_output_retention(policy);
        }
        if let Some(policy) = self.process_wake_delivery_policy.take() {
            core.control.process_wake_delivery_policy = policy;
        }
        if let Some(runtime) = self.trace_runtime.take() {
            core.tracing = runtime;
        }
        #[cfg(feature = "otel-trace")]
        if let Some(telemetry) = self.telemetry.take() {
            let metrics = telemetry.metrics().clone();
            let adapter = Arc::new(telemetry);
            core.tracing = core
                .tracing
                .with_scopes(adapter.clone())
                .with_projector(adapter)
                .with_metrics(metrics);
        }
        if let Some(sink) = self.trace_sink.take() {
            core.tracing = core.tracing.with_trace_sink(sink);
        }
        if let Some(level) = self.trace_level.take() {
            core.tracing = core.tracing.clone().with_level(level);
        }
        if let Some(context) = self.trace_context.take() {
            core.tracing = core.tracing.clone().with_base_context(context);
        }
        core.tracing = core
            .tracing
            .clone()
            .with_tool_receipts(core.backend().stores());
        if let Some(termination) = self.termination.take() {
            core.control.termination = termination;
        }
        if let Some(policy) = self.tool_source_policy.take() {
            core.control.tool_source_policy = policy;
        }
        if let Some(budgets) = self.execution_budgets.take() {
            core.control.execution_budgets = budgets;
        }
        if let Some(coalescing) = self.delta_coalescing.take() {
            core.control.delta_coalescing = coalescing;
        }
        // The host's delivery bound is the one relay-policy source: the
        // recovery pass's relays and every immediate `deliver_now` derive
        // theirs from it (FIG-4246).
        core.control.recovery_pass = self.recovery_pass;
        core.control.trigger_route_restorer = self.trigger_route_restorer.take();
        if let Some(models) = self.models.clone() {
            core.providers.models = models;
        }
        core.providers.run_definitions = self.run_definitions.clone();
        if let Some(filter) = self.process_tool_visibility_filter.take() {
            core.control.process_tool_visibility_filter = Some(filter);
        }
        core
    }
}

#[cfg(all(test, feature = "otel-trace"))]
mod tests {}
