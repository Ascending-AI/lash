//! Shared process-level infrastructure for lash embedders.
//!
//! `RuntimeEnvironment` is the type an embedder constructs ONCE at
//! startup and reuses across every `LashRuntime` instance it spawns.
//! Fields are all `Arc`-wrapped or cheap-to-clone so building a runtime
//! from an environment never rebuilds expensive state (plugin host,
//! prompt layer, …).
//!
//! Three embedder patterns this enables:
//!
//! * **CLI interactive (single runtime, default):**
//!   `RuntimeEnvironment::builder(RuntimeHostConfig::new(backend, commit_budget, batching)).build()`.
//!   The host config is built over one backend, which supplies every store
//!   port and the effect host; the zero-infra backend is a SQLite memory
//!   backend (ADR 0102).
//! * **Long autonomous agent:** reuse the environment and let the durable
//!   store retain the session's single leaf-to-root history chain.
//! * **Webserver multi-tenant:** one `RuntimeEnvironment` per process,
//!   `park()` / `resume()` per request. HTTP connection pooling is a provider concern —
//!   provider crates accept an optional shared HTTP client in
//!   their constructors, so the host can share one pool across every
//!   materialized provider.

use crate::SessionId;
use std::sync::Arc;

use lash_trace::{TraceContext, TraceLevel, TraceSink};

use super::host::RuntimeWork;
use super::process::ProcessRegistry;
use super::{
    NoQueuedWork, ProcessWorkWiring, QueuedWorkSubstrate, RuntimeHostConfig, TerminationPolicy,
};

/// Shared runtime infrastructure an embedder builds once and reuses
/// across every `LashRuntime` it constructs.
///
/// Cloning is cheap — every field is either `Arc`-wrapped or small.
/// Default values build an embedded runtime without process lifecycle
/// support. Hosts that want long-running tools, async handles, subagents,
/// or process admins must provide a process registry explicitly.
#[derive(Clone)]
pub struct RuntimeEnvironment {
    // Shared plugin infrastructure. Created once; every session's
    // `PluginSession` is built from it via `PluginHost::build_session`.
    pub plugin_host: Option<Arc<crate::PluginHost>>,

    pub(crate) work: RuntimeWork,

    /// The host config and its one backend, which supplies the session-store
    /// factory, the trigger store and the process-definition registry every
    /// runtime built from this environment reaches (ADR 0102, D2).
    pub core: RuntimeHostConfig,
}

impl RuntimeEnvironment {
    /// The host-configured process registry, whether this environment is in the
    /// registry-only state or has full process work wired.
    ///
    /// `RuntimeWork` is the sole owner, so this and the runtime built from this
    /// environment cannot disagree.
    pub fn process_registry(&self) -> Option<&Arc<dyn ProcessRegistry>> {
        self.work.process_registry()
    }

    pub fn process_work(&self) -> Option<Arc<dyn super::ProcessWorkSubstrate>> {
        self.work
            .process_wiring()
            .map(|wiring| Arc::clone(wiring.port()))
    }

    pub fn queued_work(&self) -> Arc<dyn QueuedWorkSubstrate> {
        Arc::clone(self.work.queued_arc())
    }

    /// A builder over `core` and its one backend. There is no in-memory
    /// default: an environment cannot be built without a backend.
    pub fn builder(core: RuntimeHostConfig) -> RuntimeEnvironmentBuilder {
        RuntimeEnvironmentBuilder::new(core)
    }
}

/// Lightweight handle returned by `LashRuntime::park`. Holds no graph
/// nodes, no plugin session, no HTTP client — just enough to
/// `LashRuntime::resume` later. Cheap to cache per-session on a
/// webserver; bounded memory cost regardless of session history size.
pub struct ParkedSession {
    pub(crate) session_id: SessionId,
    pub(crate) store: Arc<dyn crate::store::RuntimePersistence>,
    pub(crate) policy: crate::SessionPolicy,
    pub(crate) runtime_lease_owner: crate::LeaseOwnerIdentity,
    pub(crate) runtime_lease_executor_id: String,
}

impl ParkedSession {
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}

/// Fluent builder for `RuntimeEnvironment`.
pub struct RuntimeEnvironmentBuilder {
    env: RuntimeEnvironment,
}

impl RuntimeEnvironmentBuilder {
    fn new(core: RuntimeHostConfig) -> Self {
        Self {
            env: RuntimeEnvironment {
                plugin_host: None,
                work: RuntimeWork::sessions_only(Arc::new(NoQueuedWork::new())),
                core,
            },
        }
    }
    pub fn with_plugin_host(mut self, host: Arc<crate::PluginHost>) -> Self {
        self.env.plugin_host = Some(host);
        self
    }

    /// A later [`Self::with_process_work`] replaces it with the full wiring, and vice versa:
    /// the work wiring is one owner, so setting both is last-write-wins rather than a panic.
    pub fn with_process_registry(mut self, process_registry: Arc<dyn ProcessRegistry>) -> Self {
        self.env.work = self.env.work.with_process_registry(process_registry);
        self
    }

