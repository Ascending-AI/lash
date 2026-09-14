//! FIG-2966: the attempt bound engine-started children register with.

use super::*;

/// An artifact store that publishes normally but fails every read.
///
/// A worker wired to this store meets the same infrastructure failure on every
/// attempt, which is the deterministic non-terminal failure the attempt bound
/// exists to stop. A missing artifact would not do: that is a terminal
/// producer failure and never retries.
struct UnreadableArtifactStore {
    inner: Arc<dyn lashlang::LashlangArtifactStore>,
    reads: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl lashlang::LashlangArtifactStore for UnreadableArtifactStore {
    fn durability_tier(&self) -> lashlang::DurabilityTier {
        self.inner.durability_tier()
    }

    async fn publish_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        artifact: &lashlang::ModuleArtifact,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        self.inner.publish_module_artifact(owner, artifact).await
    }

    async fn retain_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        module_ref: &lashlang::ModuleRef,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        self.inner.retain_module_artifact(owner, module_ref).await
    }

    async fn transfer_module_artifact(
        &self,
        from: &lash_core::ArtifactOwner,
        to: &lash_core::ArtifactOwner,
        module_ref: &lashlang::ModuleRef,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        self.inner
            .transfer_module_artifact(from, to, module_ref)
            .await
    }

    async fn release_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        module_ref: &lashlang::ModuleRef,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        self.inner.release_module_artifact(owner, module_ref).await
    }

    async fn retire_module_artifact_owner(
        &self,
        owner: &lash_core::ArtifactOwner,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        self.inner.retire_module_artifact_owner(owner).await
    }

    async fn get_module_artifact(
        &self,
        _module_ref: &lashlang::ModuleRef,
    ) -> Result<Option<Arc<lashlang::ModuleArtifact>>, lashlang::ArtifactStoreError> {
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Err(lashlang::ArtifactStoreError::Backend(
            "injected deterministic artifact read failure".to_string(),
        ))
    }
}

