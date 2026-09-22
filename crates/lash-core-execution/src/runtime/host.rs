use crate::SessionId;
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

/// Default attempt bound stamped onto children started by the engine that runs
/// a script, rather than by a host that states its own budget.
///
/// A registration with no bound asks the engine to pace retries indefinitely,
/// so a child that fails the same way every attempt never reaches a terminal
/// fact and its awaiters never resolve. Five attempts absorb the transient
/// owner losses a durable child is expected to survive — a worker crash, a
/// lapsed lease, a redrive — and then hand the deterministic failure to the
/// host as an Abandoned fact written by the engine, per ADR 0019. Hosts that
/// want a different budget set one; the value is resolved once and recorded on
/// the child's row, so changing it never rewrites a child already registered.
pub const DEFAULT_ENGINE_CHILD_MAX_ATTEMPTS: std::num::NonZeroU32 =
    match std::num::NonZeroU32::new(5) {
        Some(value) => value,
        None => unreachable!(),
    };

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

/// Default [`RuntimeControlConfig::abort_drain_grace`].
pub const DEFAULT_ABORT_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(2_000);

#[derive(Clone)]
pub struct RuntimeControlConfig {
    pub effect_host: Arc<dyn EffectHost>,
    pub termination: TerminationPolicy,
    /// How long a protocol-owned stream abort (a protocol boundary that ends
    /// the model's turn under ADR 0036's no-wire-stop rule) keeps draining the
    /// provider stream before the task is aborted. The drain exists so a
    /// cooperative provider's trailing usage event still lands on the aborted
    /// attempt; past the grace the attempt is sealed with a typed unreported
    /// disposition (ADR 0031). Defaults to
    /// [`DEFAULT_ABORT_DRAIN_GRACE`] (2 s).
    pub abort_drain_grace: std::time::Duration,
    /// Host-selected boundary for process wakes entering the target session.
    pub process_wake_delivery_policy: crate::DeliveryPolicy,
    /// Optional narrow-only policy for the model-facing session process tools.
    pub process_tool_visibility_filter: Option<Arc<dyn crate::ProcessToolVisibilityFilter>>,
    /// Lease timing capability for every durable single-writer *lease* lane this
    /// runtime renews on a cadence: session execution leases, process leases,
    /// and durable effect-replay leases. Queued-work and turn-input claims are
    /// not leases and carry no TTL; they pin a session execution lease generation
    /// for claimability and handoff (ADR 0029). Defaults to
    /// [`crate::LeaseTimings::default`] (30s TTL / 10s renew).
    pub lease_timings: crate::LeaseTimings,
    /// What an open does when a persisted tool id no registered source
    /// resolves. Defaults to
    /// [`ToolSourcePolicy::Tolerate`](crate::ToolSourcePolicy): the session
    /// opens and the host receives the typed
    /// [`ToolRestoreReport`](crate::ToolRestoreReport). Set
    /// [`Require`](crate::ToolSourcePolicy::Require) in unattended or
    /// fixed-tool deployments to refuse an open that lost a catalog member.
    /// It is carried on the host config, not on the session, so every
    /// runtime-initiated construction honours the host's choice.
    pub tool_source_policy: crate::ToolSourcePolicy,
    /// What an open does with the persisted tool surface. Defaults to
    /// [`ToolSurfaceOpenMode::Reconcile`](crate::ToolSurfaceOpenMode). An open
    /// that will not run a turn — enqueue-only or read-only — is declared with
    /// [`PreservePersisted`](crate::ToolSurfaceOpenMode::PreservePersisted),
    /// which skips the tool-state reconcile and catalog rebuild so an open on
    /// a core without the session's sources cannot orphan or restamp its
    /// persisted tools (FIG-3353). Carried on the host config so every
    /// construction below the facade sees the same choice.
    pub tool_surface_open_mode: crate::ToolSurfaceOpenMode,
    /// Attempt bound stamped onto every child a script engine starts on the
    /// model's behalf, where no host or tool author is present to state one.
    /// Resolved once per execution segment and recorded on the child's record,
    /// so it is hashed by the registration fingerprint and a redrive across a
    /// config change re-registers the recorded value instead of conflicting.
    /// Defaults to [`DEFAULT_ENGINE_CHILD_MAX_ATTEMPTS`].
    pub engine_child_max_attempts: std::num::NonZeroU32,
    /// This deployment's tool-child wiring: the live-opener registry a turn or
    /// process incarnation registers itself in, and the resolver that routes a
    /// journaled tool child of an effect group to the handler-level driver
    /// (ADR 0099 §2, FIG-2266).
    ///
    /// Default wiring, not an opt-in: it is installed on the effect host here,
    /// so native, SQLite and PostgreSQL deployments all route first dispatch
    /// and recovery through the one resolver without a caller-closure route.
    ///
    /// `None` when the host routes no tool children — it implements no durable
    /// effect groups, or a different resolver is already registered on it,
    /// which the conformance suites do deliberately. Openers are then not
    /// registered either, so nothing is half-wired.
    pub tool_children: Option<Arc<crate::runtime::effect::ToolChildHost>>,
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
        let clock: Arc<dyn super::Clock> = Arc::new(super::SystemClock);
        let tool_children =
            effect_host.install_tool_child_host(crate::runtime::effect::ToolChildHost::new(
                &effect_host,
                Arc::clone(&process_env_store),
            ));
        if let Some(tool_children) = &tool_children {
            tool_children.with_clock(Arc::clone(&clock));
        }
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
                abort_drain_grace: DEFAULT_ABORT_DRAIN_GRACE,
                effect_host,
                process_wake_delivery_policy: crate::DeliveryPolicy::EarliestSafeBoundary,
                lease_timings: crate::LeaseTimings::default(),
                process_tool_visibility_filter: None,
                tool_source_policy: crate::ToolSourcePolicy::default(),
                tool_surface_open_mode: crate::ToolSurfaceOpenMode::default(),
                engine_child_max_attempts: DEFAULT_ENGINE_CHILD_MAX_ATTEMPTS,
                tool_children,
            },
            tracing: RuntimeTracingConfig {
                trace_sink: None,
                trace_level: TraceLevel::Standard,
                trace_context: TraceContext::default(),
            },
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            clock,
        }
    }

    /// Replace the runtime time source. Hosts that need deterministic replay or
    /// test-driven time inject their own [`Clock`](super::Clock); the default is
    /// [`SystemClock`](super::SystemClock).
    ///
    /// Also propagates to the installed tool-child host, whose `Sleep`/
    /// `AwaitEvent` group-child executors wait on it: the host was installed
    /// get-or-init before this clock existed, so the update happens in place
    /// rather than by re-install.
    pub fn with_clock(mut self, clock: Arc<dyn super::Clock>) -> Self {
        if let Some(tool_children) = &self.control.tool_children {
            tool_children.with_clock(Arc::clone(&clock));
        }
        self.clock = clock;
        self
    }

    /// `None` is the default and preserves unbounded attachment puts. A
    /// configured limit is independent from the runtime commit budget and is
    /// enforced before the attachment backend is called.
    pub fn with_max_attachment_bytes(mut self, max_attachment_bytes: Option<u64>) -> Self {
        self.durability.attachment_store = Arc::new(
            self.durability
                .attachment_store
                .reconfigured_max_attachment_bytes(max_attachment_bytes),
        );
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

    pub fn with_process_engine_registration(
        mut self,
        registration: crate::ProcessEngineRegistration,
    ) -> Self {
        self.process_engines = self.process_engines.with_registration(registration);
        self
    }
}

