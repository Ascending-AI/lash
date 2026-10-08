use crate::ActorContext;
use crate::SessionId;
use lash_trace::TraceSink;
use std::sync::Arc;

use super::process::{
    ArtifactReferrerPorts, ProcessEngineRegistry, ProcessExecutionEnvStore, ProcessRegistry,
};
use super::{DeploymentStore, ProcessWorkSubstrate, ProcessWorkWiring};

/// Required host configuration for all runtimes.
///
/// A config is built over exactly one [`Backend`](crate::Backend) (ADR 0102,
/// D2): its effect host, attachment port, process-exec-env store and clock
/// start as the backend's, and every other port a runtime reaches — the
/// session-store factory and the process-definition registry — is read from the same backend. There is no in-memory default.
#[derive(Clone)]
pub struct RuntimeHostConfig {
    /// Working budgets shared by this host's observation surfaces.
    pub observation_work_limits: lash_trace::ObservationWorkLimits,
    backend: crate::Backend,
    provider_file_uploaders: Vec<Arc<dyn crate::attachments::ProviderFileUploader>>,
    provider_file_cache: crate::attachments::ProviderFileCacheLimits,
    pub durability: RuntimeDurabilityConfig,
    pub process_engines: ProcessEngineRegistry,
    pub providers: RuntimeProviderConfig,
    pub control: RuntimeControlConfig,
    /// The runtime's one trace handle: every engine path and every plugin
    /// emits through it.
    pub tracing: crate::trace::TraceRuntime,
    /// Shared, unstable instrumentation resolved by served sessions and processes.
    #[doc(hidden)]
    pub turn_phase_probes: super::RuntimeTurnPhaseProbeSlot,
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
    pub delivery_fetch_horizon: lash_sansio::llm::attachment_delivery::DeliveryFetchHorizon,
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
    /// The frame interval of [`recommended`](Self::recommended).
    pub const RECOMMENDED_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
    /// The frame cap of [`recommended`](Self::recommended), in bytes of text.
    pub const RECOMMENDED_MAX_FRAME_BYTES: usize = 8 * 1024;
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

    /// The named preset a host may choose: frames of
    /// [`RECOMMENDED_INTERVAL`](Self::RECOMMENDED_INTERVAL) (50 ms) holding at
    /// most [`RECOMMENDED_MAX_FRAME_BYTES`](Self::RECOMMENDED_MAX_FRAME_BYTES)
    /// (8 KiB), the first delta of a block published at once. No measurement
    /// backs these values. Coalescing trades stream latency against event
    /// volume, so nothing installs this preset for a host: it is passed
    /// explicitly, as is [`off`](Self::off).
    #[must_use]
    pub const fn recommended() -> Self {
        Self {
            interval: Self::RECOMMENDED_INTERVAL,
            max_frame_bytes: Self::RECOMMENDED_MAX_FRAME_BYTES,
            first_delta_immediate: true,
        }
    }

