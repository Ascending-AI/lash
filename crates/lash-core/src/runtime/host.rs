use lash_trace::{TraceContext, TraceLevel, TraceSink};
use std::sync::Arc;

use super::process::{
    InMemoryProcessExecutionEnvStore, ProcessEngineRegistry, ProcessExecutionEnvStore,
    ProcessRegistry,
};
use super::{
    EffectHost, InlineEffectHost, ProcessWorkDriver, QueuedWorkDriver, SessionStoreFactory,
    TerminationPolicy,
};

/// Default registry-wide admission cap for concurrently running managed child
/// turns in each runtime's opened-session registry, not across the process. It
/// matches the existing per-turn event channel bound so the registry cannot
/// admit more independently buffered turn streams than that established
/// resource envelope without an explicit host override. Runtime-internal
/// rolling-history compaction remains observable in the registry but is exempt
/// from this cap so correctness-critical context maintenance cannot be starved.
pub const DEFAULT_MANAGED_TURN_CONCURRENCY_LIMIT: usize = 100;

/// Required host configuration for all runtimes.
#[derive(Clone)]
pub struct RuntimeHostConfig {
    pub durability: RuntimeDurabilityConfig,
    pub process_engines: ProcessEngineRegistry,
    pub providers: RuntimeProviderConfig,
    pub prompt: RuntimePromptConfig,
    pub control: RuntimeControlConfig,
    pub tracing: RuntimeTracingConfig,
    pub attachment_source_policy: Arc<dyn crate::AttachmentSourcePolicy>,
    /// Injected time source. Durable timestamps and timeout/backoff logic read
    /// this rather than the OS clock directly, so replay is reproducible and
    /// tests can drive time. Defaults to [`SystemClock`](super::SystemClock).
    pub clock: Arc<dyn super::Clock>,
}

#[derive(Clone)]
pub struct RuntimeDurabilityConfig {
    /// Operational limits stamped onto every runtime commit assembled by this
    /// host and revalidated by the shared facade and concrete backend.
    pub commit_budget: crate::CommitBudget,
    /// Host-owned bounds for automatically grouping durable queued work.
    pub queued_work_batching: crate::QueuedWorkBatchingConfig,
    /// The session-bound attachment facade every runtime consumer sees. Hosts
    /// supply a flat [`AttachmentStore`](crate::AttachmentStore) backend
    /// (`RuntimeHostConfig::new`, the builder); the runtime wraps it here in a
    /// [`SessionAttachmentStore`](crate::SessionAttachmentStore) and rebinds it
    /// to the live session (with a reference-tracking manifest) at session
    /// start. Before rebinding it is an ephemeral facade with no boundary guard.
    pub attachment_store: Arc<crate::SessionAttachmentStore>,
    pub process_env_store: Arc<dyn ProcessExecutionEnvStore>,
}

#[derive(Clone)]
pub struct RuntimeProviderConfig {
    pub provider_resolver: Arc<dyn crate::RuntimeProviderResolver>,
}

#[derive(Clone)]
pub struct RuntimePromptConfig {
    pub prompt: crate::PromptLayer,
}

#[derive(Clone)]
pub struct RuntimeControlConfig {
    pub effect_host: Arc<dyn EffectHost>,
    pub termination: TerminationPolicy,
    /// Optional narrow-only policy for the model-facing session process tools.
    pub process_tool_visibility_filter: Option<Arc<dyn crate::ProcessToolVisibilityFilter>>,
    /// Per-runtime registry cap on concurrently running managed child turns.
    /// Runtime-internal rolling-history compaction bypasses the cap while still
    /// being registered for observability and collision checks.
    /// Defaults to [`DEFAULT_MANAGED_TURN_CONCURRENCY_LIMIT`].
    pub managed_turn_concurrency_limit: std::num::NonZeroUsize,
    /// Lease timing capability for every durable single-writer *lease* lane this
    /// runtime renews on a cadence: session execution leases, process leases,
    /// and durable effect-replay leases. Queued-work and turn-input claims are
    /// not leases and carry no TTL; they pin a session execution lease generation
    /// for claimability and handoff (ADR 0029). Defaults to
    /// [`LeaseTimings::default`] (30s TTL / 10s renew).
    pub lease_timings: crate::LeaseTimings,
}

#[derive(Clone)]
pub struct RuntimeTracingConfig {
    pub trace_sink: Option<Arc<dyn TraceSink>>,
    pub trace_level: TraceLevel,
    pub trace_context: TraceContext,
}