    /// Every `RuntimeHost` built from this environment carries it, so process starts can
    /// directly drive pending work.
    /// This replaces a registry-only state configured by [`Self::with_process_registry`]; the
    /// wiring carries its own registry.
    pub fn with_process_work(mut self, wiring: ProcessWorkWiring) -> Self {
        self.env.work = self.env.work.with_process_wiring(wiring);
        self
    }

    pub fn with_queued_work(mut self, queued: Arc<dyn QueuedWorkSubstrate>) -> Self {
        self.env.work = self.env.work.with_queued(queued);
        self
    }

    pub fn with_process_tool_visibility_filter(
        mut self,
        filter: Arc<dyn crate::ProcessToolVisibilityFilter>,
    ) -> Self {
        self.env.core.control.process_tool_visibility_filter = Some(filter);
        self
    }

    pub fn with_prompt_template(mut self, template: crate::PromptTemplate) -> Self {
        self.env.core.prompt.prompt.template = Some(template);
        self
    }

    pub fn with_prompt_contribution(mut self, contribution: crate::PromptContribution) -> Self {
        self.env.core.prompt.prompt.add_contribution(contribution);
        self
    }

    pub fn with_replaced_prompt_slot(
        mut self,
        slot: crate::PromptSlot,
        contributions: impl IntoIterator<Item = crate::PromptContribution>,
    ) -> Self {
        self.env
            .core
            .prompt
            .prompt
            .replace_slot(slot, contributions);
        self
    }

    pub fn with_cleared_prompt_slot(mut self, slot: crate::PromptSlot) -> Self {
        self.env.core.prompt.prompt.clear_slot(slot);
        self
    }

    pub fn with_prompt_layer(mut self, prompt: crate::PromptLayer) -> Self {
        self.env.core.prompt.prompt = prompt;
        self
    }

    pub fn with_trace_sink(mut self, sink: Option<Arc<dyn TraceSink>>) -> Self {
        self.env.core.tracing.trace_sink = sink;
        self
    }

    pub fn with_trace_level(mut self, level: TraceLevel) -> Self {
        self.env.core.tracing.trace_level = level;
        self
    }

    pub fn with_trace_context(mut self, context: TraceContext) -> Self {
        self.env.core.tracing.trace_context = context;
        self
    }

    /// See [`crate::ToolSourcePolicy`]; the default is `Tolerate`.
    pub fn with_tool_source_policy(mut self, policy: crate::ToolSourcePolicy) -> Self {
        self.env.core.control.tool_source_policy = policy;
        self
    }

    /// See [`crate::ToolSurfaceOpenMode`]; the default is `Reconcile`.
    pub fn with_tool_surface_open_mode(mut self, mode: crate::ToolSurfaceOpenMode) -> Self {
        self.env.core.control.tool_surface_open_mode = mode;
        self
    }

    pub fn with_termination(mut self, termination: TerminationPolicy) -> Self {
        self.env.core.control.termination = termination;
        self
    }

    /// Bound the provider-stream drain after a protocol-owned abort. See
    /// [`crate::RuntimeControlConfig::abort_drain_grace`].
    pub fn with_abort_drain_grace(mut self, grace: std::time::Duration) -> Self {
        self.env.core.control.abort_drain_grace = grace;
        self
    }

    pub fn with_provider_resolver(
        mut self,
        provider_resolver: Arc<dyn crate::RuntimeProviderResolver>,
    ) -> Self {
        self.env.core.providers.provider_resolver = provider_resolver;
        self
    }

    pub fn build(self) -> RuntimeEnvironment {
        self.env
    }
}