    /// No coalescing: every delta is its own event.
    #[must_use]
    pub const fn off() -> Self {
        Self {
            interval: std::time::Duration::ZERO,
            max_frame_bytes: Self::RECOMMENDED_MAX_FRAME_BYTES,
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

#[derive(Clone)]
pub struct RuntimeControlConfig {
    /// Readable transcript copies; defaults to the named standard preset.
    pub output_cuts: lash_sansio::session_model::RuntimeOutputCuts,
    /// Optional host pool; absence uses the shared standard pool. Every model
    /// call, including direct compaction, uses the selected pool.
    pub prompt_render_pool: Option<Arc<crate::plugin::prompt::PromptRenderPool>>,
    pub effect_host: ActorContext,
    /// Every execution bound this runtime enforces (spec v3 Part C): the
    /// tool default and inline ceiling, the model call's hard total, the
    /// control-phase bound, the stop grace, the wait bounds and the provider
    /// attempt limits. The stop grace is also how long a protocol-owned
    /// stream abort (ADR 0036) keeps draining the provider stream, so a
    /// cooperative provider's trailing usage still lands on the aborted
    /// attempt; past it the attempt is sealed with a typed unreported
    /// disposition (ADR 0031). A host decision with no default:
    /// [`RuntimeHostConfig::new`] takes it.
    pub execution_budgets: crate::ExecutionBudgets,
    /// How a turn coalesces its stream deltas into frames before they reach
    /// the host sinks and the live replay store (FIG-5098). A host decision with
    /// no default: [`RuntimeHostConfig::new`] takes it.
    pub delta_coalescing: DeltaCoalescing,
    /// Optional narrow-only policy for the model-facing session process tools.
    pub process_tool_visibility_filter: Option<Arc<dyn crate::ProcessToolVisibilityFilter>>,
    /// What a turn run does when a persisted tool id no registered source
    /// resolves. Defaults to
    /// [`ToolSourcePolicy::Tolerate`](crate::ToolSourcePolicy): the run goes
    /// on and reports the typed
    /// [`ToolRestoreReport`](crate::ToolRestoreReport) as its
    /// `TurnEvent::ToolRestoreReported`. Set
    /// [`Require`](crate::ToolSourcePolicy::Require) in unattended or
    /// fixed-tool deployments to refuse, before it builds the session, a turn
    /// run that would lose a catalog member (FIG-5134). It is carried on the
    /// host config of the runtime that executes the run; a command run
    /// tolerates whatever it says.
    pub tool_source_policy: crate::ToolSourcePolicy,
    /// The host's bound on one obligation delivery (ADR 0109 §1.8). This is
    /// the one source every relay reads: [`relay_policy`](Self::relay_policy)
    /// derives the policy the leader's artifact-cleanup due pass and a
    /// producer's immediate `deliver_now` run under alike, so a delivery honors the
    /// host's bound however it is reached.
    pub recovery_pass: crate::engine::RecoveryPassBudget,
    /// Retry and claim settings for every obligation relay. The delivery budget
    /// is resolved from `recovery_pass` by [`Self::relay_policy`].
    pub relay: super::obligations::relay::RelayPolicy,
    /// Queue capacity and TTL for same-session commit attempts.
    pub commit_admission: super::CommitAdmissionPolicy,
    /// Work batches and fault retry pacing used by this runtime.
    pub pacing: super::RuntimePacingPolicy,
}

impl RuntimeControlConfig {
    /// The [`RelayPolicy`](crate::runtime::obligations::relay::RelayPolicy) every
    /// obligation relay of this runtime runs under: the configured retry
    /// and claim shape with the recovery pass's delivery budget. Both due
    /// passes and immediate producer attempts use this resolved policy.
    #[must_use]
    pub fn relay_policy(&self) -> crate::runtime::obligations::relay::RelayPolicy {
        crate::runtime::obligations::relay::RelayPolicy {
            attempt_budget_ms: self.recovery_pass.attempt_ms(),
            ..self.relay
        }
    }
}

impl RuntimeHostConfig {
    /// A config over `backend`: its effect host, attachment port,
    /// process-exec-env store and clock, with the commit budget, queued-work
    /// batching, tool-source policy, execution budgets and delta coalescing
    /// named explicitly.
    ///
    /// There is intentionally no `Default` and no in-memory constructor. The
    /// backend and the commit limits decide a runtime's durability envelope,
    /// so hosts must choose them rather than silently inheriting policy; the
    /// local durable choice is the durable engine over one SQLite database
    /// file (ADR 0132 §1).
    pub fn new(
        backend: crate::Backend,
        commit_budget: crate::CommitBudget,
        queued_work_batching: crate::QueuedWorkBatchingConfig,
        tool_source_policy: crate::ToolSourcePolicy,
        execution_budgets: crate::ExecutionBudgets,
        delta_coalescing: DeltaCoalescing,
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
            // The backend's engines are the host's: a start of one is
            // admitted on its recorded input, as an accepting registration.
            process_engines: backend
                .process_engines()
                .fold(ProcessEngineRegistry::new(), |registry, engine| {
                    registry.with_registration(crate::ProcessEngineRegistration::accepting(
                        Arc::clone(engine),
                    ))
                })
                .with_artifact_ports(artifact_ports),
            providers: RuntimeProviderConfig {
                delivery_fetch_horizon: Default::default(),
                models: Arc::new(crate::EmptyLlmProfiles),
                run_definitions: crate::RunDefinitions::default(),
            },
            control: RuntimeControlConfig {
                output_cuts: lash_sansio::session_model::RuntimeOutputCuts::standard(),
                prompt_render_pool: None,
                execution_budgets,
                delta_coalescing,
                effect_host,
                process_tool_visibility_filter: None,
                tool_source_policy,
                recovery_pass: crate::engine::RecoveryPassBudget::default(),
                relay: super::obligations::relay::RelayPolicy::standard(),
                commit_admission: super::CommitAdmissionPolicy::standard(),
                pacing: super::RuntimePacingPolicy::standard(),
            },
            tracing: crate::trace::TraceRuntime::new(Arc::clone(&clock)),
            turn_phase_probes: super::RuntimeTurnPhaseProbeSlot::default(),
            provider_file_uploaders: Vec::new(),
            provider_file_cache: Default::default(),
            observation_work_limits: lash_trace::ObservationWorkLimits::standard(),
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
            crate::RuntimeAttachmentStore::ephemeral(
                self.delivery_backend(backend.attachment_store()),
            )
            .with_max_attachment_bytes(max_attachment_bytes)
            .with_read_policy(self.durability.attachment_store.read_policy())
            .with_upload_expiry_ms(upload_expiry_ms)
            .with_output_retention(output_retention)
            .with_reclamation_retry(self.durability.attachment_store.reclamation_retry()),
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
        Self { backend, ..config }
    }

    /// The backend's deployment store: the catalog every session this
    /// runtime creates, reopens or deletes goes through.
    pub fn session_store_factory(&self) -> Arc<dyn DeploymentStore> {
        self.backend.session_store_factory()
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

    /// Install host-scoped upload infrastructure without changing attachment holders.
    pub fn with_provider_file_uploaders(
        mut self,
        uploaders: Vec<Arc<dyn crate::attachments::ProviderFileUploader>>,
    ) -> Self {
        self.provider_file_uploaders = uploaders;
        self.rewrap_delivery_backend()
    }

    /// Bound the provider-file cache the installed uploaders fill: how many
    /// files it remembers and for how long.
    pub fn with_provider_file_cache(
        mut self,
        limits: crate::attachments::ProviderFileCacheLimits,
    ) -> Self {
        self.provider_file_cache = limits;
        self.rewrap_delivery_backend()
    }
    fn rewrap_delivery_backend(mut self) -> Self {
        let backend = self.delivery_backend(self.backend.attachment_store());
        self.durability.attachment_store = Arc::new(
            self.durability
                .attachment_store
                .reconfigured_backend(backend),
        );
        self
    }
    fn delivery_backend(
        &self,
        backend: Arc<dyn crate::AttachmentStore>,
    ) -> Arc<dyn crate::AttachmentStore> {
        if self.provider_file_uploaders.is_empty() {
            backend
        } else {
            Arc::new(crate::attachments::ProviderFileDelivery::new(
                backend,
                self.provider_file_uploaders.clone(),
                self.provider_file_cache,
            ))
        }
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

    pub fn with_process_tool_visibility_filter(
        mut self,
        filter: Arc<dyn crate::ProcessToolVisibilityFilter>,
    ) -> Self {
        self.control.process_tool_visibility_filter = Some(filter);
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
}

impl ProcessRuntimeHost {
    pub fn embedded(&self) -> &EmbeddedRuntimeHost {
        &self.embedded
    }

    /// Construct a process-capable host from a registry/port wiring.
    pub fn with_ports(embedded: EmbeddedRuntimeHost, wiring: ProcessWorkWiring) -> Self {
        Self { embedded, wiring }
    }

    pub fn process_registry(&self) -> &Arc<dyn ProcessRegistry> {
        self.wiring.registry()
    }

    pub fn process_work(&self) -> &Arc<dyn ProcessWorkSubstrate> {
        self.wiring.port()
    }
}

/// A runtime's exhaustive work wiring. Process wiring owns both the registry
/// and the port that executes its work. Session work needs no port: its
/// producers wake the session actor in their own transaction (ADR 0132).
#[derive(Clone)]
pub enum RuntimeWork {
    SessionsOnly,
    Processes { wiring: ProcessWorkWiring },
}

impl RuntimeWork {
    pub fn sessions_only() -> Self {
        Self::SessionsOnly
    }

    pub fn processes(wiring: ProcessWorkWiring) -> Self {
        Self::Processes { wiring }
    }

    pub fn process_registry(&self) -> Option<&Arc<dyn ProcessRegistry>> {
        self.process_wiring().map(ProcessWorkWiring::registry)
    }

    pub fn process_wiring(&self) -> Option<&ProcessWorkWiring> {
        match self {
            Self::SessionsOnly => None,
            Self::Processes { wiring } => Some(wiring),
        }
    }

    /// Install process wiring.
    pub fn with_process_wiring(self, wiring: ProcessWorkWiring) -> Self {
        Self::Processes { wiring }
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
        Self::from_embedded_with_work(value, RuntimeWork::sessions_only())
    }
}

impl From<ProcessRuntimeHost> for RuntimeHost {
    fn from(value: ProcessRuntimeHost) -> Self {
        Self {
            core: value.embedded.core,
            work: RuntimeWork::processes(value.wiring),
        }
    }
}
