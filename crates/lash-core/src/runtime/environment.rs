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
//!   `RuntimeEnvironment::builder(crate::CommitBudget::bounded(1024 * 1024, 512), crate::QueuedWorkBatchingConfig::new(1)).build()`. The builder seeds an explicit
//!   in-memory core (`RuntimeHostConfig::in_memory`); override it with
//!   `with_runtime_host_config` for durable stores.
//! * **Long autonomous agent:** reuse the environment and let the durable
//!   store retain the session's single leaf-to-root history chain.
//! * **Webserver multi-tenant:** one `RuntimeEnvironment` per process,
//!   `park()` / `resume()` per request. HTTP connection pooling is a provider concern —
//!   provider crates accept an optional shared HTTP client in
//!   their constructors, so the host can share one pool across every
//!   materialized provider.

use std::sync::Arc;

use lash_trace::{TraceContext, TraceLevel, TraceSink};

#[cfg(test)]
use super::InlineEffectHost;
use super::host::RuntimeWork;
use super::process::ProcessRegistry;
use super::{
    EffectHost, NoQueuedWork, ProcessWorkWiring, QueuedWorkSubstrate, RuntimeHostConfig,
    TerminationPolicy,
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

    // Host-owned process lifecycle support. When present, `work` is the
    // corresponding `RuntimeWork::Processes` value before host construction.
    pub process_registry: Option<Arc<dyn ProcessRegistry>>,

    // Host-owned trigger subscription and trigger occurrence routing.
    pub trigger_store: Option<Arc<dyn crate::TriggerStore>>,

    // Store factory used by managed child sessions created from runtimes
    // built with this environment.
    pub session_store_factory: Option<Arc<dyn crate::SessionStoreFactory>>,

    pub(crate) work: RuntimeWork,

    pub core: RuntimeHostConfig,
}

impl RuntimeEnvironment {
    #[doc(hidden)]
    pub fn process_work(&self) -> Option<Arc<dyn super::ProcessWorkSubstrate>> {
        self.work
            .process_ports()
            .map(|(_, process)| Arc::clone(process))
    }

    #[doc(hidden)]
    pub fn queued_work(&self) -> Arc<dyn QueuedWorkSubstrate> {
        Arc::clone(self.work.queued_arc())
    }

    /// Construct an environment builder seeded with an in-memory host using
    /// `commit_budget`. A later
    /// [`with_runtime_host_config`](RuntimeEnvironmentBuilder::with_runtime_host_config)
    /// call replaces that host config wholesale, including its commit budget.
    pub fn builder(
        commit_budget: crate::CommitBudget,
        queued_work_batching: crate::QueuedWorkBatchingConfig,
    ) -> RuntimeEnvironmentBuilder {
        RuntimeEnvironmentBuilder::new(commit_budget, queued_work_batching)
    }
}

/// Lightweight handle returned by `LashRuntime::park`. Holds no graph
/// nodes, no plugin session, no HTTP client — just enough to
/// `LashRuntime::resume` later. Cheap to cache per-session on a
/// webserver; bounded memory cost regardless of session history size.
pub struct ParkedSession {
    pub(crate) session_id: String,
    pub(crate) store: Arc<dyn crate::store::RuntimePersistence>,
    pub(crate) policy: crate::SessionPolicy,
    pub(crate) runtime_lease_owner: crate::LeaseOwnerIdentity,
    pub(crate) runtime_lease_executor_id: String,
}

impl ParkedSession {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// Fluent builder for `RuntimeEnvironment`.
pub struct RuntimeEnvironmentBuilder {
    env: RuntimeEnvironment,
    process_registry: Option<Arc<dyn ProcessRegistry>>,
}

impl RuntimeEnvironmentBuilder {
    /// Construct an environment builder with an explicit commit budget.
    fn new(
        commit_budget: crate::CommitBudget,
        queued_work_batching: crate::QueuedWorkBatchingConfig,
    ) -> Self {
        // `RuntimeHostConfig` has no `Default`; the builder starts from an
        // explicitly named in-memory core so the choice is visible in source.
        // The `lash` facade always overrides this via `with_runtime_host_config`
        // and rejects builds that never named their stores.
        Self {
            env: RuntimeEnvironment {
                plugin_host: None,
                process_registry: None,
                trigger_store: Some(Arc::new(crate::InMemoryTriggerStore::default())),
                session_store_factory: None,
                work: RuntimeWork::sessions_only(Arc::new(NoQueuedWork::new())),
                core: RuntimeHostConfig::in_memory(commit_budget, queued_work_batching),
            },
            process_registry: None,
        }
    }
    pub fn with_plugin_host(mut self, host: Arc<crate::PluginHost>) -> Self {
        self.env.plugin_host = Some(host);
        self
    }

    pub fn with_process_registry(mut self, process_registry: Arc<dyn ProcessRegistry>) -> Self {
        self.process_registry = Some(process_registry);
        self
    }