impl RuntimeHostConfig {
    /// Construct a config with the host-owned durability dependencies and
    /// commit budget named explicitly.
    ///
    /// There is intentionally no `Default`. The effect host, stores, and commit
    /// limits decide a runtime's durability envelope, so hosts must choose them
    /// rather than silently inheriting policy. Use
    /// [`RuntimeHostConfig::in_memory`] to opt into the in-process / in-memory
    /// implementations while still supplying the budget.
    pub fn new(
        effect_host: Arc<dyn EffectHost>,
        attachment_store: Arc<dyn crate::AttachmentStore>,
        process_env_store: Arc<dyn ProcessExecutionEnvStore>,
        commit_budget: crate::CommitBudget,
        queued_work_batching: crate::QueuedWorkBatchingConfig,
    ) -> Self {
        Self {
            durability: RuntimeDurabilityConfig {
                commit_budget,
                queued_work_batching,
                attachment_store: Arc::new(crate::SessionAttachmentStore::ephemeral(
                    attachment_store,
                )),
                process_env_store,
            },
            process_engines: ProcessEngineRegistry::new(),
            providers: RuntimeProviderConfig {
                provider_resolver: Arc::new(crate::EmptyProviderResolver),
            },
            prompt: RuntimePromptConfig {
                prompt: crate::PromptLayer::new(),
            },
            control: RuntimeControlConfig {
                termination: TerminationPolicy::default(),
                effect_host,
                lease_timings: crate::LeaseTimings::default(),
                process_tool_visibility_filter: None,
                managed_turn_concurrency_limit: std::num::NonZeroUsize::new(
                    DEFAULT_MANAGED_TURN_CONCURRENCY_LIMIT,
                )
                .expect("the managed-turn concurrency default is non-zero"),
            },
            tracing: RuntimeTracingConfig {
                trace_sink: None,
                trace_level: TraceLevel::Standard,
                trace_context: TraceContext::default(),
            },
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            clock: Arc::new(super::SystemClock),
        }
    }

    /// Replace the runtime time source. Hosts that need deterministic replay or
    /// test-driven time inject their own [`Clock`](super::Clock); the default is
    /// [`SystemClock`](super::SystemClock).
    pub fn with_clock(mut self, clock: Arc<dyn super::Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_attachment_source_policy(
        mut self,
        policy: Arc<dyn crate::AttachmentSourcePolicy>,
    ) -> Self {
        self.attachment_source_policy = policy;
        self
    }

    /// Explicit in-process / in-memory configuration: an
    /// [`InlineEffectHost`] and in-memory stores.
    ///
    /// Convenient for tests and local experiments; not durable. The commit
    /// budget remains required because backend latency policy is independent
    /// of whether persistence is in-memory.
    pub fn in_memory(
        commit_budget: crate::CommitBudget,
        queued_work_batching: crate::QueuedWorkBatchingConfig,
    ) -> Self {
        Self::new(
            Arc::new(InlineEffectHost::default()),
            Arc::new(crate::InMemoryAttachmentStore::new()),
            Arc::new(InMemoryProcessExecutionEnvStore::new()),
            commit_budget,
            queued_work_batching,
        )
    }

    pub fn with_process_env_store(
        mut self,
        process_env_store: Arc<dyn ProcessExecutionEnvStore>,
    ) -> Self {
        self.durability.process_env_store = process_env_store;
        self
    }

    pub fn with_process_engine(mut self, engine: Arc<dyn crate::ProcessEngine>) -> Self {
        self.process_engines = self.process_engines.with_engine(engine);
        self
    }

    /// Replace the lease timing capability governing every durable lease and
    /// claim this runtime takes.
    pub fn with_lease_timings(mut self, lease_timings: crate::LeaseTimings) -> Self {
        self.control.lease_timings = lease_timings;
        self
    }

    pub fn with_process_tool_visibility_filter(
        mut self,
        filter: Arc<dyn crate::ProcessToolVisibilityFilter>,
    ) -> Self {
        self.control.process_tool_visibility_filter = Some(filter);
        self
    }

    /// Set the per-runtime registry admission cap for managed child turns.
    pub fn with_managed_turn_concurrency_limit(mut self, limit: std::num::NonZeroUsize) -> Self {
        self.control.managed_turn_concurrency_limit = limit;
        self
    }
}

/// Base host shape for embedded runtimes.
#[derive(Clone)]
pub struct EmbeddedRuntimeHost {
    pub core: RuntimeHostConfig,
    pub session_store_factory: Option<Arc<dyn SessionStoreFactory>>,
    pub trigger_store: Option<Arc<dyn crate::TriggerStore>>,
}

impl EmbeddedRuntimeHost {
    pub fn new(core: RuntimeHostConfig) -> Self {
        let clock = Arc::clone(&core.clock);
        Self {
            core,
            session_store_factory: None,
            trigger_store: Some(Arc::new(crate::InMemoryTriggerStore::with_clock(clock))),
        }
    }

