mod durable_host;

pub(crate) use durable_host::{Candidates, DurableHost};

use std::sync::Arc;

use lash_core::core_internal::RuntimeExecutionContextRuntimeOps as _;

std::thread_local! {
    /// The store sets the running test opened, held as its backends are.
    static TEST_STORE_SETS: std::cell::RefCell<Vec<std::sync::Arc<lash_sqlite_store::SqliteStoreSet>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh SQLite memory store set, storage only (no engine), held for the
/// rest of the running test.
pub(crate) async fn sqlite_memory_store_set() -> std::sync::Arc<lash_sqlite_store::SqliteStoreSet> {
    let stores = std::sync::Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    TEST_STORE_SETS.with(|held| held.borrow_mut().push(std::sync::Arc::clone(&stores)));
    lash_execution_contracts::testing::process_execution_env_fixture(
        stores.process_env_store().as_ref(),
    )
    .await;
    stores
}

/// The scope a context built with no parent invocation claims: the builder's
/// default test turn. Open the [`DurableHost`] whose context serves it for it.
pub(crate) fn default_cell_scope() -> lash_core::AdmittedScope {
    lash_core::AdmittedScope::turn(
        lash_vocabulary::identity::SessionId::from("test-session"),
        lash_vocabulary::identity::TurnId::from("test-turn"),
    )
}

/// The render a session records at creation: the builtin renderer under the
/// default parameters.
pub(crate) fn recorded_test_render() -> lash_core::RecordedRender {
    lash_core::RecordedRender {
        renderer_id: crate::render::CodeRendererSlot::default()
            .0
            .id()
            .to_string(),
        params: serde_json::to_value(crate::render::ResolvedRlmRender::default())
            .expect("test render params serialize")
            .into(),
    }
}

/// A session's execution state for `dialect`, staging its pages in the
/// test host's process-private spools.
pub(crate) fn session_state(
    dialect: &crate::CellDialect,
) -> crate::executor::CodeModeExecutionState {
    static SPOOLS: std::sync::LazyLock<Arc<lash_vm_broker::kernel::pages::SpoolRoot>> =
        std::sync::LazyLock::new(|| {
            let mut config = lash_vm_client::service::Service::default().config().clone();
            #[expect(
                clippy::disallowed_methods,
                reason = "the test host explicitly chooses a process-private staging directory"
            )]
            let dir =
                std::env::temp_dir().join(format!("lash-rlm-cell-laws-{}", std::process::id()));
            config.staging_dir = Some(dir);
            lash_vm_client::service::Service::new(config)
                .spools()
                .expect("open the test host's staging directory")
        });
    crate::executor::CodeModeExecutionState::new(dialect.name(), dialect.numbers())
        .staging(Ok(Arc::clone(&SPOOLS)))
}

/// What a cell of `dialect` needs of its session, with no bounds and the
/// builtin renderer, on the cell channel.
pub(crate) fn cell_services(
    dialect: &crate::CellDialect,
    workers: lash_vm_client::service::Service,
) -> crate::executor::CellServices {
    crate::executor::CellServices {
        events: Arc::default(),
        workers,
        execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
        channel: crate::plugin::RlmChannel::Cell,
        code_renderer: crate::render::CodeRendererSlot::default(),
        prompts: dialect.prompts(),
        kernel: crate::executor::KernelCarry::default(),
    }
}

/// Run one cell through the production executor entry and settle it as
/// its turn would: accepted.
mod packages;

pub(crate) async fn run_cell(
    state: &mut crate::executor::CodeModeExecutionState,
    ctx: lash_core::RuntimeExecutionContext<'_>,
    services: &crate::executor::CellServices,
    code: &str,
) -> lash_core::ExecResponse {
    packages::show(state, &ctx).await;
    let response = Box::pin(crate::executor::execute_cell(
        state,
        ctx.with_recorded_render(recorded_test_render()),
        lash_core::ExecRequest {
            code: code.to_string(),
        },
        services,
        crate::projection::CodeModeProjectedBindings::default(),
    ))
    .await;
    if !response.suspended {
        state.mark_code_execution_response_returned();
        state.accept_code_execution();
    }
    response
}

/// Captures `state` as its turn's working state and commits it on `host`,
/// as the turn phase commit after a cell would, the capture acknowledged.
/// Answers the capture and how many pages its commit published.
pub(crate) async fn commit_working(
    state: &mut crate::executor::CodeModeExecutionState,
    host: &DurableHost,
) -> (lash_core_store::execution_state::ExecutionGraphCapture, u64) {
    let capture = state
        .capture_execution_graph(lash_core_store::execution_state::ExecutionGraphPurpose::Working)
        .await
        .expect("the session's state captures");
    let published = host.commit_working(&capture, state).await;
    state.acknowledge_execution_state_capture().await;
    (capture, published)
}

