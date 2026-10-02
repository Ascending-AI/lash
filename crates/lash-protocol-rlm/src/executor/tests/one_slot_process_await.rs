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
    run_one_slot_process_await().await;
}

#[tokio::test]
async fn one_slot_process_await_under_parallel_load() {
    let runs = (0..4)
        .map(|_| {
            tokio::task::spawn_blocking(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("load runtime")
                    .block_on(run_one_slot_process_await());
            })
        })
        .collect::<Vec<_>>();
    for run in runs {
        run.await.expect("one-slot process await under load");
    }
}

async fn run_one_slot_process_await() {
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
        model: Some(lash_core::testing::test_llm_profile_config(
            "mock-model",
            lash_core::LlmProfileMetadata::builder("mock-model")
                .context_window_tokens(200_000)
                .build()
                .expect("one-slot process await test model"),
        )),
        ..lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )
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
    );
    let processes: Arc<dyn lash_core::ProcessService> = Arc::new(TypeScriptSignalProcessService {
        hand_over_awaits: None,
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
                    None,
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

async fn published_definition_fixture(
    checkout_deadline: std::time::Duration,
) -> (
    crate::testing::DoubleProcesses,
    lash_vm_client::service::Service,
    lashlang::LashlangArtifacts,
    lash_vm_client::service::CreatedDefinition,
) {
    use lash_vm_client::service::{Request, Response};
    let table = crate::testing::DoubleProcesses::new(0x4625_0001).await;
    let mut config = one_slot_workers(table.backend()).config().clone();
    config.deadlines.checkout = checkout_deadline;
    let workers = lash_vm_client::service::Service::new(config)
        .with_recovery_store(table.backend().worker_recovery());
    let Response::Definition(created) = workers
        .request_accounted(Request::CreateDefinition {
            source: "const answer = async (): Promise<number> => { return 42; };".into(),
            environment: lashlang::LashlangHostEnvironment::default(),
        })
        .await
        .expect("compile the definition")
    else {
        panic!("a compiled definition");
    };
    let artifacts = lashlang::LashlangArtifacts::new(table.backend().module_artifacts());
    let claim = lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
        lash_core::HostArtifactPin::mint(),
    ))
    .expect("host claim");
    artifacts
        .store()
        .publish_module_artifact(
            &claim,
            &created.module.module_ref,
            created.module.bytes.as_bytes(),
        )
        .await
        .expect("publish the module");
    (table, workers, artifacts, created)
}

fn held_worker(workers: &lash_vm_client::service::Service) -> lash_vm_client::Checkout {
    use lash_vm_client::WorkerPoolRuntimeOps as _;
    workers
        .pool()
        .expect("pool")
        .checkout(
            4096,
            lash_vm_protocol::OwnerEpoch(0),
            lash_vm_protocol::FrameEpoch(0),
            lash_vm_client::ExecutionBudget::default(),
        )
        .expect("hold the sole worker")
}