/// FIG-2966: the registry row, not the in-flight pin, decides an already
/// registered child's attempt bound.
///
/// The pin is less durable than the registration it identifies: the child row
/// lands in the registrar's own transaction, while the pin only reaches disk at
/// the cell's execution-state commit. A run that starts a child and then loses
/// its lease before that commit resumes with no pin at all. If such a redrive
/// trusted the host default in force now, the same deterministic child id would
/// hash a different registration fingerprint and conflict forever.
#[tokio::test]
pub(super) async fn a_redrive_after_the_host_default_moved_reregisters_the_recorded_bound() {
    const PINNED: u32 = 5;
    const CHANGED: u32 = 10;

    let published: Arc<dyn lashlang::LashlangArtifactStore> =
        Arc::new(lashlang::InMemoryLashlangArtifactStore::new());
    let registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
    let process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
        Arc::new(lash_core::facade_support::InMemoryProcessExecutionEnvStore::new());
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default().with_processes(),
        lashlang::LashlangLanguageFeatures::default(),
        lashlang::LashlangHostCatalog::new(),
    );
    let session_policy = lash_core::SessionPolicy {
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("redrive test model"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };

    let run = |host_default: u32| {
        let published = Arc::clone(&published);
        let registry = registry.clone();
        let process_env_store = Arc::clone(&process_env_store);
        let surface = surface.clone();
        let session_policy = session_policy.clone();
        async move {
            let controller: Arc<dyn lash_core::RuntimeEffectController> = Arc::new(
                lash_core::facade_support::NativeRuntimeEffectController::default()
                    .allow_process_lifetime_completion_keys(),
            );
            let processes: Arc<dyn lash_core::ProcessService> =
                Arc::new(TypeScriptSignalProcessService {
                    registry: registry.clone(),
                    controller: controller.clone(),
                    originator_override: None,
                });
            let ctx = lash_core::testing::with_engine_child_max_attempts(
                lash_core::testing::code_execution_context_with_process_dependencies(
                    Arc::new(EmptyTypeScriptSignalToolProvider),
                    lash_core::ToolCatalog::from_tool_definitions(Vec::new()),
                    None,
                    processes,
                    controller,
                    process_env_store,
                    lash_core::ProcessExecutionEnvSpec::new(
                        lash_core::PluginOptions::default(),
                        session_policy,
                    ),
                ),
                std::num::NonZeroU32::new(host_default).expect("test attempt bound is non-zero"),
            );
            // A fresh execution state is exactly the redrive case: the pin the
            // first run held never reached the durable snapshot.
            let mut state = RlmExecutionState::for_engine("typescript");
            execute_code_with_channel_and_bounds(
                &mut state,
                ctx.clone(),
                ExecRequest {
                    language: "typescript".to_string(),
                    code: r#"
                    const worker = defineProcess({ name: "worker", run: async () => 1 });
                    start(worker);
                    finish("started");
                "#
                    .to_string(),
                },
                published,
                surface,
                None,
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
                lashlang::ExecutionBounds::unbounded(),
                crate::plugin::RlmChannel::Cell,
            )
            .await
        }
    };

    let first = run(PINNED).await;
    assert!(first.error.is_none(), "{:?}", first.error);
    let observed = registry
        .list_observed_by(
            &SessionId::from("test-session"),
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("list the started child");
    let [child] = observed.as_slice() else {
        panic!("expected exactly one started child, got {observed:?}");
    };
    let child_id = child.id.clone();
    assert_eq!(
        child.max_attempts,
        Some(PINNED),
        "the first run records the bound it resolved"
    );

    // The operator reconfigures the host and the run is redriven.
    let second = run(CHANGED).await;
    assert!(
        second.error.is_none(),
        "the redrive must re-register idempotently, got {:?}",
        second.error
    );
    let observed = registry
        .list_observed_by(
            &SessionId::from("test-session"),
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("list the child after the redrive");
    let [redriven] = observed.as_slice() else {
        panic!("the redrive must not create a second child, got {observed:?}");
    };
    assert_eq!(redriven.id, child_id, "the child id is deterministic");
    assert_eq!(
        redriven.max_attempts,
        Some(PINNED),
        "the redrive re-registers with the bound on the row, not the new host default"
    );
}

/// FIG-2966: a script-started child registers with the host's attempt bound, so
/// a run that fails the same way on every attempt becomes an Abandoned fact
/// written by the engine instead of retrying forever.
#[tokio::test]
pub(super) async fn engine_started_child_failing_every_attempt_is_abandoned_at_the_bound() {
    const MAX_ATTEMPTS: u32 = 2;

    let published: Arc<dyn lashlang::LashlangArtifactStore> =
        Arc::new(lashlang::InMemoryLashlangArtifactStore::new());
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let worker_store: Arc<dyn lashlang::LashlangArtifactStore> =
        Arc::new(UnreadableArtifactStore {
            inner: Arc::clone(&published),
            reads: Arc::clone(&reads),
        });
    let registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
    let process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
        Arc::new(lash_core::facade_support::InMemoryProcessExecutionEnvStore::new());
    let controller: Arc<dyn lash_core::RuntimeEffectController> = Arc::new(
        lash_core::facade_support::NativeRuntimeEffectController::default()
            .allow_process_lifetime_completion_keys(),
    );
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default().with_processes(),
        lashlang::LashlangLanguageFeatures::default(),
        lashlang::LashlangHostCatalog::new(),
    );
    let session_policy = lash_core::SessionPolicy {
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("attempt-bound test model"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let runtime_host = lash_core::facade_support::RuntimeHostConfig::new(
        Arc::new(
            lash_core::facade_support::NativeEffectHost::new(controller.clone())
                .allow_process_lifetime_completion_keys(),
        ),
        Arc::new(lash_core::facade_support::InMemoryAttachmentStore::new()),
        process_env_store.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_process_engine_registration(
        lash_lashlang_runtime::lashlang_process_engine_registration(
            lash_lashlang_runtime::LashlangProcessEngine::new(
                Arc::clone(&worker_store),
                surface.clone(),
            ),
        ),
    );
    let registry_dyn: Arc<dyn lash_core::ProcessRegistry> = registry.clone();
    let watched = lash_core::facade_support::watch_process_registry(registry_dyn);
    let worker = lash_core::facade_support::DurableProcessWorker::new(
        lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(
                lash_core::testing::test_code_protocol_factories(),
            )),
            runtime_host,
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            lash_core::WorkerProcessWork::SelfNative(watched),
            Arc::new(lash_core::NoQueuedWork::new()),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_policy(session_policy.clone()),
    )
    .expect("valid test native substrate config");
    let processes: Arc<dyn lash_core::ProcessService> = Arc::new(TypeScriptSignalProcessService {
        registry: registry.clone(),
        controller: controller.clone(),
        originator_override: None,
    });
    let ctx = lash_core::testing::with_engine_child_max_attempts(
        lash_core::testing::code_execution_context_with_process_dependencies(
            Arc::new(EmptyTypeScriptSignalToolProvider),
            lash_core::ToolCatalog::from_tool_definitions(Vec::new()),
            None,
            processes,
            controller,
            process_env_store,
            lash_core::ProcessExecutionEnvSpec::new(
                lash_core::PluginOptions::default(),
                session_policy,
            ),
        ),
        std::num::NonZeroU32::new(MAX_ATTEMPTS).expect("test attempt bound is non-zero"),
    );

    let mut state = RlmExecutionState::for_engine("typescript");
    let response = execute_code_with_channel_and_bounds(
        &mut state,
        ctx.clone(),
        ExecRequest {
            language: "typescript".to_string(),
            code: r#"
                    const worker = defineProcess({ name: "worker", run: async () => 1 });
                    start(worker);
                    finish("started");
                "#
            .to_string(),
        },
        Arc::clone(&published),
        surface.clone(),
        None,
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);

    let records = registry
        .list_observed_by(
            &SessionId::from("test-session"),
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("list the started child");
    let [observed] = records.as_slice() else {
        panic!("expected exactly one started child, got {records:?}");
    };
    let child_id = observed.id.clone();
    let registered = registry
        .get_process(&child_id)
        .await
        .expect("read the child record")
        .expect("the child row is registered");
    assert_eq!(
        registered.max_attempts,
        Some(MAX_ATTEMPTS),
        "the resolved host bound must be written to the child's record"
    );

    // Every drive meets the same read failure and releases the row without a
    // terminal, so the row is claimable again immediately.
    let mut drives = 0;
    let record = loop {
        assert!(
            drives < 60,
            "the bounded child must terminalize (reads={}, last={:?})",
            reads.load(std::sync::atomic::Ordering::Relaxed),
            registry.get_process(&child_id).await
        );
        let _ = worker
            .drive_pending_processes()
            .await
            .expect("drive the failing child");
        drives += 1;
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let record = registry
            .get_process(&child_id)
            .await
            .expect("read the child record")
            .expect("the child row survives each attempt");
        if record.is_terminal() {
            break record;
        }
    };

    assert_eq!(
        reads.load(std::sync::atomic::Ordering::Relaxed),
        MAX_ATTEMPTS as usize,
        "the engine runs exactly the budgeted attempts before giving up"
    );
    assert_eq!(record.status, lash_core::ProcessStatus::Abandoned);
    let terminal = record
        .outcome
        .as_ref()
        .expect("an abandoned row carries its terminal output");
    let lash_core::ProcessAwaitOutput::Abandoned { evidence, .. } = terminal else {
        panic!("expected an abandoned terminal, got {terminal:?}");
    };
    assert_eq!(
        evidence.writer,
        lash_core::AbandonWriter::EngineGaveUp,
        "the engine, not an operator, wrote the terminal"
    );
}
