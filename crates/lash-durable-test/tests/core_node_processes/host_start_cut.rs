//! A host start that states its environment, cut between the environment's
//! hold and the process's registration (FIG-7700 A5).

use super::*;

/// An environment store that commits its first publish and then never
/// answers: the node dies with the start's hold committed and nothing
/// registered.
struct DyingEnvStore {
    inner: Arc<dyn lash_execution_contracts::process::ProcessEnvStore>,
    armed: std::sync::atomic::AtomicBool,
    cut: tokio::sync::Notify,
}

impl lash_core_store::store::FleetFormatStore for DyingEnvStore {
    fn fleet_format(&self) -> lash_core_store::store::FleetFormat {
        self.inner.fleet_format()
    }
    fn plugin_writers(&self) -> lash_core_store::store::PluginWriterRangesFuture<'_> {
        self.inner.plugin_writers()
    }
    fn provision_plugin_writers<'a>(
        &'a self,
        registrations: &'a [lash_core_store::store::plugin_writers::PluginWriterRegistration],
    ) -> lash_core_store::store::PluginWriterRangesFuture<'a> {
        self.inner.provision_plugin_writers(registrations)
    }
}

#[async_trait::async_trait]
impl lash_execution_contracts::process::ProcessEnvStore for DyingEnvStore {
    async fn publish_process_env(
        &self,
        claim: &lash_core_store::artifact_referrer::ReferrerClaim,
        env_ref: &lash::process::ProcessEnvRef,
        bytes: &[u8],
    ) -> Result<(), lash_execution_contracts::ArtifactStoreError> {
        self.inner
            .publish_process_env(claim, env_ref, bytes)
            .await?;
        if self.armed.swap(false, Ordering::SeqCst) {
            self.cut.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(())
    }
    async fn acquire_process_env(
        &self,
        claim: &lash_core_store::artifact_referrer::ReferrerClaim,
        env_ref: &lash::process::ProcessEnvRef,
    ) -> Result<(), lash_execution_contracts::ArtifactStoreError> {
        self.inner.acquire_process_env(claim, env_ref).await
    }
    async fn end_process_env_referrer(
        &self,
        cleanup: &lash_core_store::artifact_referrer::ResolvedArtifactCleanup,
    ) -> Result<(), lash_execution_contracts::ArtifactStoreError> {
        self.inner.end_process_env_referrer(cleanup).await
    }
    async fn get_process_env(
        &self,
        env_ref: &lash::process::ProcessEnvRef,
    ) -> Result<Option<Vec<u8>>, lash_execution_contracts::ArtifactStoreError> {
        self.inner.get_process_env(env_ref).await
    }
}

/// `inner` with [`DyingEnvStore`] as its environment store.
struct DyingStores {
    inner: Arc<dyn StoreSet>,
    envs: Arc<DyingEnvStore>,
}

impl StoreSet for DyingStores {
    fn package_store(&self) -> Arc<dyn lash_durable::packages::PackageStore> {
        self.inner.package_store()
    }
    fn execution_resolutions(&self) -> Arc<dyn lash_durable::resolutions::ExecutionResolutions> {
        self.inner.execution_resolutions()
    }
    fn durable_store(&self) -> Arc<dyn lash_durable::DurableStore> {
        self.inner.durable_store()
    }
    fn wake_sink(&self) -> lash_durable::runner::WakeSink {
        self.inner.wake_sink()
    }
    fn node_wakes(&self) -> Option<Arc<dyn lash_durable::NodeWakes>> {
        self.inner.node_wakes()
    }
    fn binding_identity(&self) -> &lash_execution_contracts::backend::StoreBindingId {
        self.inner.binding_identity()
    }
    fn clock(&self) -> Arc<dyn lash_core::Clock> {
        self.inner.clock()
    }
    fn session_store_factory(
        &self,
    ) -> Arc<dyn lash_execution_contracts::deployment::DeploymentStore> {
        self.inner.session_store_factory()
    }
    fn attachment_referrers(&self) -> Arc<dyn lash_core_execution::store::AttachmentReferrers> {
        self.inner.attachment_referrers()
    }
    fn process_registry(&self) -> Arc<dyn lash_execution_contracts::process::ProcessRegistry> {
        self.inner.process_registry()
    }
    fn process_authority_reads(
        &self,
    ) -> Arc<dyn lash_execution_contracts::backend::ProcessAuthorityReads> {
        self.inner.process_authority_reads()
    }
    fn process_env_store(&self) -> Arc<dyn lash_execution_contracts::process::ProcessEnvStore> {
        self.envs.clone()
    }
    fn turn_prelude_store(
        &self,
    ) -> Arc<dyn lash_execution_contracts::turn_prelude::TurnPreludeStore> {
        self.inner.turn_prelude_store()
    }
    fn tool_material_store(&self) -> Arc<dyn lash_core_execution::store::ToolMaterialStore> {
        self.inner.tool_material_store()
    }
    fn definition_store(
        &self,
    ) -> Arc<dyn lash_execution_contracts::process::ProcessDefinitionStore> {
        self.inner.definition_store()
    }
    fn attachment_store(&self) -> Arc<dyn lash_core::AttachmentStore> {
        self.inner.attachment_store()
    }
    fn module_artifacts(
        &self,
    ) -> Arc<dyn lash_execution_contracts::module_artifacts::ModuleArtifactStore> {
        self.inner.module_artifacts()
    }
    fn obligation_ledger(
        &self,
        kind: lash_core_execution::store::ObligationKind,
    ) -> Arc<dyn lash_core_execution::store::ObligationLedger> {
        self.inner.obligation_ledger(kind)
    }
    fn artifact_cleanup(&self) -> Arc<dyn lash_core_execution::store::ArtifactCleanupLedger> {
        self.inner.artifact_cleanup()
    }
}

/// A node that dies after its host start held the stated environment and
/// before the process registered leaves the environment to the start's key:
/// a retry under the key on another node registers the process, which then
/// holds the environment alone, so once its ended record is pruned and the
/// relay runs, nothing holds it. A hold that only a live attempt releases
/// would outlive the process.
async fn a_host_start_cut_before_registration_leaves_its_environment_to_the_key(tier: Tier) {
    let engine: Arc<dyn lash_execution_contracts::process::ProcessEngine> =
        Arc::new(ScriptEngine {
            kind: HOLD_ENGINE,
            advance: holding_engine_advance,
        });
    let (stores, _keep) = stores(tier).await;
    let envs = Arc::new(DyingEnvStore {
        inner: stores.process_env_store(),
        armed: std::sync::atomic::AtomicBool::new(true),
        cut: tokio::sync::Notify::new(),
    });
    let dying: Arc<dyn StoreSet> = Arc::new(DyingStores {
        inner: Arc::clone(&stores),
        envs: Arc::clone(&envs),
    });
    let node = |stores: Arc<dyn StoreSet>| {
        node_over(
            stores,
            lash_core_execution::DurableSettings::default(),
            vec![Arc::clone(&engine)],
            |backend| lash::LashCore::standard_builder(backend.clone()),
        )
    };
    let env = environment();
    let env_ref = env.stable_ref().expect("the environment encodes");
    let request = || {
        lash_execution_contracts::process::ProcessStartRequest::engine(
            HOLD_ENGINE.to_owned(),
            serde_json::json!({ "run": "cut" }),
            lash_execution_contracts::process::EngineLaunch::new(env_ref.clone(), None),
            lash_execution_contracts::process::ProcessOriginator::host(),
            lash_execution_contracts::process::LifetimeDecision::Detached,
        )
    };
    let env_store = stores.process_env_store();
    let resolves = || {
        let env_store = Arc::clone(&env_store);
        let env_ref = env_ref.clone();
        async move {
            lash_execution_contracts::process::load_process_env(env_store.as_ref(), &env_ref)
                .await
                .is_ok()
        }
    };
    let operation = lash::OperationId::new("host-start-cut-before-registration");

    let (_, dead) = node(dying);
    let start = tokio::spawn({
        let processes = dead.processes();
        let (operation, request, env) = (operation.clone(), request(), env.clone());
        async move { processes.start_with_env(operation, request, env).await }
    });
    tokio::time::timeout(Duration::from_secs(60), envs.cut.notified())
        .await
        .expect("the start holds its environment");
    start.abort();
    assert!(
        start.await.is_err_and(|error| error.is_cancelled()),
        "the start dies before it registers"
    );
    dead.shutdown().await.expect("the dead node stops");
    assert!(
        resolves().await,
        "the cut falls after the environment's hold committed"
    );

    let (backend, core) = node(Arc::clone(&stores));
    let relay = lash_core::runtime::artifact_cleanup::ArtifactCleanupRelay::over_backend(
        &backend,
        lash_core::ProcessEngineRegistry::new().with_registration(
            lash_core::ProcessEngineRegistration::accepting(Arc::clone(&engine)),
        ),
    );
    let processes = core.processes();
    let started = processes
        .start_with_env(operation.clone(), request(), env.clone())
        .await
        .expect("a retry under the key registers the process");
    relay_a_day_on(&relay).await;
    assert!(
        resolves().await,
        "the registered process holds its stated environment"
    );
    processes
        .cancel(lash::OperationId::mint(), &started.process_id)
        .await
        .expect("the process is cancelled");
    tokio::time::timeout(
        Duration::from_secs(60),
        processes.await_output(&started.process_id),
    )
    .await
    .expect("the process ends within a minute")
    .expect("the process's end is read");
    tokio::time::timeout(Duration::from_secs(60), async {
        while processes
            .prune(
                u64::MAX,
                None,
                lash_execution_contracts::process::ProjectionWatermark::NoProjector,
            )
            .await
            .expect("terminal processes are pruned")
            .pruned_processes
            == 0
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the ended process's record is pruned");
    relay_a_day_on(&relay).await;
    assert!(
        !resolves().await,
        "once the start's key and its process have ended, nothing holds the environment"
    );
    core.shutdown().await.expect("the node stops");
}

on_every_tier!(a_host_start_cut_before_registration_leaves_its_environment_to_the_key);
