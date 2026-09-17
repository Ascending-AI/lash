use super::*;

/// The generic host-owned durability dependencies `RuntimeHostConfig::new`
/// requires. Grouped so a whole-config override rejects them in one place and
/// a from-parts build consumes them in one place — the five fields cannot be
/// listed twice because the group is moved out before overlay application is
/// reachable.
#[derive(Default)]
pub(super) struct HostDependencies {
    pub(super) effect_host: Option<Arc<dyn EffectHost>>,
    pub(super) attachment_store: Option<Arc<dyn AttachmentStore>>,
    pub(super) process_env_store: Option<Arc<dyn ProcessExecutionEnvStore>>,
    pub(super) commit_budget: Option<facade_support::CommitBudget>,
    pub(super) queued_work_batching: Option<facade_support::QueuedWorkBatchingConfig>,
}

impl HostDependencies {
    /// Reject a whole-config override that would duplicate a named
    /// dependency. Field names match the `RuntimeHostConfigConflict` contract.
    fn reject_if_any_set(&self) -> Result<()> {
        for (configured, field) in [
            (self.effect_host.is_some(), "effect_host"),
            (self.attachment_store.is_some(), "attachment_store"),
            (self.process_env_store.is_some(), "process_env_store"),
            (self.commit_budget.is_some(), "commit_budget"),
            (self.queued_work_batching.is_some(), "queued_work_batching"),
        ] {
            if configured {
                return Err(EmbedError::RuntimeHostConfigConflict { field });
            }
        }
        Ok(())
    }

    /// Consume all five dependencies into a fresh `RuntimeHostConfig`,
    /// erroring on the first unset one.
    fn take_or_missing(&mut self) -> Result<RuntimeHostConfig> {
        Ok(RuntimeHostConfig::new(
            self.effect_host
                .take()
                .ok_or(EmbedError::MissingEffectHost)?,
            self.attachment_store
                .take()
                .ok_or(EmbedError::MissingAttachmentStore)?,
            self.process_env_store
                .take()
                .ok_or(EmbedError::MissingProcessEnvStore)?,
            self.commit_budget
                .take()
                .ok_or(EmbedError::MissingCommitBudget)?,
            self.queued_work_batching
                .take()
                .ok_or(EmbedError::MissingQueuedWorkBatching)?,
        ))
    }
}

/// One builder field that duplicates a `RuntimeHostConfig` value. Both jobs
/// live on the same row — the conflict check a whole-config override runs and
/// the arm that applies the field to a base config — so the two cannot drift.
struct HostConfigOverlay {
    /// The `RuntimeHostConfigConflict` field name.
    field: &'static str,
    /// Whether setting this builder field conflicts with the supplied base.
    /// `provider` is the one conditional row: a builder provider only
    /// conflicts when the base config already resolves providers itself.
    conflicts: fn(&LashCoreBuilder, &RuntimeHostConfig) -> bool,
    /// Apply the field to the base config. Reached only when no conflict was
    /// reported, or on the from-parts path where the field is the sole source.
    apply: fn(&mut LashCoreBuilder, RuntimeHostConfig) -> RuntimeHostConfig,
}

const fn overlay(
    field: &'static str,
    conflicts: fn(&LashCoreBuilder, &RuntimeHostConfig) -> bool,
    apply: fn(&mut LashCoreBuilder, RuntimeHostConfig) -> RuntimeHostConfig,
) -> HostConfigOverlay {
    HostConfigOverlay {
        field,
        conflicts,
        apply,
    }
}