fn definition_engines(
    table: &crate::testing::DoubleProcesses,
    workers: &lash_vm_client::service::Service,
    artifacts: lashlang::LashlangArtifacts,
) -> lash_core::ProcessEngineRegistry {
    let engine = lash_lashlang_runtime::LashlangProcessEngine::new(
        artifacts,
        lash_lashlang_runtime::LashlangSurface::default(),
        table.backend().worker_recovery(),
    )
    .with_worker_service(workers.clone());
    lash_core::ProcessEngineRegistry::new().with_registration(
        lash_lashlang_runtime::lashlang_process_engine_registration(engine),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn artifact_resolution_yields_to_the_task_releasing_the_only_slot() {
    let (table, workers, artifacts, created) =
        published_definition_fixture(std::time::Duration::from_secs(10)).await;
    let engines = definition_engines(&table, &workers, artifacts);
    let held = held_worker(&workers);
    // Poll resolution first. Its checkout must yield so the cell can park
    // and release the only worker on this same runtime thread.
    let finished = std::sync::atomic::AtomicBool::new(false);
    let (resolved, ()) = tokio::join!(biased;
        async {
            let result = engines.derive_definition(&created.draft).await;
            finished.store(true, std::sync::atomic::Ordering::SeqCst);
            result
        },
        async {
            while workers.pool().expect("pool").stats().queued_items == 0
                && !finished.load(std::sync::atomic::Ordering::SeqCst)
            {
                tokio::task::yield_now().await;
            }
            held.release().expect("release the cell's slot");
        },
    );
    resolved.expect("artifact resolution must not block the slot's owner");
}

#[tokio::test(flavor = "current_thread")]
async fn artifact_checkout_timeout_crosses_the_plugin_boundary_as_a_retryable_fault() {
    let (table, workers, artifacts, created) =
        published_definition_fixture(std::time::Duration::from_millis(250)).await;
    let engines = definition_engines(&table, &workers, artifacts.clone());
    let held = held_worker(&workers);
    let artifact_error = workers
        .inspect_artifact(
            &artifacts,
            &lashlang::ProcessDefinitionIdentity::from_process_value(
                created.draft.value().as_json(),
            )
            .expect("definition identity")
            .module_ref,
        )
        .await
        .expect_err("the only worker remains held");
    let artifact_plugin: lash_core::PluginError = artifact_error.into();
    assert!(artifact_plugin.is_retryable(), "{artifact_plugin:?}");
    let refusal = engines
        .derive_definition(&created.draft)
        .await
        .expect_err("engine resolution must time out");
    let plugin: lash_core::PluginError = refusal.into();
    assert!(plugin.is_retryable(), "{plugin:?}");
    assert!(
        matches!(&plugin, lash_core::PluginError::RuntimeEffectController(error)
        if error.code.as_str() == "worker_checkout_timed_out" && error.is_attempt_fault())
    );
    let claim = lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
        lash_core::HostArtifactPin::mint(),
    ))
    .expect("host claim");
    let error = lash_core::ArtifactReferrerPorts::of_backend(table.backend())
        .publish_definition(&engines, &claim, &created.draft)
        .await
        .expect_err("definition inspection must time out");
    held.release().expect("release the held slot");
    assert!(error.is_retryable(), "{error:?}");
    assert!(
        matches!(&error, lash_core::PluginError::RuntimeEffectController(error)
        if error.is_attempt_fault()),
        "the cell must not seal this fault: {error:?}"
    );
    let error: lash_core::PluginError =
        serde_json::from_value(serde_json::to_value(error).expect("encode the plugin fault"))
            .expect("decode the plugin fault");
    let lash_core::PluginError::RuntimeEffectController(error) = error else {
        panic!("the typed controller fault must cross the plugin boundary");
    };
    assert_eq!(error.code.as_str(), "worker_checkout_timed_out");
    assert!(error.code.is_retryable());
    assert_eq!(
        error.turn_failure_cause(),
        lash_core::TurnFailureCause::LiveFault
    );
    let runtime = lash_core::PluginError::RuntimeEffectController(error)
        .into_turn_failure(lash_core::RuntimeErrorCode::Plugin);
    assert_eq!(runtime.code.as_str(), "worker_checkout_timed_out");
    assert_eq!(
        runtime.turn_failure_cause(),
        lash_core::TurnFailureCause::LiveFault
    );
}

