use crate::ActorContext;
use crate::SessionId;
use lash_trace::TraceSink;
use std::sync::Arc;

use super::process::{
    ArtifactReferrerPorts, ProcessEngineRegistry, ProcessExecutionEnvStore, ProcessRegistry,
};
use super::{
    DeploymentStore, NoSessionWork, ProcessWorkSubstrate, ProcessWorkWiring, SessionWorkEngine,
    TerminationPolicy,
};

/// Required host configuration for all runtimes.
///
/// A config is built over exactly one [`Backend`](crate::Backend) (ADR 0102,
/// D2): its effect host, attachment port, process-exec-env store and clock
/// start as the backend's, and every other port a runtime reaches — the
/// session-store factory, the trigger store and the process-definition
/// registry — is read from the same backend. There is no in-memory default.
#[derive(Clone)]
pub struct RuntimeHostConfig {
    backend: crate::Backend,
    pub durability: RuntimeDurabilityConfig,
    pub process_engines: ProcessEngineRegistry,
    pub providers: RuntimeProviderConfig,
    pub control: RuntimeControlConfig,
    /// The runtime's one trace handle: every engine path and every plugin
    /// emits through it.
    pub tracing: crate::trace::TraceRuntime,
    pub attachment_source_policy: Arc<dyn crate::AttachmentSourcePolicy>,
    /// Injected time source. Durable timestamps and timeout/backoff logic read
    /// this rather than the OS clock directly, so replay is reproducible and
    /// tests can advance time. Defaults to [`SystemClock`](super::SystemClock).
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
    /// [`RuntimeAttachmentStore`](crate::RuntimeAttachmentStore) and rebinds it
    /// to the live session (with a reference-tracking manifest) at session
    /// start. Before rebinding it is an ephemeral facade with no boundary guard.
    pub attachment_store: Arc<crate::RuntimeAttachmentStore>,
    pub process_env_store: Arc<dyn ProcessExecutionEnvStore>,
    /// Where a turn's environment sync records its preparation, which the
    /// sync's outcome journals by digest (FIG-5133).
    pub turn_prelude_store: Arc<dyn crate::TurnPreludeStore>,
}

#[derive(Clone)]
pub struct RuntimeProviderConfig {
    /// The host's models: the registry that mints a session's model binding
    /// and binds a recorded one to its transport.
    pub models: Arc<dyn crate::LlmProfiles>,
    /// The run definitions this deployment registers (FIG-3838): a run
    /// whose spec names a definition resolves it here, by exact reference.
    pub run_definitions: crate::RunDefinitions,
}

/// How a turn coalesces the prose and reasoning deltas of a stream block
/// before they reach the host sinks and the live replay store (FIG-5098).
///
/// While on, each lane holds one open frame of a block's deltas. A frame is
/// published [`interval`](Self::interval) after it opened (later if the host
/// is still taking events queued ahead of it, in which case it keeps
/// absorbing deltas), and is cut early by any other event, by a delta of
/// another block, and before it would pass
/// [`max_frame_bytes`](Self::max_frame_bytes) of text. With
/// [`first_delta_immediate`](Self::first_delta_immediate), the first delta of
/// a block is published at once, so time to first token never waits on a
/// frame. [`off`](Self::off) publishes one event per delta.
///
/// The defaults are a 50 ms interval, an 8 KiB frame cap and an immediate
/// first delta. It is a runtime option, independent of the live replay store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeltaCoalescing {
    /// Zero when coalescing is off.
    interval: std::time::Duration,
    max_frame_bytes: usize,
    first_delta_immediate: bool,
}

/// A [`DeltaCoalescing`] value out of range.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DeltaCoalescingError {
    #[error(
        "the delta frame interval must be at most {max:?} (zero turns coalescing off), not {interval:?}"
    )]
    Interval {
        interval: std::time::Duration,
        max: std::time::Duration,
    },
    #[error("the delta frame cap must be between 1 and {max} bytes, not {max_frame_bytes}")]
    MaxFrameBytes { max_frame_bytes: usize, max: usize },
}

