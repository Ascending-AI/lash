//! FIG-4275: a cell that starts a process and awaits it parks while the
//! process runs, so one worker slot, shared by the cell and the process
//! engine as a production registration shares it, runs both.

use super::*;

/// A worker service of exactly one slot, with a checkout deadline long
/// enough for any legitimate wait and short enough to report a deadlock.
fn one_slot_workers(backend: &lash_core::Backend) -> lash_vm_client::service::Service {
    let mut config = lash_vm_client::service::Service::default().config().clone();
    config.min_workers = 1;
    config.max_workers = 1;
    config.deadlines.checkout = std::time::Duration::from_secs(10);
    lash_vm_client::service::Service::new(config).with_recovery_store(backend.worker_recovery())
}

#[tokio::test]
async fn one_slot_cell_that_starts_and_awaits_a_process_completes() {
    let artifact_store: lashlang::LashlangArtifacts =
        crate::testing::fresh_sqlite_memory_artifact_store().await;
    let table = crate::testing::DoubleProcesses::new(0x4275_0001).await;
    let workers = one_slot_workers(table.backend());
    let effect_host = table.backend().effect_host();
    let registry = table.registry();
    let process_env_store = table.env_store();
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default(),
        lashlang::LashlangLanguageFeatures::default(),
        lashlang::LashlangHostCatalog::new(),
    );
    let session_policy = lash_core::SessionPolicy {
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("one-slot process await test model"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let engine = || {
        lash_lashlang_runtime::LashlangProcessEngine::new(
            artifact_store.clone(),
            process_engine_surface(surface.clone()),
            table.backend().worker_recovery(),
        )
        .with_worker_service(workers.clone())
    };
    let runtime_host = lash_core::facade_support::RuntimeHostConfig::new(
        table.backend().clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_process_engine_registration(
        lash_lashlang_runtime::lashlang_process_engine_registration(engine()),
    );
    table.install_worker(
        lash_core::testing::test_code_protocol_factories(),
        runtime_host,
        session_policy.clone(),
    );
    let processes: Arc<dyn lash_core::ProcessService> = Arc::new(TypeScriptSignalProcessService {
        registry: registry.clone(),
        effect_host: Arc::clone(&effect_host),
        originator_override: None,
        env_store: Arc::clone(&process_env_store),
        engines: Arc::new(
            lash_core::ProcessEngineRegistry::new()
                .with_artifact_ports(lash_core::ArtifactReferrerPorts::of_backend(
                    table.backend(),
                ))
                .with_registration(lash_lashlang_runtime::lashlang_process_engine_registration(
                    engine(),
                )),
        ),
    });
    let handler = table
        .open_handler(crate::testing::default_cell_scope())
        .await;
    let ctx = lash_core::testing::code_execution_context_with_process_dependencies(
        crate::testing::double_ports(table.double(), &handler),
        Arc::new(ProcessControlToolProvider),
        process_control_tool_catalog(),
        None,
        processes,
        lash_core::ProcessExecutionEnvSpec::new(
            lash_core::AdmittedPluginConfig::default(),
            session_policy,
        ),
    );
    let mut state = RlmExecutionState::for_engine_with_workers("typescript", workers.clone());
    let (response, _) = Box::pin(tokio::time::timeout(
        std::time::Duration::from_secs(60),
        async {
            tokio::join!(
                execute_code_with_test_render(
                    &mut state,
                    ctx,
                    ExecRequest {
                        code: r#"
                            const worker = async () => { return "done"; };
                            const handle = await processes.start({ definition: worker });
                            finish(await handle);
                        "#
                        .to_string(),
                    },
                    artifact_store.clone(),
                    surface.clone(),
                    None,
                    RlmProjectedBindings::default(),
                    RlmLashlangExecutionTraceConfig::default(),
                    lashlang::ExecutionBounds::unbounded(),
                    crate::plugin::RlmChannel::Cell,
                ),
                async {
                    // The started process is admitted while the cell awaits
                    // it; its body needs the one slot.
                    for _ in 0..40 {
                        table.admit_pending().await;
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
            )
        },
    ))
    .await
    .expect("the cell and its awaited process share one slot without deadlock");
    handler.close().await.expect("close the cell handler");
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(response.terminal_finish, Some(serde_json::json!("done")));
    assert_eq!(
        workers
            .pool()
            .expect("the shared pool")
            .config()
            .max_workers,
        1,
        "one slot ran the cell and the process body"
    );
}
