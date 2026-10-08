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
use super::{ProcessWorkWiring, RuntimeHostConfig, TerminationPolicy};

/// Shared runtime infrastructure an embedder builds once and reuses
/// across every `LashRuntime` it constructs.
///
/// Cloning is cheap — every field is either `Arc`-wrapped or small.
/// Default values build an embedded runtime without process lifecycle
/// support. Hosts that want long-running tools, async handles, child
/// sessions or process admins must provide complete process work wiring.
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

/// A park or close [`LashRuntime::park`] did not complete (FIG-4202). It
/// hands the runtime back with its resident state and its pending usage as
/// they were, so nothing the runtime held is lost: the host keeps using it,
/// or parks it again.
pub struct ParkRefused {
    pub runtime: Box<crate::LashRuntime>,
    pub error: Box<crate::SessionError>,
}

impl ParkRefused {
    /// The shift that owns the session head, when the refusal is a busy
    /// one: a dirty park's flush met a bound run, an owed follow-on or an
    /// open session command (FIG-4202). The same park succeeds once that
    /// owner's boundary passes.
    #[must_use]
    pub fn busy_owner(&self) -> Option<&crate::store::SessionHeadOwner> {
        match self.error.as_ref() {
            crate::SessionError::Store {
                source: crate::StoreError::SessionHeadOwned { owner, .. },
                ..
            } => Some(owner),
            _ => None,
        }
    }
}

impl std::fmt::Debug for ParkRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ParkRefused")
            .field("session_id", &self.runtime.session_id())
            .field("error", &self.error)
            .finish()
    }
}

impl From<ParkRefused> for crate::SessionError {
    fn from(refused: ParkRefused) -> Self {
        *refused.error
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
                work: RuntimeWork::sessions_only(),
                core,
            },
        }
    }
    pub fn with_plugin_host(mut self, host: Arc<crate::PluginHost>) -> Self {
        self.env.plugin_host = Some(host);
        self
    }

    /// Every runtime built from this environment carries the wiring's registry
    /// and process-work port, so process starts can work pending work.
    pub fn with_process_work(mut self, wiring: ProcessWorkWiring) -> Self {
        self.env.work = self.env.work.with_process_wiring(wiring);
        self
    }

    pub fn with_process_tool_visibility_filter(
        mut self,
        filter: Arc<dyn crate::ProcessToolVisibilityFilter>,
    ) -> Self {
        self.env.core.control.process_tool_visibility_filter = Some(filter);
        self
    }

    pub fn with_trace_sink(mut self, sink: Option<Arc<dyn TraceSink>>) -> Self {
        self.env.core.tracing = self.env.core.tracing.clone().with_trace_sinks(sink);
        self
    }

    pub fn with_trace_level(mut self, level: TraceLevel) -> Self {
        self.env.core.tracing = self.env.core.tracing.clone().with_level(level);
        self
    }

    pub fn with_trace_context(mut self, context: TraceContext) -> Self {
        self.env.core.tracing = self.env.core.tracing.clone().with_base_context(context);
        self
    }

    /// See [`crate::ToolSourcePolicy`]; the default is `Tolerate`.
    pub fn with_tool_source_policy(mut self, policy: crate::ToolSourcePolicy) -> Self {
        self.env.core.control.tool_source_policy = policy;
        self
    }

    pub fn with_termination(mut self, termination: TerminationPolicy) -> Self {
        self.env.core.control.termination = termination;
        self
    }

    /// Every execution bound the runtime enforces. See
    /// [`crate::RuntimeControlConfig::execution_budgets`].
    pub fn with_execution_budgets(mut self, budgets: crate::ExecutionBudgets) -> Self {
        self.env.core.control.execution_budgets = budgets;
        self
    }

    /// The host's models: the registry that mints model bindings and binds
    /// recorded ones to their transports.
    pub fn with_llm_profiles(mut self, models: Arc<dyn crate::LlmProfiles>) -> Self {
        self.env.core.providers.models = models;
        self
    }

    pub fn build(self) -> RuntimeEnvironment {
        self.env
    }
}

impl RuntimeEnvironment {
    pub fn with_work_ports(mut self, process: ProcessWorkWiring) -> Self {
        self.work = RuntimeWork::processes(process);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trigger store is the backend's, stamping from the backend's clock.
    #[tokio::test]
    async fn the_trigger_store_stamps_from_the_backend_clock() {
        const NOW_MS: u64 = 4_200_000;
        let clock = Arc::new(crate::testing::TestClock::new(NOW_MS));
        let stores = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
            .await
            .expect("open a SQLite memory store set");
        let backend = lash_conformance::backend_over(Arc::new(stores));

        let env = RuntimeEnvironment::builder(RuntimeHostConfig::new(
            backend,
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        ))
        .build();
        let plan = env
            .core
            .trigger_store()
            .plan_occurrence(&crate::TriggerOccurrenceRequest::new(
                "fig1982.clock",
                "resolved-core-clock",
                serde_json::Value::Null,
                "fig1982:resolved-core-clock",
            ))
            .await
            .expect("plan clock probe");
        let crate::TriggerOccurrencePlan::Fresh { occurrence, .. } = plan else {
            panic!("a fresh store holds no occurrence: {plan:?}");
        };
        assert_eq!(occurrence.occurred_at_ms, NOW_MS);
    }
}
