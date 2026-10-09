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
    let factory = rlm_factory(&backend);
    LashCore::rlm_builder(backend, factory)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(crate::DataRetention::standard())
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(crate::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(budgets)
        .delta_coalescing(crate::DeltaCoalescing::recommended())
        .serve_test_llm_profile(
            queued_text_provider(vec![typescript_block(r#"finish("done");"#)]),
            mock_llm_profile_spec(),
        )
        .build(crate::testing::runtime_lease_owner())
        .expect("RLM core")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reopened_rlm_session_and_process_runtime_run_under_recorded_budgets_and_authority() {
    let directory = tempfile::tempdir().expect("SQLite test directory");
    let path = directory.path().join("lash.db");
    let budgets = stated_budgets();
    let authority = crate::plugins::SessionToolAccess::ambient()
        .with_hidden_tools(["withheld".to_string()])
        .expect("valid hidden tools");
    assert_ne!(authority, crate::plugins::SessionToolAccess::ambient());
    let id = SessionId::from("recorded-carriers");

    // The first host creates the session and starts a process under the
    // session's recorded plugin configuration, then goes away.
    let core = rlm_core_at(&path, budgets.clone()).await;
    core.session(id.clone())
        .create(crate::SessionCreation::root(
            authority.clone(),
            mock_session_spec(),
        ))
        .await
        .expect("created");
    let head =
        lash_core::SessionCommitStore::load_session_head_meta(core.store_factory.as_ref(), &id)
            .await
            .expect("head read")
            .expect("persisted head")
            .config;
    let environment = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::new(head.plugin_config.clone(), head.config_revision),
        head.session_policy(),
        authority.clone(),
    );
    assert!(
        !environment.plugin_config.config.is_empty(),
        "the session records its RLM configuration"
    );
    let env_ref = lash_core::testing::publish_process_execution_env_for_testing(
        core.env.core.durability.process_env_store.as_ref(),
        &lash_core::testing::host_pin_claim_for_testing(),
        &environment,
    )
    .await
    .expect("published environment");
    let process_id = core
        .process_registry()
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::testing::held_engine_input(serde_json::Value::Null),
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(env_ref)),
        )
        .await
        .expect("registered process")
        .id;
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

    // The process runtime is handed a plugin host constructed under another
    // budget: it runs under its host config's, and under the plugin
    // configuration its environment recorded.
    let process = core
        .process_registry()
        .get_process(&process_id)
        .await
        .expect("process read")
        .expect("the process survives the reopen");
    let worker = core
        .durable_process_worker_config()
        .expect("process worker config");
    assert_eq!(worker.runtime_host.control.execution_budgets, budgets);
    let runtime = lash_core::runtime::ProcessRuntimeContext::for_record(
        lash_core::runtime::ProcessRuntimePorts {
            host: worker.runtime_host.clone(),
            plugin_host: Arc::new(
                Arc::unwrap_or_clone(Arc::clone(&worker.plugin_host))
                    .with_execution_budgets(crate::ExecutionBudgets::recommended()),
            ),
            process_work: worker.process_work().clone(),
            lease_owner: worker.lease_owner.clone(),
            turn_phase_probe: None,
        },
        &process,
    )
    .await
    .expect("process runtime");
    let plugins = runtime.plugin_session();
    assert_eq!(plugins.execution_budgets(), budgets);
    assert_eq!(plugins.admitted_plugin_config(), environment.plugin_config);
    assert_eq!(
        plugins.tool_access(),
        authority,
        "a reopened process keeps its recorded tool restriction"
    );
    drop(session);
    core.shutdown().await.expect("shutdown");
}