#[tokio::test(flavor = "current_thread")]
async fn saturated_process_create_retries_without_recording_a_tool_refusal() {
    struct CreateAttemptProbe {
        inner: Arc<dyn lash_core::ToolProvider>,
        held: Arc<Mutex<Option<lash_vm_client::Checkout>>>,
        outcomes: Arc<Mutex<Vec<(bool, bool)>>>,
    }
    #[async_trait::async_trait]
    impl lash_core::ToolProvider for CreateAttemptProbe {
        fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
            self.inner.tool_manifests()
        }
        fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
            self.inner.resolve_contract(name)
        }
        async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
            let outcome = self.inner.execute(call).await;
            self.outcomes.lock().expect("attempt observations").push((
                matches!(&outcome, lash_core::ToolAttemptOutcome::Done { .. }),
                matches!(&outcome, lash_core::ToolAttemptOutcome::Pending(_)),
            ));
            if let Some(held) = self.held.lock().expect("held slot").take() {
                held.release().expect("release before the engine retry");
            }
            outcome
        }
    }

    let table = crate::testing::DoubleProcesses::new(0x4707_0002).await;
    let backend = table.backend().clone();
    let mut config = one_slot_workers(&backend).config().clone();
    config.deadlines.checkout = std::time::Duration::from_millis(100);
    let workers = lash_vm_client::service::Service::new(config)
        .with_recovery_store(backend.worker_recovery());
    let held = Arc::new(Mutex::new(Some(held_worker(&workers))));
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let definition = lash_lashlang_runtime::process_create_tool_definition();
    let provider: Arc<dyn lash_core::ToolProvider> = Arc::new(CreateAttemptProbe {
        inner: Arc::new(lash_lashlang_runtime::process_create_tool_provider(
            "typescript",
            LashlangSurface::default(),
            workers.clone(),
        )),
        held,
        outcomes: Arc::clone(&outcomes),
    });
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![definition.clone()]);
    let engines = definition_engines(
        &table,
        &workers,
        lashlang::LashlangArtifacts::new(backend.module_artifacts()),
    )
    .with_artifact_ports(lash_core::ArtifactReferrerPorts::of_backend(&backend));
    let invocation = lash_core::testing::exec_code_invocation(
        "test-session",
        "create-retry",
        0,
        0,
        "cell",
        "create-retry",
    );
    let attempt: lash_restate_test::HandlerAttempt = {
        let outputs = Arc::clone(&outputs);
        Arc::new(move |scoped| {
            let ctx = lash_core::testing::TestExecutionContextBuilder::new(
                crate::testing::attempt_ports(&backend, scoped),
            )
            .provider(Arc::clone(&provider))
            .tool_catalog(catalog.clone())
            .process_engines(engines.clone())
            .runtime_parent_invocation(invocation.clone())
            .build()
            .into_runtime();
            let outputs = Arc::clone(&outputs);
            let tool_id = definition.manifest.id.clone();
            Box::pin(async move {
                let reply = ctx.call_command_tool(
                    &lash_core::CommandReplayKey::new("create-process"),
                    lash_core::facade_support::ToolInvocation::new(
                        lash_core::ToolCallId::fixture("create-process"), tool_id,
                        serde_json::json!({
                            "source": "const answer = async (): Promise<number> => { return 42; };",
                            "dialect": "typescript",
                        }),
                    ),
                ).await;
                assert!(ctx.take_nested_effect_error().is_none());
                outputs
                    .lock()
                    .expect("tool observations")
                    .push(reply.output);
            })
        })
    };
    table
        .double()
        .run_in_handler(
            lash_core::AdmittedScope::turn(
                lash_core::SessionId::from("test-session"),
                lash_core::TurnId::from("create-retry"),
            ),
            attempt,
        )
        .await
        .expect("the engine retries the saturated attempt");
    assert_eq!(
        *outcomes.lock().expect("attempt observations"),
        vec![(false, false), (true, false)],
        "the saturated host attempt is retried before any completed result",
    );
    let outputs = outputs.lock().expect("tool observations");
    assert!(!outputs.is_empty());
    assert!(
        outputs.iter().all(lash_core::ToolCallOutput::is_success),
        "{outputs:?}"
    );
    let view = table
        .double()
        .server()
        .invocations()
        .into_iter()
        .find(|view| view.target.starts_with("LashTestHandlerHost/"))
        .expect("the create handler ran");
    assert_eq!(view.status, "completed");
    assert_eq!(
        view.retry_count, 1,
        "pool saturation retries the engine attempt"
    );
    let journal = table
        .double()
        .server()
        .journal(&view.id)
        .expect("handler journal");
    let fault = lash_vm_client::PoolError::CheckoutTimedOut.to_string();
    assert!(
        journal.iter().all(|entry| !entry
            .payload
            .windows(fault.len())
            .any(|window| window == fault.as_bytes())),
        "the journal holds no tool refusal: {journal:?}"
    );
}
