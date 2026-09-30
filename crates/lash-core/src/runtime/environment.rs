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
    NoSessionWork, ProcessWorkWiring, RuntimeHostConfig, SessionWorkEngine, TerminationPolicy,
};

/// Shared runtime infrastructure an embedder builds once and reuses
/// across every `LashRuntime` it constructs.
///
/// Cloning is cheap — every field is either `Arc`-wrapped or small.
/// Default values build an embedded runtime without process lifecycle
/// support. Hosts that want long-running tools, async handles, subagents,
/// or process admins must provide complete process work wiring.
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
    /// The registry carried by the host-configured process work wiring.
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

    pub fn queued_work(&self) -> Arc<dyn SessionWorkEngine> {
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
    pub(crate) store: crate::store::SessionStore,
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
                work: RuntimeWork::sessions_only(Arc::new(NoSessionWork::new())),
                core,
            },
        }
    }
    pub fn with_plugin_host(mut self, host: Arc<crate::PluginHost>) -> Self {
        self.env.plugin_host = Some(host);
        self
    }

    /// Every runtime built from this environment carries the wiring's registry
    /// and process-work port, so process starts can drive pending work.
    pub fn with_process_work(mut self, wiring: ProcessWorkWiring) -> Self {
        self.env.work = self.env.work.with_process_wiring(wiring);
        self
    }

    pub fn with_queued_work(mut self, queued: Arc<dyn SessionWorkEngine>) -> Self {
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
        process: ProcessWorkWiring,
        queued: Arc<dyn SessionWorkEngine>,
    ) -> Self {
        self.work = RuntimeWork::processes(process, queued);
        self
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // FIG-2971: test module is a host; ambient fs/env/process access is sanctioned
mod tests {
    use super::*;

    fn core_over(backend: &crate::Backend) -> RuntimeHostConfig {
        RuntimeHostConfig::new(
            backend.clone(),
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        )
    }

    #[tokio::test]
    async fn builder_methods_configure_runtime_host() {
        let backend = crate::testing::memory_store_backend().await;
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
        let backend = crate::testing::memory_store_backend().await;
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

    #[tokio::test]
    async fn sessions_only_environment_and_host_have_no_process_ports() {
        let backend = crate::testing::memory_store_backend().await;
        let queued: Arc<dyn SessionWorkEngine> = Arc::new(NoSessionWork::new());
        let env = RuntimeEnvironment::builder(core_over(&backend))
            .with_queued_work(Arc::clone(&queued))
            .build();
        let host = super::super::host::RuntimeHost::from_embedded_with_work(
            super::super::host::EmbeddedRuntimeHost::new(env.core.clone()),
            env.work.clone(),
        );

        assert!(env.process_registry().is_none());
        assert!(env.process_work().is_none());
        assert!(host.process_registry().is_none());
        assert!(host.process_work().is_none());
        assert!(Arc::ptr_eq(&env.queued_work(), &queued));
        assert!(Arc::ptr_eq(host.queued_work(), &queued));
    }

    #[tokio::test]
    async fn rebinding_work_ports_replaces_the_registry_and_both_ports() {
        let backend = crate::testing::memory_store_backend().await;
        let env = RuntimeEnvironment::builder(core_over(&backend))
            .with_process_work(backend.process_work())
            .build();
        let replacement = crate::testing::memory_store_backend().await;
        let wiring = replacement.process_work();
        let queued: Arc<dyn SessionWorkEngine> = Arc::new(NoSessionWork::new());
        assert!(!Arc::ptr_eq(
            env.process_registry().expect("initial registry"),
            wiring.registry(),
        ));

        let rebound = env.with_work_ports(wiring.clone(), Arc::clone(&queued));

        assert!(Arc::ptr_eq(
            rebound.process_registry().expect("replacement registry"),
            wiring.registry(),
        ));
        assert!(Arc::ptr_eq(
            &rebound.process_work().expect("replacement process port"),
            wiring.port(),
        ));
        assert!(Arc::ptr_eq(&rebound.queued_work(), &queued));
    }

    #[tokio::test]
    async fn a_host_built_from_an_environment_carries_its_complete_process_wiring() {
        let backend = crate::testing::memory_store_backend().await;
        let wiring = backend.process_work();
        let queued: Arc<dyn SessionWorkEngine> = Arc::new(NoSessionWork::new());
        let env = RuntimeEnvironment::builder(core_over(&backend))
            .with_process_work(wiring.clone())
            .with_queued_work(Arc::clone(&queued))
            .build();
        let host = super::super::host::RuntimeHost::from_embedded_with_work(
            super::super::host::EmbeddedRuntimeHost::new(env.core.clone()),
            env.work.clone(),
        );

        assert!(Arc::ptr_eq(
            host.process_registry().expect("host registry"),
            env.process_registry().expect("environment registry"),
        ));
        assert!(Arc::ptr_eq(
            host.process_work().expect("host process port"),
            wiring.port(),
        ));
        assert!(Arc::ptr_eq(host.queued_work(), &queued));
    }

    #[tokio::test]
    async fn embedded_builder_keeps_queued_work_in_both_setter_orders() {
        let backend = crate::testing::memory_store_backend().await;
        let core = crate::testing::runtime_helpers::test_host_config(&backend).core;
        let wiring = backend.process_work();
        let queued: Arc<dyn SessionWorkEngine> = Arc::new(NoSessionWork::new());
        for process_first in [false, true] {
            let builder = crate::runtime::EmbeddedRuntimeBuilder::new(
                core.clone(),
                crate::testing::runtime_lease_owner(),
            )
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
            .with_policy(crate::testing::standard_test_policy());
            let builder = if process_first {
                builder
                    .with_process_work(wiring.clone())
                    .with_queued_work(Arc::clone(&queued))
            } else {
                builder
                    .with_queued_work(Arc::clone(&queued))
                    .with_process_work(wiring.clone())
            };
            let runtime = Box::pin(builder.build())
                .await
                .expect("build process runtime");
            assert!(Arc::ptr_eq(
                runtime.host.process_registry().expect("builder registry"),
                wiring.registry(),
            ));
            assert!(Arc::ptr_eq(
                runtime.host.process_work().expect("builder process port"),
                wiring.port(),
            ));
            assert!(Arc::ptr_eq(runtime.host.queued_work(), &queued));
        }
        let runtime = Box::pin(
            crate::runtime::EmbeddedRuntimeBuilder::new(
                core.clone(),
                crate::testing::runtime_lease_owner(),
            )
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
            .with_policy(crate::testing::standard_test_policy())
            .with_queued_work(Arc::clone(&queued))
            .build(),
        )
        .await
        .expect("build sessions-only runtime");
        assert!(runtime.host.process_registry().is_none());
        assert!(runtime.host.process_work().is_none());
        assert!(Arc::ptr_eq(runtime.host.queued_work(), &queued));
    }

    /// The trigger store is the backend's, stamping from the backend's clock.
    #[tokio::test]
    async fn the_trigger_store_stamps_from_the_backend_clock() {
        const NOW_MS: u64 = 4_200_000;
        let double = crate::testing::kernel_double(
            0xf6_0003,
            lash_restate_test::ServerConfig {
                start_time_ms: NOW_MS,
                time: lash_restate_test::TimeMode::Manual,
                ..Default::default()
            },
        )
        .await;
        let backend = double.lash_backend();

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