const HOST_CONFIG_OVERLAYS: &[HostConfigOverlay] = &[
    overlay(
        "max_attachment_bytes",
        |b, _| b.max_attachment_bytes.is_some(),
        |b, core| match b.max_attachment_bytes.take() {
            Some(max) => core.with_max_attachment_bytes(max),
            None => core,
        },
    ),
    overlay(
        "process_wake_delivery_policy",
        |b, _| b.process_wake_delivery_policy.is_some(),
        |b, mut core| {
            if let Some(policy) = b.process_wake_delivery_policy.take() {
                core.control.process_wake_delivery_policy = policy;
            }
            core
        },
    ),
    overlay(
        "prompt",
        |b, _| b.prompt.is_some(),
        |b, mut core| {
            if let Some(prompt) = b.prompt.take() {
                core.prompt.prompt = prompt;
            }
            core
        },
    ),
    overlay(
        "trace_sink",
        |b, _| b.trace_sink.is_some(),
        |b, mut core| {
            if let Some(sink) = b.trace_sink.take() {
                core.tracing.trace_sink = Some(sink);
            }
            core
        },
    ),
    overlay(
        "trace_level",
        |b, _| b.trace_level.is_some(),
        |b, mut core| {
            if let Some(level) = b.trace_level.take() {
                core.tracing.trace_level = level;
            }
            core
        },
    ),
    overlay(
        "trace_context",
        |b, _| b.trace_context.is_some(),
        |b, mut core| {
            if let Some(context) = b.trace_context.take() {
                core.tracing.trace_context = context;
            }
            core
        },
    ),
    overlay(
        "termination",
        |b, _| b.termination.is_some(),
        |b, mut core| {
            if let Some(termination) = b.termination.take() {
                core.control.termination = termination;
            }
            core
        },
    ),
    overlay(
        "abort_drain_grace",
        |b, _| b.abort_drain_grace.is_some(),
        |b, mut core| {
            if let Some(grace) = b.abort_drain_grace.take() {
                core.control.abort_drain_grace = grace;
            }
            core
        },
    ),
    overlay(
        "lease_timings",
        |b, _| b.lease_timings.is_some(),
        |b, mut core| {
            if let Some(timings) = b.lease_timings.take() {
                core.control.lease_timings = timings;
            }
            core
        },
    ),
    overlay(
        "clock",
        |b, _| b.clock.is_some(),
        |b, mut core| {
            if let Some(clock) = b.clock.take() {
                core.clock = clock;
            }
            core
        },
    ),
    overlay(
        "provider_resolver",
        |b, base| b.provider.is_some() && base.providers.provider_resolver.is_configured(),
        |b, mut core| {
            if let Some(provider) = b.provider.clone() {
                core.providers.provider_resolver =
                    Arc::new(facade_support::SingleProviderResolver::new(provider));
            }
            core
        },
    ),
    overlay(
        "process_tool_visibility_filter",
        |b, _| b.process_tool_visibility_filter.is_some(),
        |b, mut core| {
            if let Some(filter) = b.process_tool_visibility_filter.take() {
                core.control.process_tool_visibility_filter = Some(filter);
            }
            core
        },
    ),
];

impl LashCoreBuilder {
    /// Resolve the runtime host config, requiring the generic host-owned
    /// durability dependencies to have been named.
    pub(super) fn resolve_runtime_host_config(&mut self) -> Result<RuntimeHostConfig> {
        if let Some(base) = self.runtime_host_config.take() {
            self.reject_runtime_host_config_conflicts(&base)?;
            return Ok(self.apply_core_overrides(base));
        }
        let core = self.deps.take_or_missing()?;
        Ok(self.apply_core_overrides(core))
    }

    fn reject_runtime_host_config_conflicts(&self, base: &RuntimeHostConfig) -> Result<()> {
        self.deps.reject_if_any_set()?;
        for overlay in HOST_CONFIG_OVERLAYS {
            if (overlay.conflicts)(self, base) {
                return Err(EmbedError::RuntimeHostConfigConflict {
                    field: overlay.field,
                });
            }
        }
        Ok(())
    }