    pub fn with_trigger_store(mut self, store: Arc<dyn crate::TriggerStore>) -> Self {
        self.env.trigger_store = Some(store);
        self
    }

    pub fn with_session_store_factory(
        mut self,
        factory: Arc<dyn crate::SessionStoreFactory>,
    ) -> Self {
        self.env.session_store_factory = Some(factory);
        self
    }

    /// Set the host's process work driver. Every `RuntimeHost` built from this
    /// environment carries it, so process starts can directly drive pending work.
    pub fn with_process_work(mut self, wiring: ProcessWorkWiring) -> Self {
        self.env.process_registry = Some(Arc::clone(wiring.registry()));
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

    pub fn with_runtime_host_config(mut self, core: RuntimeHostConfig) -> Self {
        self.env.core = core;
        self
    }

    pub fn with_attachment_store(mut self, store: Arc<dyn crate::AttachmentStore>) -> Self {
        self.env.core.durability.attachment_store =
            Arc::new(crate::SessionAttachmentStore::ephemeral(store));
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

    pub fn with_termination(mut self, termination: TerminationPolicy) -> Self {
        self.env.core.control.termination = termination;
        self
    }

    pub fn with_effect_host(mut self, effect_host: Arc<dyn EffectHost>) -> Self {
        self.env.core.control.effect_host = effect_host;
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
        let mut env = self.env;
        if env.process_registry.is_none() {
            env.process_registry = self.process_registry;
        }
        env
    }
}

impl RuntimeEnvironment {
    #[doc(hidden)]
    pub fn with_work_ports(
        mut self,
        process: Option<ProcessWorkWiring>,
        queued: Arc<dyn QueuedWorkSubstrate>,
    ) -> Self {
        self.work = match process {
            Some(wiring) => {
                self.process_registry = Some(Arc::clone(wiring.registry()));
                RuntimeWork::processes(wiring, queued)
            }
            None => {
                self.process_registry = None;
                RuntimeWork::sessions_only(queued)
            }
        };
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_methods_configure_runtime_host() {
        let attachment_store: Arc<dyn crate::AttachmentStore> =
            Arc::new(crate::InMemoryAttachmentStore::new());
        let effect_host: Arc<dyn EffectHost> = Arc::new(InlineEffectHost::default());
        let trace_context = TraceContext::default().for_session("session-1");
        let termination = TerminationPolicy {
            treat_missing_done_as_failure: false,
        };

        let env = RuntimeEnvironment::builder(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        )
        .with_attachment_store(Arc::clone(&attachment_store))
        .with_prompt_template(crate::default_prompt_template())
        .with_trace_sink(Some(Arc::new(lash_trace::JsonlTraceSink::new(
            std::env::temp_dir().join("lash-runtime-environment-builder-test.jsonl"),
        ))))
        .with_trace_level(TraceLevel::Extended)
        .with_trace_context(trace_context.clone())
        .with_termination(termination.clone())
        .with_effect_host(Arc::clone(&effect_host))
        .build();

        assert!(Arc::ptr_eq(
            env.core.durability.attachment_store.backend(),
            &attachment_store
        ));
        assert!(env.core.prompt.prompt.template.is_some());
        assert!(env.core.tracing.trace_sink.is_some());
        assert_eq!(env.core.tracing.trace_level, TraceLevel::Extended);
        assert_eq!(env.core.tracing.trace_context, trace_context);
        assert_eq!(
            env.core.control.termination.treat_missing_done_as_failure,
            termination.treat_missing_done_as_failure
        );
        assert!(Arc::ptr_eq(&env.core.control.effect_host, &effect_host));
    }

    #[test]
    fn runtime_host_config_replaces_core_config() {
        let mut core = RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
        core.tracing.trace_level = TraceLevel::Extended;
        core.control.termination = TerminationPolicy {
            treat_missing_done_as_failure: false,
        };

        let env = RuntimeEnvironment::builder(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        )
        .with_trace_level(TraceLevel::Standard)
        .with_runtime_host_config(core)
        .build();

        assert_eq!(env.core.tracing.trace_level, TraceLevel::Extended);
        assert!(!env.core.control.termination.treat_missing_done_as_failure);
    }

    #[test]
    // Architecture lint: lexical ownership guard, not behavior proof.
    fn lint_runtime_environment_does_not_mirror_runtime_host_config_fields() {
        let source = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/runtime/environment.rs"),
        )
        .expect("read environment source");
        for field in [
            ["pub ", "attachment_store:"].concat(),
            ["pub ", "prompt:"].concat(),
            ["pub ", "trace_sink:"].concat(),
            ["pub ", "trace_level:"].concat(),
            ["pub ", "trace_context:"].concat(),
            ["pub ", "termination:"].concat(),
            ["pub ", "effect_host:"].concat(),
            ["mirror ", "`RuntimeHostConfig`"].concat(),
        ] {
            assert!(
                !source.contains(&field),
                "found mirrored field/comment: {field}"
            );
        }
    }
}
