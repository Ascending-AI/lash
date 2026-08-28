use lash_trace::{TraceContext, TraceLevel, TraceSink};
use std::sync::Arc;

use super::process::{
    InMemoryProcessExecutionEnvStore, ProcessEngineRegistry, ProcessExecutionEnvStore,
    ProcessRegistry,
};
use super::{
    EffectHost, NativeEffectHost, NoQueuedWork, ProcessWorkSubstrate, ProcessWorkWiring,
    QueuedWorkSubstrate, SessionStoreFactory, TerminationPolicy,
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
    /// Host-selected boundary for process wakes entering the target session.
    pub process_wake_delivery_policy: crate::DeliveryPolicy,
    /// Optional narrow-only policy for the model-facing session process tools.
    pub process_tool_visibility_filter: Option<Arc<dyn crate::ProcessToolVisibilityFilter>>,
    /// Per-runtime registry cap on concurrently running managed child turns.
    /// Runtime-internal rolling-history compaction bypasses the cap while still
    /// being registered for observability and collision checks.
    /// Defaults to the runtime's managed-turn concurrency limit of 100.
    pub managed_turn_concurrency_limit: std::num::NonZeroUsize,
    /// Lease timing capability for every durable single-writer *lease* lane this
    /// runtime renews on a cadence: session execution leases, process leases,
    /// and durable effect-replay leases. Queued-work and turn-input claims are
    /// not leases and carry no TTL; they pin a session execution lease generation
    /// for claimability and handoff (ADR 0029). Defaults to
    /// [`crate::LeaseTimings::default`] (30s TTL / 10s renew).
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
                process_wake_delivery_policy: crate::DeliveryPolicy::EarliestSafeBoundary,
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
    /// [`NativeEffectHost`] and in-memory stores.
    ///
    /// Convenient for tests and local experiments; not durable. The commit
    /// budget remains required because backend latency policy is independent
    /// of whether persistence is in-memory.
    pub fn in_memory(
        commit_budget: crate::CommitBudget,
        queued_work_batching: crate::QueuedWorkBatchingConfig,
    ) -> Self {
        Self::new(
            Arc::new(NativeEffectHost::default()),
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

    /// Select when process wakes may enter a target session. This remains
    /// independent from the wake merge key and all batching safety gates.
    pub fn with_process_wake_delivery_policy(mut self, policy: crate::DeliveryPolicy) -> Self {
        self.control.process_wake_delivery_policy = policy;
        self
    }

    /// Set the per-runtime registry admission cap for managed child turns.
    pub fn with_managed_turn_concurrency_limit(mut self, limit: std::num::NonZeroUsize) -> Self {
        self.control.managed_turn_concurrency_limit = limit;
        self
    }
}

/// Base host shape for embedded runtimes.
///
/// "Embedded" means a runtime with no process registry.
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
    embedded: EmbeddedRuntimeHost,
    wiring: ProcessWorkWiring,
    queued_work: Arc<dyn QueuedWorkSubstrate>,
}

impl ProcessRuntimeHost {
    pub(crate) fn embedded(&self) -> &EmbeddedRuntimeHost {
        &self.embedded
    }

    /// Construct a process-capable host from a registry/port wiring and a
    /// required queued-work port.
    pub fn with_ports(
        embedded: EmbeddedRuntimeHost,
        wiring: ProcessWorkWiring,
        queued_work: Arc<dyn QueuedWorkSubstrate>,
    ) -> Self {
        Self {
            embedded,
            wiring,
            queued_work,
        }
    }

    /// Return the watched process registry installed on this host.
    pub fn process_registry(&self) -> &Arc<dyn ProcessRegistry> {
        self.wiring.registry()
    }

    /// Return the required queued-work port installed on this host.
    pub fn queued_work(&self) -> &Arc<dyn QueuedWorkSubstrate> {
        &self.queued_work
    }

    /// Return the process-work port bound to this host's registry.
    pub fn process_work(&self) -> &Arc<dyn ProcessWorkSubstrate> {
        self.wiring.port()
    }
}

/// A runtime's exhaustive work wiring.
#[derive(Clone)]
pub(crate) enum RuntimeWork {
    SessionsOnly {
        queued: Arc<dyn QueuedWorkSubstrate>,
    },
    Processes {
        wiring: ProcessWorkWiring,
        queued: Arc<dyn QueuedWorkSubstrate>,
    },
}

impl RuntimeWork {
    pub(crate) fn sessions_only(queued: Arc<dyn QueuedWorkSubstrate>) -> Self {
        Self::SessionsOnly { queued }
    }

    pub(crate) fn processes(
        wiring: ProcessWorkWiring,
        queued: Arc<dyn QueuedWorkSubstrate>,
    ) -> Self {
        Self::Processes { wiring, queued }
    }

    pub(crate) fn queued_arc(&self) -> &Arc<dyn QueuedWorkSubstrate> {
        match self {
            Self::SessionsOnly { queued } | Self::Processes { queued, .. } => queued,
        }
    }

    pub(crate) fn process_wiring(&self) -> Option<&ProcessWorkWiring> {
        match self {
            Self::SessionsOnly { .. } => None,
            Self::Processes { wiring, .. } => Some(wiring),
        }
    }

    pub(crate) fn with_queued(self, queued: Arc<dyn QueuedWorkSubstrate>) -> Self {
        match self {
            Self::SessionsOnly { .. } => Self::SessionsOnly { queued },
            Self::Processes { wiring, .. } => Self::Processes { wiring, queued },
        }
    }

    pub(crate) fn with_process_wiring(self, wiring: ProcessWorkWiring) -> Self {
        let queued = Arc::clone(self.queued_arc());
        Self::Processes { wiring, queued }
    }
}

#[derive(Clone)]
pub(crate) struct RuntimeHost {
    pub core: RuntimeHostConfig,
    pub session_store_factory: Option<Arc<dyn SessionStoreFactory>>,
    pub trigger_store: Option<Arc<dyn crate::TriggerStore>>,
    pub work: RuntimeWork,
}

impl RuntimeHost {
    pub(crate) fn from_embedded_with_work(
        embedded: EmbeddedRuntimeHost,
        work: RuntimeWork,
    ) -> Self {
        Self {
            core: embedded.core,
            session_store_factory: embedded.session_store_factory,
            trigger_store: embedded.trigger_store,
            work,
        }
    }

    pub(crate) fn process_registry(&self) -> Option<&Arc<dyn ProcessRegistry>> {
        self.work.process_wiring().map(ProcessWorkWiring::registry)
    }

    pub(crate) fn process_work(&self) -> Option<&Arc<dyn ProcessWorkSubstrate>> {
        self.work.process_wiring().map(ProcessWorkWiring::port)
    }

    pub(crate) fn queued_work(&self) -> &Arc<dyn QueuedWorkSubstrate> {
        self.work.queued_arc()
    }

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
        Self::from_embedded_with_work(
            value,
            RuntimeWork::sessions_only(Arc::new(NoQueuedWork::new())),
        )
    }
}

impl From<ProcessRuntimeHost> for RuntimeHost {
    fn from(value: ProcessRuntimeHost) -> Self {
        Self {
            core: value.embedded.core,
            session_store_factory: value.embedded.session_store_factory,
            trigger_store: value.embedded.trigger_store,
            work: RuntimeWork::processes(value.wiring, value.queued_work),
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