    /// Apply benign dependency overrides on top of a base core.
    fn apply_core_overrides(&mut self, mut core: RuntimeHostConfig) -> RuntimeHostConfig {
        for overlay in HOST_CONFIG_OVERLAYS {
            core = (overlay.apply)(self, core);
        }
        core
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopTraceSink;

    impl lash_trace::TraceSink for NoopTraceSink {
        fn append(
            &self,
            _record: &lash_trace::TraceRecord,
        ) -> std::result::Result<(), lash_trace::TraceSinkError> {
            Ok(())
        }
    }

    struct HideAllProcessTools;

    impl facade_support::ProcessToolVisibilityFilter for HideAllProcessTools {
        fn narrow(
            &self,
            _session: &lash_core::SessionId,
            _candidates: &[lash_core::ProcessId],
        ) -> Vec<lash_core::ProcessId> {
            Vec::new()
        }
    }

    fn test_provider() -> ProviderHandle {
        crate::testing::TestProvider::builder()
            .kind("host-config-conflict-test")
            .complete(|_| async { Ok(lash_core::LlmResponse::default()) })
            .build()
            .into_handle()
    }

    fn base_config() -> RuntimeHostConfig {
        RuntimeHostConfig::in_memory(
            facade_support::CommitBudget::bounded(1024, 16),
            facade_support::QueuedWorkBatchingConfig::new(1),
        )
    }

    fn fresh_builder() -> LashCoreBuilder {
        LashCoreBuilder::new(lash_core::TurnBudget::Unbounded)
    }

    fn assert_conflict(
        mut builder: LashCoreBuilder,
        base: RuntimeHostConfig,
        expected: &'static str,
    ) {
        builder.runtime_host_config = Some(base);
        match builder.resolve_runtime_host_config() {
            Err(EmbedError::RuntimeHostConfigConflict { field }) => {
                assert_eq!(field, expected)
            }
            _ => panic!("expected RuntimeHostConfigConflict for {expected}"),
        }
    }

    /// Every grouped host dependency must reject a whole-config override with
    /// its declared field name.
    #[test]
    fn every_host_dependency_conflicts_with_a_whole_config_by_name() {
        let mut builder = fresh_builder();
        builder.deps.effect_host = Some(Arc::new(facade_support::NativeEffectHost::default()));
        assert_conflict(builder, base_config(), "effect_host");

        let mut builder = fresh_builder();
        builder.deps.attachment_store =
            Some(Arc::new(facade_support::InMemoryAttachmentStore::new()));
        assert_conflict(builder, base_config(), "attachment_store");

        let mut builder = fresh_builder();
        builder.deps.process_env_store =
            Some(lash_core::testing::process_execution_env_fixture().0);
        assert_conflict(builder, base_config(), "process_env_store");

        let mut builder = fresh_builder();
        builder.deps.commit_budget = Some(facade_support::CommitBudget::bounded(1, 1));
        assert_conflict(builder, base_config(), "commit_budget");

        let mut builder = fresh_builder();
        builder.deps.queued_work_batching = Some(facade_support::QueuedWorkBatchingConfig::new(1));
        assert_conflict(builder, base_config(), "queued_work_batching");
    }

    /// Every overlay row must reject a whole-config override with the field
    /// name it declares. The match arms make the table self-checking: a new
    /// overlay row without a setter here fails this test.
    #[test]
    fn every_overlay_field_conflicts_with_a_whole_config_by_name() {
        for overlay in HOST_CONFIG_OVERLAYS {
            let mut builder = fresh_builder();
            match overlay.field {
                "max_attachment_bytes" => builder.max_attachment_bytes = Some(Some(1)),
                "process_wake_delivery_policy" => {
                    builder.process_wake_delivery_policy =
                        Some(lash_core::DeliveryPolicy::EarliestSafeBoundary)
                }
                "prompt" => builder.prompt = Some(PromptLayer::new()),
                "trace_sink" => builder.trace_sink = Some(Arc::new(NoopTraceSink)),
                "trace_level" => builder.trace_level = Some(lash_trace::TraceLevel::Standard),
                "trace_context" => {
                    builder.trace_context = Some(lash_trace::TraceContext::default())
                }
                "termination" => builder.termination = Some(TerminationPolicy::default()),
                "abort_drain_grace" => builder.abort_drain_grace = Some(std::time::Duration::ZERO),
                "lease_timings" => {
                    builder.lease_timings = Some(facade_support::LeaseTimings::default())
                }
                "clock" => builder.clock = Some(Arc::new(facade_support::SystemClock)),
                "provider_resolver" => builder.provider = Some(test_provider()),
                "process_tool_visibility_filter" => {
                    builder.process_tool_visibility_filter = Some(Arc::new(HideAllProcessTools))
                }
                other => panic!("no test setter for overlay field {other}"),
            }
            let mut base = base_config();
            if overlay.field == "provider_resolver" {
                // The row's conflict is conditional on the base config already
                // resolving providers itself.
                base.providers.provider_resolver =
                    Arc::new(facade_support::SingleProviderResolver::new(test_provider()));
            }
            assert_conflict(builder, base, overlay.field);
        }
    }

    /// A builder-only `max_attachment_bytes` still reaches the assembled
    /// config on the from-parts path — the one live wrapping arm.
    #[test]
    fn from_parts_build_applies_max_attachment_bytes() {
        let mut builder = fresh_builder();
        builder.deps.effect_host = Some(Arc::new(facade_support::NativeEffectHost::default()));
        builder.deps.attachment_store =
            Some(Arc::new(facade_support::InMemoryAttachmentStore::new()));
        builder.deps.process_env_store =
            Some(lash_core::testing::process_execution_env_fixture().0);
        builder.deps.commit_budget = Some(facade_support::CommitBudget::bounded(1, 1));
        builder.deps.queued_work_batching = Some(facade_support::QueuedWorkBatchingConfig::new(1));
        builder.max_attachment_bytes = Some(Some(4096));

        let core = builder
            .resolve_runtime_host_config()
            .expect("all dependencies named");
        assert_eq!(
            core.durability.attachment_store.max_attachment_bytes(),
            Some(4096)
        );
    }
}