/// The turn's phase commit after a settled cell of `state`'s session: the
/// session's state as its working state, which the next cell reads.
pub(crate) async fn commit_phase(
    state: &crate::plugin::runtime_state::RlmRuntimeState,
    host: &DurableHost,
) {
    let capture = state
        .capture_execution_graph(lash_core::plugin::ExecutionGraphPurpose::Working)
        .await
        .expect("the session's state captures");
    host.commit_working(&capture, &mut Executor(state)).await;
    state.acknowledge_execution_state_capture().await;
}

/// The session's executor as a working commit publishes its pages.
struct Executor<'a>(&'a crate::plugin::runtime_state::RlmRuntimeState);

#[async_trait::async_trait]
impl Candidates for Executor<'_> {
    async fn candidate(
        &mut self,
        root: lash_core_store::checkpoint_graph::NodeId,
        commit: lash_core_store::checkpoint_graph::CommitId,
        tx: &mut lash_durable::ActorTx,
    ) -> Option<lash_core::plugin::ExecutionCandidate> {
        self.0
            .execution_candidate(root, commit, tx)
            .await
            .expect("the capture's candidate builds")
    }
}

/// The context of the cell `replay_key` of `session`'s turn `turn`, with
/// `provider`'s tools as the cell's catalog.
pub(crate) fn cell_context(
    host: &DurableHost,
    session: &'static str,
    turn: &'static str,
    replay_key: &'static str,
    provider: Arc<dyn lash_core::ToolProvider>,
) -> lash_core::RuntimeExecutionContext<'static> {
    let (provider, catalog) = with_finish(provider);
    lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
        host.ports(),
        provider,
        catalog,
        lash_core::testing::exec_code_invocation(session, turn, 0, 0, "exec-code", replay_key),
    )
    .with_session_pages(host.session_pages())
}

/// A provider that offers no tool of its own.
pub(crate) struct NoTools;

#[async_trait::async_trait]
impl lash_core::ToolProvider for NoTools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(Vec::new())
    }

    fn resolve_contract(
        &self,
        _name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        None
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::err_fmt(format_args!("Unknown tool: {}", call.name())).into()
    }
}

/// `provider`'s tools and Lash's `control.finish`, which a law's cell ends
/// with to answer a value.
struct WithFinish {
    tools: Arc<dyn lash_core::ToolProvider>,
    finish: lash_vocabulary::tool_contract::ToolDefinition,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for WithFinish {
    fn tool_manifests(
        &self,
        config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok({
            let mut manifests = self.tools.tool_manifests(config)?;
            manifests.push(self.finish.manifest());
            manifests
        })
    }

    fn resolve_manifest_by_id(
        &self,
        id: &lash_vocabulary::tool_contract::ToolId,
        config: &lash_core::OwnerConfigView,
    ) -> Option<lash_vocabulary::tool_contract::ToolManifest> {
        let finish = self.finish.manifest();
        if &finish.id == id {
            return Some(finish);
        }
        self.tools.resolve_manifest_by_id(id, config)
    }

    fn resolve_contract(
        &self,
        name: &str,
        config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        let finish = self.finish.manifest();
        if name == finish.tool() || name == finish.id.as_str() {
            return Some(Arc::new(self.finish.contract()));
        }
        self.tools.resolve_contract(name, config)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() == crate::FINISH_TOOL_NAME {
            return lash_core::ToolOutcome::finish(call.args.clone()).into();
        }
        self.tools.execute(call).await
    }
}

/// `provider` with `control.finish` beside its tools, and the catalog of
/// both.
pub(crate) fn with_finish(
    provider: Arc<dyn lash_core::ToolProvider>,
) -> (Arc<dyn lash_core::ToolProvider>, lash_core::ToolCatalog) {
    let provider: Arc<dyn lash_core::ToolProvider> = Arc::new(WithFinish {
        tools: provider,
        finish: crate::finish_tool_definition(&crate::dialect::TypescriptPrompts),
    });
    let catalog = lash_core::ToolCatalog::from_tool_definitions(
        provider
            .tool_manifests(&lash_core::OwnerConfigView::default())
            .expect("fixture provider advertises")
            .into_iter()
            .filter_map(|manifest| {
                let contract = provider
                    .resolve_contract(&manifest.tool(), &lash_core::OwnerConfigView::default())?;
                Some(lash_vocabulary::tool_contract::ToolDefinition::from_parts(
                    manifest,
                    contract.as_ref().clone(),
                ))
            })
            .collect(),
    );
    (provider, catalog)
}

/// The name of [`python_worker_entry`], as libtest spells it.
const PYTHON_WORKER_ENTRY: &str = "testing::python_worker_entry";

/// What the worker of a host that names Python alone runs: the kernel
/// library and the Python dialect, through the builder every host uses.
fn python_embedding(
    _tuning: &lash_vm_client::WorkerTuning,
) -> Result<lash_vm_worker::Embedder, lash_vm_worker::EmbedError> {
    lash_vm_worker::Embedder::new().dialect(lash_vm_worker::python())
}

/// The worker entry of [`python_workers`]: a pool execs this test binary
/// with this one test selected, and the test becomes the worker. Run by a
/// test runner, it is no worker and passes.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "a worker's process ends when its pool closes it, not when libtest returns"
)]
fn python_worker_entry() {
    if lash_vm_worker::worker_entry(&python_embedding).expect("serve as a Python worker") {
        std::process::exit(0);
    }
}

