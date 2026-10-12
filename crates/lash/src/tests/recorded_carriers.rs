//! FIG-5520: no runtime carrier runs under a blank ambient authority or a
//! silent preset. After a reopen, an RLM session's plugin session and a
//! process runtime run under the budgets their host records and the
//! authority their session or environment recorded.

use super::*;
use crate::support::TurnInput;
use lash_sansio::SessionId;

fn stated_budgets() -> crate::ExecutionBudgets {
    let budgets = crate::ExecutionBudgets::new(crate::ExecutionBudgetsConfig {
        model_total: std::time::Duration::from_secs(90),
        control_phase: std::time::Duration::from_secs(7),
        stop_grace: std::time::Duration::from_secs(1),
        provider: crate::ProviderAttemptLimits::new(
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(10),
            std::time::Duration::from_secs(10),
            2,
        )
        .expect("valid provider limits"),
        agent_frame_switch_limit: std::num::NonZeroU32::new(3).expect("nonzero"),
    })
    .expect("valid budgets");
    assert_ne!(budgets, crate::ExecutionBudgets::recommended());
    budgets
}

/// An RLM core over the SQLite file store at `path`, under `budgets`, whose
/// model answers with one cell that finishes.
async fn rlm_core_at(path: &std::path::Path, budgets: crate::ExecutionBudgets) -> LashCore {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(path, lash_sqlite_store::SqliteSynchronous::Normal)
            .await
            .expect("open the SQLite file store"),
    );
    let backend = lash_conformance::backend_over(stores);
    let factory = rlm_factory();
    LashCore::rlm_builder(backend, factory)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(crate::DataRetention::standard())
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(crate::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(budgets)
        .delta_coalescing(crate::DeltaCoalescing::recommended())
        .serve_test_llm_profile(
            queued_text_provider(vec![typescript_block(r#"await control.finish("done");"#)]),
            mock_llm_profile_spec(),
        )
        .build(crate::testing::runtime_lease_owner())
        .expect("RLM core")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reopened_rlm_session_and_its_process_worker_run_under_recorded_budgets_and_authority() {
    let directory = tempfile::tempdir().expect("SQLite test directory");
    let path = directory.path().join("lash.db");
    let budgets = stated_budgets();
    let authority = crate::plugins::SessionToolAccess::ambient()
        .with_hidden_tools(["withheld".to_string()])
        .expect("valid hidden tools");
    assert_ne!(authority, crate::plugins::SessionToolAccess::ambient());
    let id = SessionId::from("recorded-carriers");

    // The first host creates the session, then goes away.
    let core = rlm_core_at(&path, budgets.clone()).await;
    core.session(id.clone())
        .create(crate::SessionCreation::root(
            authority.clone(),
            mock_session_spec(),
        ))
        .await
        .expect("created");
    core.shutdown().await.expect("shutdown");
    drop(core);

    // A second host reopens the store under the same stated budgets.
    let core = rlm_core_at(&path, budgets.clone()).await;
    let session = core.session(id).open().await.expect("reopened session");
    session
        .send(TurnInput::text("finish"))
        .output()
        .await
        .expect("the reopened session runs a turn");
    let plugins = Arc::clone(
        session
            .runtime
            .writer()
            .lock()
            .await
            .assembled_plugin_session(),
    );
    assert_eq!(plugins.execution_budgets(), budgets);
    assert_eq!(plugins.tool_access(), authority);

    // A process worker the reopened host serves runs under the host's
    // stated budgets.
    let worker = core
        .durable_process_worker_config()
        .expect("process worker config");
    assert_eq!(worker.runtime_host.control.execution_budgets, budgets);
    drop(session);
    core.shutdown().await.expect("shutdown");
}