impl DeltaCoalescing {
    /// The default frame interval.
    pub const DEFAULT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
    /// The default frame cap, in bytes of text.
    pub const DEFAULT_MAX_FRAME_BYTES: usize = 8 * 1024;
    /// The longest frame interval: a frame must stay well inside the live
    /// replay window.
    pub const MAX_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
    /// The largest frame cap.
    pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

    /// Coalescing on these terms. A zero `interval` turns coalescing off; an
    /// interval past [`MAX_INTERVAL`](Self::MAX_INTERVAL), or a frame cap of
    /// zero or past [`MAX_FRAME_BYTES`](Self::MAX_FRAME_BYTES), is refused.
    pub fn new(
        interval: std::time::Duration,
        max_frame_bytes: usize,
        first_delta_immediate: bool,
    ) -> Result<Self, DeltaCoalescingError> {
        if interval > Self::MAX_INTERVAL {
            return Err(DeltaCoalescingError::Interval {
                interval,
                max: Self::MAX_INTERVAL,
            });
        }
        if max_frame_bytes == 0 || max_frame_bytes > Self::MAX_FRAME_BYTES {
            return Err(DeltaCoalescingError::MaxFrameBytes {
                max_frame_bytes,
                max: Self::MAX_FRAME_BYTES,
            });
        }
        Ok(Self {
            interval,
            max_frame_bytes,
            first_delta_immediate,
        })
    }

    /// No coalescing: every delta is its own event.
    #[must_use]
    pub const fn off() -> Self {
        Self {
            interval: std::time::Duration::ZERO,
            max_frame_bytes: Self::DEFAULT_MAX_FRAME_BYTES,
            first_delta_immediate: true,
        }
    }

    /// Whether every delta is its own event.
    #[must_use]
    pub fn is_off(&self) -> bool {
        self.interval.is_zero()
    }

    /// How long a frame stays open; zero when coalescing is off.
    #[must_use]
    pub fn interval(&self) -> std::time::Duration {
        self.interval
    }

    /// The most text one frame carries, in bytes.
    #[must_use]
    pub fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    /// Whether the first delta of a block is published at once rather than
    /// opening a frame.
    #[must_use]
    pub fn first_delta_immediate(&self) -> bool {
        self.first_delta_immediate
    }
}

impl Default for DeltaCoalescing {
    fn default() -> Self {
        Self {
            interval: Self::DEFAULT_INTERVAL,
            max_frame_bytes: Self::DEFAULT_MAX_FRAME_BYTES,
            first_delta_immediate: true,
        }
    }
}