impl crate::plugin::ProcessEngineContributionTarget for RuntimeHostConfig {
    fn process_engine_trace_context(&self) -> &TraceContext {
        &self.tracing.trace_context
    }

    fn install_contributed_process_engine(
        &mut self,
        registration: crate::ProcessEngineRegistration,
    ) -> Result<(), crate::PluginError> {
        self.process_engines = self.process_engines.clone().try_with_engine(registration)?;
        Ok(())
    }
}

impl RuntimeHostConfig {
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

    /// This remains
    /// independent from the wake merge key and all batching safety gates.
    pub fn with_process_wake_delivery_policy(mut self, policy: crate::DeliveryPolicy) -> Self {
        self.control.process_wake_delivery_policy = policy;
        self
    }

    /// Set the attempt bound stamped onto children a script engine starts.
    ///
    /// The bound is resolved when an execution segment begins and recorded on
    /// each child it registers, so a change takes effect for children started
    /// after it and never for one already on the registry.
    pub fn with_engine_child_max_attempts(mut self, max_attempts: std::num::NonZeroU32) -> Self {
        self.control.engine_child_max_attempts = max_attempts;
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
    /// Durable home for the named process-definition registry (FIG-2995).
    pub process_definitions: Option<Arc<dyn crate::ProcessDefinitionRegistry>>,
}

impl EmbeddedRuntimeHost {
    pub fn new(core: RuntimeHostConfig) -> Self {
        let clock = Arc::clone(&core.clock);
        Self {
            core,
            session_store_factory: None,
            trigger_store: Some(Arc::new(crate::InMemoryTriggerStore::with_clock(clock))),
            process_definitions: Some(
                Arc::new(crate::InMemoryProcessDefinitionRegistry::default()),
            ),
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

    pub fn with_process_definition_registry(
        mut self,
        registry: Arc<dyn crate::ProcessDefinitionRegistry>,
    ) -> Self {
        self.process_definitions = Some(registry);
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
    pub fn embedded(&self) -> &EmbeddedRuntimeHost {
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

    pub fn process_registry(&self) -> &Arc<dyn ProcessRegistry> {
        self.wiring.registry()
    }

    /// Return the required queued-work port installed on this host.
    pub fn queued_work(&self) -> &Arc<dyn QueuedWorkSubstrate> {
        &self.queued_work
    }

    pub fn process_work(&self) -> &Arc<dyn ProcessWorkSubstrate> {
        self.wiring.port()
    }
}

/// A runtime's exhaustive work wiring, and the single owner of whether this
/// runtime has a process registry.
///
/// `RegistryOnly` is the named state a host is in while it holds a watched
/// registry but has not resolved its native process port yet (the facade's lazy
/// native composition). It is a state of the wiring rather than a field beside
/// it, so "does this runtime have a process registry" has exactly one answer no
/// matter which layer asks.
#[derive(Clone)]
pub enum RuntimeWork {
    SessionsOnly {
        queued: Arc<dyn QueuedWorkSubstrate>,
    },
    RegistryOnly {
        registry: Arc<dyn ProcessRegistry>,
        queued: Arc<dyn QueuedWorkSubstrate>,
    },
    Processes {
        wiring: ProcessWorkWiring,
        queued: Arc<dyn QueuedWorkSubstrate>,
    },
}

impl RuntimeWork {
    pub fn sessions_only(queued: Arc<dyn QueuedWorkSubstrate>) -> Self {
        Self::SessionsOnly { queued }
    }

    pub fn registry_only(
        registry: Arc<dyn ProcessRegistry>,
        queued: Arc<dyn QueuedWorkSubstrate>,
    ) -> Self {
        Self::RegistryOnly { registry, queued }
    }

    pub fn processes(wiring: ProcessWorkWiring, queued: Arc<dyn QueuedWorkSubstrate>) -> Self {
        Self::Processes { wiring, queued }
    }

    pub fn queued_arc(&self) -> &Arc<dyn QueuedWorkSubstrate> {
        match self {
            Self::SessionsOnly { queued }
            | Self::RegistryOnly { queued, .. }
            | Self::Processes { queued, .. } => queued,
        }
    }

    /// The process registry this runtime carries, in either the registry-only
    /// or the fully wired state.
    pub fn process_registry(&self) -> Option<&Arc<dyn ProcessRegistry>> {
        match self {
            Self::SessionsOnly { .. } => None,
            Self::RegistryOnly { registry, .. } => Some(registry),
            Self::Processes { wiring, .. } => Some(wiring.registry()),
        }
    }

    pub fn process_wiring(&self) -> Option<&ProcessWorkWiring> {
        match self {
            Self::SessionsOnly { .. } | Self::RegistryOnly { .. } => None,
            Self::Processes { wiring, .. } => Some(wiring),
        }
    }

    pub fn with_queued(self, queued: Arc<dyn QueuedWorkSubstrate>) -> Self {
        match self {
            Self::SessionsOnly { .. } => Self::SessionsOnly { queued },
            Self::RegistryOnly { registry, .. } => Self::RegistryOnly { registry, queued },
            Self::Processes { wiring, .. } => Self::Processes { wiring, queued },
        }
    }

    /// Wire full process work, replacing whatever registry state was there.
    /// Setting both is a last-write-wins transition, not an error: the wiring
    /// carries its own registry.
    pub fn with_process_wiring(self, wiring: ProcessWorkWiring) -> Self {
        let queued = Arc::clone(self.queued_arc());
        Self::Processes { wiring, queued }
    }

    pub fn with_process_registry(self, registry: Arc<dyn ProcessRegistry>) -> Self {
        let queued = Arc::clone(self.queued_arc());
        Self::RegistryOnly { registry, queued }
    }

    /// Rebind the work ports. Dropping a process wiring keeps the registry it
    /// carried: losing the port is not losing the registry.
    pub fn with_work_ports(
        self,
        process: Option<ProcessWorkWiring>,
        queued: Arc<dyn QueuedWorkSubstrate>,
    ) -> Self {
        match process {
            Some(wiring) => Self::Processes { wiring, queued },
            None => match self.process_registry().cloned() {
                Some(registry) => Self::RegistryOnly { registry, queued },
                None => Self::SessionsOnly { queued },
            },
        }
    }
}

#[derive(Clone)]
pub struct RuntimeHost {
    pub core: RuntimeHostConfig,
    pub session_store_factory: Option<Arc<dyn SessionStoreFactory>>,
    pub trigger_store: Option<Arc<dyn crate::TriggerStore>>,
    pub process_definitions: Option<Arc<dyn crate::ProcessDefinitionRegistry>>,
    pub work: RuntimeWork,
}

impl RuntimeHost {
    pub fn from_embedded_with_work(embedded: EmbeddedRuntimeHost, work: RuntimeWork) -> Self {
        Self {
            core: embedded.core,
            session_store_factory: embedded.session_store_factory,
            trigger_store: embedded.trigger_store,
            process_definitions: embedded.process_definitions,
            work,
        }
    }

    pub fn process_registry(&self) -> Option<&Arc<dyn ProcessRegistry>> {
        self.work.process_registry()
    }

    pub fn process_work(&self) -> Option<&Arc<dyn ProcessWorkSubstrate>> {
        self.work.process_wiring().map(ProcessWorkWiring::port)
    }

    pub fn queued_work(&self) -> &Arc<dyn QueuedWorkSubstrate> {
        self.work.queued_arc()
    }

    pub fn resolve_session_policy(
        &self,
        session_id: &SessionId,
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
                        session_id: SessionId::from(session_id.to_string()),
                    }
                }
                crate::ProviderResolutionError::UnknownProvider { provider_id } => {
                    crate::SessionError::ProviderUnavailable {
                        provider_id,
                        session_id: SessionId::from(session_id.to_string()),
                    }
                }
                crate::ProviderResolutionError::ProviderIdMismatch { expected, actual } => {
                    crate::SessionError::ProviderMismatch {
                        expected,
                        actual,
                        session_id: SessionId::from(session_id.to_string()),
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
            process_definitions: value.embedded.process_definitions,
            work: RuntimeWork::processes(value.wiring, value.queued_work),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_limit_defaults_unbounded_and_accepts_host_override() {
        let unbounded = RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
        assert_eq!(
            unbounded.durability.attachment_store.max_attachment_bytes(),
            None
        );

        let bounded = unbounded.with_max_attachment_bytes(Some(4096));
        assert_eq!(
            bounded.durability.attachment_store.max_attachment_bytes(),
            Some(4096)
        );
    }
}