/// A worker service whose workers have the Python dialect installed: this
/// test binary, entered at [`python_worker_entry`].
pub(crate) fn python_workers() -> lash_vm_client::service::Service {
    let mut entry = lash_vm_client::WorkerEntry::reexec().expect("this test binary's path");
    entry.args = vec![
        "--exact".to_owned(),
        "--nocapture".to_owned(),
        "--test-threads=1".to_owned(),
        "--".to_owned(),
        PYTHON_WORKER_ENTRY.to_owned(),
    ];
    lash_vm_client::service::Service::new(lash_vm_client::PoolConfig::codemode(entry))
}

/// A front end that lowers as the one it wraps does and keeps no map of
/// where the statements came from. Nothing a run or a save needs may be
/// missing from such a cell.
struct Unmapped(Box<dyn lash_kernel_dialect::FrontEnd>);

impl lash_kernel_dialect::FrontEnd for Unmapped {
    fn tool_calls(
        &self,
        source: &str,
        environment: &lash_kernel_dialect::Environment<'_>,
        cell: lash_kernel_dialect::CellEnvironment<'_>,
    ) -> Result<std::collections::BTreeSet<String>, lash_kernel_dialect::Diagnostic> {
        self.0.tool_calls(source, environment, cell)
    }

    fn lower(
        &self,
        source: &str,
        environment: &lash_kernel_dialect::Environment<'_>,
        mode: lash_kernel_dialect::Mode<'_>,
    ) -> Result<lash_kernel_dialect::Lowered, lash_kernel_dialect::Diagnostic> {
        let mut lowered = self.0.lower(source, environment, mode)?;
        lowered.source_map = None;
        Ok(lowered)
    }
}

/// The name of [`unmapped_worker_entry`], as libtest spells it.
const UNMAPPED_WORKER_ENTRY: &str = "testing::unmapped_worker_entry";

/// Both dialects, each lowering with no source map.
fn unmapped_embedding(
    tuning: &lash_vm_client::WorkerTuning,
) -> Result<lash_vm_worker::Embedder, lash_vm_worker::EmbedError> {
    let mut typescript = lash_vm_worker::typescript(tuning)?;
    typescript.front_end = Box::new(Unmapped(typescript.front_end));
    let mut python = lash_vm_worker::python();
    python.front_end = Box::new(Unmapped(python.front_end));
    lash_vm_worker::Embedder::new()
        .dialect(typescript)?
        .dialect(python)
}

/// The worker entry of [`unmapped_workers`], as [`python_worker_entry`] is
/// of [`python_workers`].
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "a worker's process ends when its pool closes it, not when libtest returns"
)]
fn unmapped_worker_entry() {
    if lash_vm_worker::worker_entry(&unmapped_embedding).expect("serve as an unmapped worker") {
        std::process::exit(0);
    }
}

/// A worker service whose workers lower both dialects with no source map:
/// this test binary, entered at [`unmapped_worker_entry`].
pub(crate) fn unmapped_workers() -> lash_vm_client::service::Service {
    let mut entry = lash_vm_client::WorkerEntry::reexec().expect("this test binary's path");
    entry.args = vec![
        "--exact".to_owned(),
        "--nocapture".to_owned(),
        "--test-threads=1".to_owned(),
        "--".to_owned(),
        UNMAPPED_WORKER_ENTRY.to_owned(),
    ];
    lash_vm_client::service::Service::new(lash_vm_client::PoolConfig::codemode(entry))
}