#[derive(Clone)]
pub struct RuntimeControlConfig {
    pub effect_host: ActorContext,
    /// Live restoration of captured provider routes for new trigger starts,
    /// shared by immediate delivery and recovery. Never journaled as wiring.
    pub trigger_route_restorer: Option<Arc<dyn crate::TriggerRouteRestorer>>,
    /// The termination policy a run records on its first execution. Terminal
    /// assembly reads the run's record, never this field (FIG-4389).
    pub termination: TerminationPolicy,
    /// Every execution bound this runtime enforces (spec v3 Part C): the
    /// tool default and inline ceiling, the model call's hard total, the
    /// control-phase bound, the stop grace, the wait bounds and the provider
    /// attempt limits. The stop grace is also how long a protocol-owned
    /// stream abort (ADR 0036) keeps draining the provider stream, so a
    /// cooperative provider's trailing usage still lands on the aborted
    /// attempt; past it the attempt is sealed with a typed unreported
    /// disposition (ADR 0031). Defaults to [`crate::ExecutionBudgets::default`].
    pub execution_budgets: crate::ExecutionBudgets,
    /// How a turn coalesces its stream deltas into frames before they reach
    /// the host sinks and the live replay store (FIG-5098). Defaults to
    /// [`DeltaCoalescing::default`]: 50 ms frames of at most 8 KiB, the first
    /// delta of a block published at once.
    pub delta_coalescing: DeltaCoalescing,
    /// Host-selected boundary for process wakes entering the target session.
    pub process_wake_delivery_policy: crate::DeliveryPolicy,
    /// Optional narrow-only policy for the model-facing session process tools.
    pub process_tool_visibility_filter: Option<Arc<dyn crate::ProcessToolVisibilityFilter>>,
    /// Lease timing capability for every durable single-writer *lease* lane this
    /// runtime renews on a cadence: session execution leases,
    /// and durable effect-replay leases. Queued work and turn inputs are not
    /// leased and carry no TTL: a run admits them under its shift fence and
    /// holds them until its own commit settles them (FIG-3927). Defaults to
    /// [`crate::LeaseTimings::default`] (30s TTL / 10s renew).
    pub lease_timings: crate::LeaseTimings,
    /// What a turn run does when a persisted tool id no registered source
    /// resolves. Defaults to
    /// [`ToolSourcePolicy::Tolerate`](crate::ToolSourcePolicy): the run goes
    /// on and reports the typed
    /// [`ToolRestoreReport`](crate::ToolRestoreReport) as its
    /// `TurnEvent::ToolRestoreReported`. Set
    /// [`Require`](crate::ToolSourcePolicy::Require) in unattended or
    /// fixed-tool deployments to refuse, at its plugin transition, a run that
    /// would lose a catalog member (FIG-5134). It is carried on the host
    /// config of the runtime that executes the run, so a session's resident
    /// open applies the policy its open stated.
    pub tool_source_policy: crate::ToolSourcePolicy,
    /// Where the shift reports a logical run's closed scope, after the
    /// run's terminal evidence is durable (FIG-3607 item 7). Defaults to
    /// [`NoScopeClose`](crate::engine::NoScopeClose); a host composition that
    /// owns lifetime scopes installs the process registry's
    /// [`RegistryScopeClose`](crate::runtime::process::RegistryScopeClose).
    ///
    /// A close reaches this sink only as the delivery of the `ScopeClose`
    /// obligation the terminal transaction armed on the run's row, through
    /// the backend's ledger of that kind (ADR 0109 §3).
    pub scope_close: Arc<dyn crate::engine::ScopeCloseSink>,
    /// The host's bound on one obligation delivery (ADR 0109 §1.8). This is
    /// the one source every relay reads: [`relay_policy`](Self::relay_policy)
    /// derives the policy a reconcile tick's due pass and a producer's
    /// immediate `deliver_now` run under alike, so a delivery honors the
    /// host's bound however it is reached.
    pub recovery_pass: crate::engine::RecoveryPassBudget,
}

impl RuntimeControlConfig {
    /// The [`RelayPolicy`](crate::runtime::shift::relay::RelayPolicy) every
    /// obligation relay of this runtime runs under: the recovery pass's
    /// attempt budget on the kinds' shared retry shape. There is no second
    /// default — a `deliver_now` construction that skips it builds a relay
    /// at the 30 s kind default instead.
    #[must_use]
    pub fn relay_policy(&self) -> crate::runtime::shift::relay::RelayPolicy {
        crate::runtime::shift::relay::RelayPolicy {
            attempt_budget_ms: self.recovery_pass.attempt_ms(),
            ..crate::runtime::shift::relay::RelayPolicy::default()
        }
    }
}

