mod durable_host;

pub(crate) use durable_host::DurableHost;

use std::sync::Arc;

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
    lash_core::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    stores
}

/// The scope a context built with no parent invocation claims: the builder's
/// default test turn. Open the [`DurableHost`] whose context serves it for it.
pub(crate) fn default_cell_scope() -> lash_core::AdmittedScope {
    lash_core::AdmittedScope::turn(
        lash_core::SessionId::from("test-session"),
        lash_core::TurnId::from("test-turn"),
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
            .expect("test render params serialize"),
    }
}

/// What a cell of `dialect` needs of its session, with no bounds and the
/// builtin renderer, on the cell channel.
pub(crate) fn cell_services(
    dialect: &crate::CellDialect,
    workers: lash_vm_client::service::Service,
    deferred_tool_resolver: Option<crate::SharedDeferredToolResolver>,
) -> crate::executor::CellServices {
    crate::executor::CellServices {
        workers,
        deferred_tool_resolver,
        execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
        channel: crate::plugin::RlmChannel::Cell,
        code_renderer: crate::render::CodeRendererSlot::default(),
        prompts: dialect.prompts(),
    }
}

/// Run one cell through the production executor entry and settle it as
/// its turn would: accepted.
pub(crate) async fn run_cell(
    state: &mut crate::executor::RlmExecutionState,
    ctx: lash_core::RuntimeExecutionContext<'_>,
    services: &crate::executor::CellServices,
    code: &str,
) -> lash_core::ExecResponse {
    let response = Box::pin(crate::executor::execute_cell(
        state,
        ctx.with_recorded_render(recorded_test_render()),
        lash_core::ExecRequest {
            code: code.to_string(),
        },
        services,
        crate::projection::RlmProjectedBindings::default(),
    ))
    .await;
    if !response.suspended {
        state.mark_code_execution_response_returned();
        state.accept_code_execution();
    }
    response
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
    let catalog = lash_core::ToolCatalog::from_tool_definitions(
        provider
            .tool_manifests()
            .into_iter()
            .filter_map(|manifest| {
                let contract = provider.resolve_contract(&manifest.name)?;
                Some(lash_core::ToolDefinition::from_parts(
                    manifest,
                    contract.as_ref().clone(),
                ))
            })
            .collect(),
    );
    lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
        host.ports(),
        provider,
        catalog,
        lash_core::testing::exec_code_invocation(session, turn, 0, 0, "exec-code", replay_key),
    )
}

/// The name of [`python_worker_entry`], as libtest spells it.
const PYTHON_WORKER_ENTRY: &str = "testing::python_worker_entry";

/// What a worker with the Python dialect runs: the kernel library and
/// `lash-dialect-python`.
fn python_embedding(
    _tuning: &lash_vm_client::WorkerTuning,
) -> Result<lash_vm_worker::Embedding, lash_vm_worker::EmbedError> {
    let mut embedder = lash_vm_worker::Embedder::kernel()?;
    let mut library = embedder.library()?;
    let functions = lash_dialect_python::define_helpers(&mut library).map_err(|error| {
        lash_vm_worker::EmbedError::Dialect {
            dialect: lash_dialect_python::DIALECT.to_owned(),
            message: error.to_string(),
        }
    })?;
    embedder.install(lash_dialect_python::package(functions))?;
    embedder.finish()
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
    if lash_vm_worker::worker_entry_with(&python_embedding).expect("serve as a Python worker") {
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
    lash_vm_client::service::Service::new(lash_vm_client::PoolConfig::rlm(entry))
}