    pub fn with_session_store_factory(
        mut self,
        session_store_factory: Arc<dyn SessionStoreFactory>,
    ) -> Self {
        self.session_store_factory = Some(session_store_factory);
        self
    }

    pub fn with_trigger_store(mut self, store: Arc<dyn crate::TriggerStore>) -> Self {
        self.trigger_store = Some(store);
        self
    }
}

/// Host shape for runtimes that support background plugin work.
#[derive(Clone)]
pub struct ProcessRuntimeHost {
    pub embedded: EmbeddedRuntimeHost,
    pub process_registry: Arc<dyn ProcessRegistry>,
    pub process_work_driver: Option<ProcessWorkDriver>,
    pub queued_work_driver: Option<QueuedWorkDriver>,
}

impl ProcessRuntimeHost {
    pub fn new(embedded: EmbeddedRuntimeHost, process_registry: Arc<dyn ProcessRegistry>) -> Self {
        Self {
            embedded,
            process_registry,
            process_work_driver: None,
            queued_work_driver: None,
        }
    }

    pub fn with_process_work_driver(mut self, driver: ProcessWorkDriver) -> Self {
        self.process_work_driver = Some(driver);
        self
    }

    pub fn with_queued_work_driver(mut self, driver: QueuedWorkDriver) -> Self {
        self.queued_work_driver = Some(driver);
        self
    }
}

#[derive(Clone)]
pub(crate) struct RuntimeHost {
    pub core: RuntimeHostConfig,
    pub session_store_factory: Option<Arc<dyn SessionStoreFactory>>,
    pub trigger_store: Option<Arc<dyn crate::TriggerStore>>,
    pub process_registry: Option<Arc<dyn ProcessRegistry>>,
    /// Host-owned process work driver. Absent when no process registry is wired.
    pub process_work_driver: Option<ProcessWorkDriver>,
    /// Host-owned queued work driver. Absent when queued work is delegated to an
    /// external host or no session store exists.
    pub queued_work_driver: Option<QueuedWorkDriver>,
}

impl RuntimeHost {
    pub(crate) fn resolve_session_policy(
        &self,
        session_id: &str,
        policy: crate::SessionPolicy,
    ) -> Result<crate::RuntimeSessionPolicy, crate::SessionError> {
        let provider_id = policy.recorded_provider_id();
        let mut binding = self
            .core
            .providers
            .provider_resolver
            .resolve_provider_binding(provider_id)
            .map_err(|err| match err {
                crate::ProviderResolutionError::MissingProviderId => {
                    crate::SessionError::ProviderUnconfigured {
                        session_id: session_id.to_string(),
                    }
                }
                crate::ProviderResolutionError::UnknownProvider { provider_id } => {
                    crate::SessionError::ProviderUnavailable {
                        provider_id,
                        session_id: session_id.to_string(),
                    }
                }
                crate::ProviderResolutionError::ProviderIdMismatch { expected, actual } => {
                    crate::SessionError::ProviderMismatch {
                        expected,
                        actual,
                        session_id: session_id.to_string(),
                    }
                }
            })?;
        binding.provider = binding.provider.with_clock(Arc::clone(&self.core.clock));
        Ok(crate::RuntimeSessionPolicy::new(policy, binding))
    }
}

impl From<EmbeddedRuntimeHost> for RuntimeHost {
    fn from(value: EmbeddedRuntimeHost) -> Self {
        Self {
            core: value.core,
            session_store_factory: value.session_store_factory,
            trigger_store: value.trigger_store,
            process_registry: None,
            process_work_driver: None,
            queued_work_driver: None,
        }
    }
}

impl From<ProcessRuntimeHost> for RuntimeHost {
    fn from(value: ProcessRuntimeHost) -> Self {
        Self {
            core: value.embedded.core,
            session_store_factory: value.embedded.session_store_factory,
            trigger_store: value.embedded.trigger_store,
            process_registry: Some(value.process_registry),
            process_work_driver: value.process_work_driver,
            queued_work_driver: value.queued_work_driver,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_turn_concurrency_limit_defaults_to_event_channel_bound() {
        let config = RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );

        assert_eq!(
            config.control.managed_turn_concurrency_limit.get(),
            DEFAULT_MANAGED_TURN_CONCURRENCY_LIMIT
        );
        assert_eq!(DEFAULT_MANAGED_TURN_CONCURRENCY_LIMIT, 100);
    }
}