impl RuntimeHostConfig {
    /// A config over `backend`: its effect host, attachment port,
    /// process-exec-env store and clock, with the commit budget and queued-work
    /// batching named explicitly.
    ///
    /// There is intentionally no `Default` and no in-memory constructor. The
    /// backend and the commit limits decide a runtime's durability envelope,
    /// so hosts must choose them rather than silently inheriting policy; the
    /// local durable choice is Restate over SQLite file storage (ADR 0104 §4).
    pub fn new(
        backend: crate::Backend,
        commit_budget: crate::CommitBudget,
        queued_work_batching: crate::QueuedWorkBatchingConfig,
    ) -> Self {
        let effect_host = crate::ActorContext::detached(backend.clone());
        let attachment_store = backend.attachment_store();
        let process_env_store = backend.process_env_store();
        let turn_prelude_store = backend.turn_prelude_store();
        let clock = backend.clock();
        let artifact_ports = ArtifactReferrerPorts::of_backend(&backend);
        Self {
            backend: backend.clone(),
            durability: RuntimeDurabilityConfig {
                commit_budget,
                queued_work_batching,
                attachment_store: Arc::new(crate::RuntimeAttachmentStore::ephemeral(
                    attachment_store,
                )),
                process_env_store,
                turn_prelude_store,
            },
            process_engines: ProcessEngineRegistry::new().with_artifact_ports(artifact_ports),
            providers: RuntimeProviderConfig {
                models: Arc::new(crate::EmptyLlmProfiles),
                run_definitions: crate::RunDefinitions::default(),
            },
            control: RuntimeControlConfig {
                termination: TerminationPolicy::default(),
                execution_budgets: crate::ExecutionBudgets::default(),
                delta_coalescing: DeltaCoalescing::default(),
                effect_host,
                trigger_route_restorer: None,
                process_wake_delivery_policy: crate::DeliveryPolicy::EarliestSafeBoundary,
                lease_timings: crate::LeaseTimings::default(),
                process_tool_visibility_filter: None,
                tool_source_policy: crate::ToolSourcePolicy::default(),
                scope_close: Arc::new(crate::engine::NoScopeClose),
                recovery_pass: crate::engine::RecoveryPassBudget::default(),
            },
            tracing: crate::trace::TraceRuntime::new(Arc::clone(&clock))
                .with_tool_receipts(backend.stores()),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            clock,
        }
    }

    /// The backend this config's ports come from.
    pub fn backend(&self) -> &crate::Backend {
        &self.backend
    }

    /// This config moved onto `backend`: its effect host, attachment port,
    /// process-exec-env store and clock become `backend`'s, with every other
    /// setting kept. Every backend-bound port moves together, so the config
    /// still names exactly one backend (ADR 0102, D2).
    pub fn with_backend(mut self, backend: crate::Backend) -> Self {
        let max_attachment_bytes = self.durability.attachment_store.max_attachment_bytes();
        let upload_expiry_ms = self.durability.attachment_store.upload_expiry_ms();
        let output_retention = self.durability.attachment_store.output_retention();
        self.durability.attachment_store = Arc::new(
            crate::RuntimeAttachmentStore::ephemeral(backend.attachment_store())
                .with_max_attachment_bytes(max_attachment_bytes)
                .with_read_policy(self.durability.attachment_store.read_policy())
                .with_upload_expiry_ms(upload_expiry_ms)
                .with_output_retention(output_retention),
        );
        self.durability.turn_prelude_store = backend.turn_prelude_store();
        let mut config = self
            .with_process_env_store(backend.process_env_store())
            .with_effect_host(crate::ActorContext::detached(backend.clone()))
            .with_clock(backend.clock());
        config.process_engines = config
            .process_engines
            .clone()
            .with_artifact_ports(ArtifactReferrerPorts::of_backend(&backend));
        config.tracing = config.tracing.with_tool_receipts(backend.stores());
        Self { backend, ..config }
    }

    /// The backend's deployment store: the catalog every session this
    /// runtime creates, reopens or deletes goes through.
    pub fn session_store_factory(&self) -> Arc<dyn DeploymentStore> {
        self.backend.session_store_factory()
    }

    /// The backend's trigger subscriptions and occurrences.
    pub fn trigger_store(&self) -> Arc<dyn crate::TriggerStore> {
        self.backend.trigger_store()
    }