impl RuntimeEnvironment {
    pub fn with_work_ports(
        mut self,
        process: Option<ProcessWorkWiring>,
        queued: Arc<dyn QueuedWorkSubstrate>,
    ) -> Self {
        self.work = self.work.with_work_ports(process, queued);
        self
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // FIG-2971: test module is a host; ambient fs/env/process access is sanctioned
mod tests {
    use super::*;

    fn core_over(backend: &Arc<dyn crate::Backend>) -> RuntimeHostConfig {
        RuntimeHostConfig::new(
            Arc::clone(backend),
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        )
    }

    #[tokio::test]
    async fn builder_methods_configure_runtime_host() {
        let backend = crate::testing::memory_backend().await;
        let trace_context = TraceContext::default().for_session("session-1");
        let termination = TerminationPolicy {
            treat_missing_done_as_failure: false,
        };

        let env = RuntimeEnvironment::builder(core_over(&backend))
            .with_prompt_template(crate::default_prompt_template())
            .with_trace_sink(Some(Arc::new(lash_trace::JsonlTraceSink::new(
                std::env::temp_dir().join("lash-runtime-environment-builder-test.jsonl"),
            ))))
            .with_trace_level(TraceLevel::Extended)
            .with_trace_context(trace_context.clone())
            .with_termination(termination.clone())
            .build();

        assert!(env.core.prompt.prompt.template.is_some());
        assert!(env.core.tracing.trace_sink.is_some());
        assert_eq!(env.core.tracing.trace_level, TraceLevel::Extended);
        assert_eq!(env.core.tracing.trace_context, trace_context);
        assert_eq!(
            env.core.control.termination.treat_missing_done_as_failure,
            termination.treat_missing_done_as_failure
        );
    }

    /// An environment has no store port of its own: every port a runtime
    /// built from it reaches is its config's backend's (ADR 0102, D2), and
    /// nothing falls back to an in-memory store.
    #[tokio::test]
    async fn every_store_port_is_the_config_backends() {
        let backend = crate::testing::memory_backend().await;
        let env = RuntimeEnvironment::builder(core_over(&backend)).build();

        assert!(Arc::ptr_eq(
            &env.core.control.effect_host,
            &backend.effect_host()
        ));
        assert!(Arc::ptr_eq(
            env.core.durability.attachment_store.backend(),
            &backend.attachment_store()
        ));
        assert!(Arc::ptr_eq(
            &env.core.durability.process_env_store,
            &backend.process_env_store()
        ));
        assert!(Arc::ptr_eq(
            &env.core.session_store_factory(),
            &backend.session_store_factory()
        ));
        assert!(Arc::ptr_eq(
            &env.core.trigger_store(),
            &backend.trigger_store()
        ));
        assert!(Arc::ptr_eq(
            &env.core.process_definitions(),
            &backend.process_definition_registry()
        ));
    }

    fn registry_only_environment(
        backend: &Arc<dyn crate::Backend>,
        registry: &Arc<dyn ProcessRegistry>,
    ) -> RuntimeEnvironment {
        RuntimeEnvironment::builder(core_over(backend))
            .with_process_registry(Arc::clone(registry))
            .build()
    }

    /// Rebinding work ports without a process wiring must not silently drop a
    /// registry the host configured: the registry-only state is a state of the
    /// work wiring, not a field that `with_work_ports` is free to clear.
    #[tokio::test]
    async fn rebinding_work_ports_without_a_wiring_keeps_a_registry_only_registry() {
        let backend = crate::testing::memory_backend().await;
        let registry = backend.process_registry();
        let env = registry_only_environment(&backend, &registry);
        assert!(
            env.process_registry().is_some(),
            "a registry-only environment starts with its registry"
        );

        let rebound = env.with_work_ports(None, Arc::new(NoQueuedWork::new()));

        let kept = rebound
            .process_registry()
            .expect("rebinding work ports without a wiring keeps the registry");
        assert!(
            Arc::ptr_eq(kept, &registry),
            "the kept registry is the one the host configured"
        );
        assert!(
            rebound.process_work().is_none(),
            "no process-work port is invented by keeping the registry"
        );
    }

    /// A runtime built from a registry-only environment must report the same
    /// registry the environment does. The host is assembled from `env.work`
    /// alone (`LashRuntime::from_environment_for_executor`), so a registry that
    /// does not live in `work` never reaches the runtime.
    #[tokio::test]
    async fn a_host_built_from_a_registry_only_environment_reports_that_registry() {
        let backend = crate::testing::memory_backend().await;
        let registry = backend.process_registry();
        let env = registry_only_environment(&backend, &registry);

        let host = super::super::host::RuntimeHost::from_embedded_with_work(
            super::super::host::EmbeddedRuntimeHost::new(env.core.clone()),
            env.work.clone(),
        );

        let observed = host
            .process_registry()
            .expect("the runtime host carries the environment's registry");
        assert!(
            Arc::ptr_eq(observed, &registry),
            "the environment and the runtime it builds answer the registry question the same way"
        );
    }

    /// The trigger store is the backend's, stamping from the backend's clock.
    #[tokio::test]
    async fn the_trigger_store_stamps_from_the_backend_clock() {
        const NOW_MS: u64 = 4_200_000;
        let clock: Arc<dyn crate::Clock> = Arc::new(crate::testing::TestClock::new(NOW_MS));
        let backend: Arc<dyn crate::Backend> = Arc::new(
            lash_sqlite_store::SqliteBackend::memory_with_clock(Arc::clone(&clock))
                .await
                .expect("open a SQLite memory backend"),
        );

        let env = RuntimeEnvironment::builder(core_over(&backend)).build();
        let receipt = env
            .core
            .trigger_store()
            .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
                "fig1982.clock",
                "resolved-core-clock",
                serde_json::Value::Null,
                "fig1982:resolved-core-clock",
            ))
            .await
            .expect("ingest clock probe");
        assert_eq!(receipt.occurrence.occurred_at_ms, NOW_MS);
    }
}