    /// Replace the runtime time source. Hosts that need deterministic replay or
    /// test-driven time inject their own [`Clock`](super::Clock); the default is
    /// [`SystemClock`](super::SystemClock).
    ///
    pub fn with_clock(mut self, clock: Arc<dyn super::Clock>) -> Self {
        self.tracing = self.tracing.with_clock(Arc::clone(&clock));
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

    pub fn with_attachment_read_policy(mut self, policy: crate::AttachmentReadPolicy) -> Self {
        self.durability.attachment_store = Arc::new(
            self.durability
                .attachment_store
                .reconfigured_read_policy(policy),
        );
        self
    }

    /// How long an unbound put's upload edge holds its bytes before the
    /// cleanup executor may end it (ADR 0124). Every runtime this host
    /// builds, session or process, inherits it.
    pub fn with_attachment_upload_expiry_ms(mut self, upload_expiry_ms: u64) -> Self {
        let max_attachment_bytes = self.durability.attachment_store.max_attachment_bytes();
        self.durability.attachment_store = Arc::new(
            self.durability
                .attachment_store
                .reconfigured_max_attachment_bytes(max_attachment_bytes)
                .with_upload_expiry_ms(upload_expiry_ms),
        );
        self
    }

    /// The byte policy every output is measured against before it enters
    /// session history (FIG-1643): an oversized tool presentation or RLM
    /// print or final value is retained as a session attachment, and history
    /// keeps a bounded witness and its reference. The default is
    /// [`OutputRetentionPolicy::DEFAULT`](crate::OutputRetentionPolicy::DEFAULT).
    /// Each step that applies the policy journals it, so changing it never
    /// changes what a replay serves.
    pub fn with_output_retention(mut self, policy: crate::OutputRetentionPolicy) -> Self {
        self.durability.attachment_store = Arc::new(
            self.durability
                .attachment_store
                .reconfigured_output_retention(policy),
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

    /// Replace the effect host.
    pub fn with_effect_host(mut self, effect_host: ActorContext) -> Self {
        self.control.effect_host = effect_host;
        self
    }

    /// Replace the process execution-environment store.
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

/// What [`PluginHost::install_process_engine_contributions`](crate::plugin::PluginHost::install_process_engine_contributions)
/// reads from and writes to the config it installs into.
impl RuntimeHostConfig {
    pub(crate) fn install_contributed_process_engine(
        &mut self,
        registration: crate::ProcessEngineRegistration,
    ) -> Result<(), crate::PluginError> {
        self.process_engines = self.process_engines.clone().try_with_engine(registration)?;
        Ok(())
    }
}

impl RuntimeHostConfig {}

impl RuntimeHostConfig {
    pub fn with_process_observation_sink(mut self, sink: Arc<dyn TraceSink>) -> Self {
        self.tracing = self.tracing.with_product_observer(sink);
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

    /// This remains
    /// independent from the wake merge key and all batching safety gates.
    pub fn with_process_wake_delivery_policy(mut self, policy: crate::DeliveryPolicy) -> Self {
        self.control.process_wake_delivery_policy = policy;
        self
    }
}

/// Base host shape for embedded runtimes.
///
/// "Embedded" means a runtime with no process registry. Every store port it
/// reaches comes from its config's one backend (ADR 0102, D2).
#[derive(Clone)]
pub struct EmbeddedRuntimeHost {
    pub core: RuntimeHostConfig,
}

impl EmbeddedRuntimeHost {
    pub fn new(core: RuntimeHostConfig) -> Self {
        Self { core }
    }
}

/// Host shape for runtimes that support background plugin work.
#[derive(Clone)]
pub struct ProcessRuntimeHost {
    embedded: EmbeddedRuntimeHost,
    wiring: ProcessWorkWiring,
    queued_work: Arc<dyn SessionWorkEngine>,
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
        queued_work: Arc<dyn SessionWorkEngine>,
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
    pub fn queued_work(&self) -> &Arc<dyn SessionWorkEngine> {
        &self.queued_work
    }

    pub fn process_work(&self) -> &Arc<dyn ProcessWorkSubstrate> {
        self.wiring.port()
    }
}

/// A runtime's exhaustive work wiring. Process wiring owns both the registry
/// and the port that executes its work.
#[derive(Clone)]
pub enum RuntimeWork {
    SessionsOnly {
        queued: Arc<dyn SessionWorkEngine>,
    },
    Processes {
        wiring: ProcessWorkWiring,
        queued: Arc<dyn SessionWorkEngine>,
    },
}

impl RuntimeWork {
    pub fn sessions_only(queued: Arc<dyn SessionWorkEngine>) -> Self {
        Self::SessionsOnly { queued }
    }

    pub fn processes(wiring: ProcessWorkWiring, queued: Arc<dyn SessionWorkEngine>) -> Self {
        Self::Processes { wiring, queued }
    }

    pub fn queued_arc(&self) -> &Arc<dyn SessionWorkEngine> {
        match self {
            Self::SessionsOnly { queued } | Self::Processes { queued, .. } => queued,
        }
    }

    pub fn process_registry(&self) -> Option<&Arc<dyn ProcessRegistry>> {
        self.process_wiring().map(ProcessWorkWiring::registry)
    }

    pub fn process_wiring(&self) -> Option<&ProcessWorkWiring> {
        match self {
            Self::SessionsOnly { .. } => None,
            Self::Processes { wiring, .. } => Some(wiring),
        }
    }

    pub fn with_queued(self, queued: Arc<dyn SessionWorkEngine>) -> Self {
        match self {
            Self::SessionsOnly { .. } => Self::SessionsOnly { queued },
            Self::Processes { wiring, .. } => Self::Processes { wiring, queued },
        }
    }

    /// Install process wiring while retaining the queued-work port.
    pub fn with_process_wiring(self, wiring: ProcessWorkWiring) -> Self {
        let queued = Arc::clone(self.queued_arc());
        Self::Processes { wiring, queued }
    }
}

#[derive(Clone)]
pub struct RuntimeHost {
    pub core: RuntimeHostConfig,
    pub work: RuntimeWork,
}

impl RuntimeHost {
    pub fn from_embedded_with_work(embedded: EmbeddedRuntimeHost, work: RuntimeWork) -> Self {
        Self {
            core: embedded.core,
            work,
        }
    }

    pub fn process_registry(&self) -> Option<&Arc<dyn ProcessRegistry>> {
        self.work.process_registry()
    }

    pub fn process_work(&self) -> Option<&Arc<dyn ProcessWorkSubstrate>> {
        self.work.process_wiring().map(ProcessWorkWiring::port)
    }

    pub fn queued_work(&self) -> &Arc<dyn SessionWorkEngine> {
        self.work.queued_arc()
    }

    /// `policy` with the lazy binding of its recorded model. Nothing is
    /// resolved here: the binding asks this host's models only when an
    /// unjournaled model call runs (FIG-4404).
    pub fn resolve_session_policy(
        &self,
        session_id: &SessionId,
        policy: crate::SessionPolicy,
    ) -> Result<crate::RuntimeSessionPolicy, crate::SessionError> {
        self.runtime_policy(policy)
            .ok_or_else(|| crate::SessionError::LlmProfileUnconfigured {
                session_id: session_id.clone(),
            })
    }

    /// `policy` with the lazy binding of its recorded model for `owner`, its
    /// failure named by the owner. Nothing binds here. A recorded model this
    /// worker cannot bind is met by the body of the unjournaled call, as the
    /// typed, retryable
    /// [`RuntimeErrorCode::LlmProfileUnavailable`](crate::RuntimeErrorCode::LlmProfileUnavailable):
    /// the deployment is at fault, and a deployment that serves the key
    /// repairs it (FIG-4404, FIG-4531).
    pub fn resolve_owner_policy(
        &self,
        owner: &crate::RuntimeOwner,
        policy: crate::SessionPolicy,
    ) -> Result<crate::RuntimeSessionPolicy, crate::PluginError> {
        self.runtime_policy(policy).ok_or_else(|| {
            let owner = match owner {
                crate::RuntimeOwner::Session(session_id) => format!("session `{session_id}`"),
                crate::RuntimeOwner::Process(process_id) => format!("process `{process_id}`"),
            };
            crate::PluginError::Session(format!("{owner} has selected no model"))
        })
    }

    /// `None` is a policy with no model selected.
    fn runtime_policy(&self, policy: crate::SessionPolicy) -> Option<crate::RuntimeSessionPolicy> {
        crate::RuntimeSessionPolicy::new(
            policy,
            Arc::clone(&self.core.providers.models),
            Arc::clone(&self.core.clock),
        )
    }
}

impl From<EmbeddedRuntimeHost> for RuntimeHost {
    fn from(value: EmbeddedRuntimeHost) -> Self {
        Self::from_embedded_with_work(
            value,
            RuntimeWork::sessions_only(Arc::new(NoSessionWork::new())),
        )
    }
}

impl From<ProcessRuntimeHost> for RuntimeHost {
    fn from(value: ProcessRuntimeHost) -> Self {
        Self {
            core: value.embedded.core,
            work: RuntimeWork::processes(value.wiring, value.queued_work),
        }
    }
}
