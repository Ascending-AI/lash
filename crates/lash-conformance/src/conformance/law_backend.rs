//! The backend a conformance law runs its runtime over.
//!
//! A runtime takes every port from one [`crate::Backend`] (ADR 0102, D2). A
//! law is handed the backend under test by the embedder and runs its runtime
//! over that backend's ports. [`LawBackend`] is that backend with the ports a
//! law substitutes: its own handle on the same substrate's process registry.

use std::sync::Arc;

pub(crate) struct LawBackend {
    layered: crate::testing::runtime_helpers::LayeredBackend,
}

impl LawBackend {
    pub(crate) fn over(backend: &crate::Backend) -> Self {
        Self {
            layered: crate::testing::runtime_helpers::LayeredBackend::over(backend.clone()),
        }
    }

    pub(crate) fn over_stores(stores: Arc<dyn crate::StoreSet>) -> Self {
        Self::over(&crate::Backend::for_testing(stores))
    }

    pub(crate) fn with_process_registry(
        self,
        process_registry: Arc<dyn crate::ProcessRegistry>,
    ) -> Self {
        Self {
            layered: self.layered.map_process_registry(|_| process_registry),
        }
    }

    pub(crate) fn into_backend(self) -> crate::Backend {
        self.layered.into_backend()
    }

    pub(crate) fn host_config(
        self,
        commit_budget: crate::CommitBudget,
        queued_work_batching: crate::QueuedWorkBatchingConfig,
    ) -> crate::RuntimeHostConfig {
        crate::RuntimeHostConfig::new(self.into_backend(), commit_budget, queued_work_batching)
    }
}

pub(crate) async fn law_session_store(
    stores: &dyn crate::StoreSet,
    session_id: &crate::SessionId,
) -> Arc<dyn crate::RuntimeStore> {
    law_session_store_with_config(
        stores,
        session_id,
        crate::testing::mock_session_policy().into(),
    )
    .await
}

/// [`law_session_store`], except the created head records `config`: a law
/// whose runtime runs a plugin that owns recorded configuration, or a policy
/// other than the canonical test one, admits the session the way a creator
/// would (FIG-4553) so a later open adopts exactly it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: a fresh catalog admits a fresh session"
)]
pub(crate) async fn law_session_store_with_config(
    stores: &dyn crate::StoreSet,
    session_id: &crate::SessionId,
    config: crate::PersistedSessionConfig,
) -> Arc<dyn crate::RuntimeStore> {
    let deployment = stores.session_store_factory();
    deployment
        .admit_session(&crate::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: crate::SessionRelation::Root,
            config,
            head: crate::SessionCreationHead::Config,
        })
        .await
        .expect("admit the law's session on the backend under test");
    deployment
}

/// A backend over `stores` whose effects journal on `effect_host`, for an
/// embedder whose substrate is storage only: the law's runtime reaches every
/// storage port of `stores`, executes no session work, and runs its effects on
/// the host the embedder supplies.
pub fn backend_over(stores: Arc<dyn crate::StoreSet>) -> crate::Backend {
    LawBackend::over_stores(stores).into_backend()
}

pub(crate) struct StoreLawBackend {
    stores: Arc<StoreLawStores>,
}

impl StoreLawBackend {
    pub(crate) fn new() -> Self {
        Self {
            stores: Arc::new(StoreLawStores {
                binding: crate::StoreBindingId::new("conformance-store-law"),
                clock: Arc::new(crate::facade_support::SystemClock),
            }),
        }
    }

    pub(crate) fn into_backend(self) -> crate::Backend {
        crate::Backend::for_testing(self.stores)
    }

    /// A store set whose every port panics: what a law reaches only for
    /// a backend it never runs.
    #[cfg(test)]
    pub(crate) fn stores() -> Arc<dyn crate::StoreSet> {
        Self::new().stores
    }

    pub(crate) fn host_config(
        self,
        commit_budget: crate::CommitBudget,
        queued_work_batching: crate::QueuedWorkBatchingConfig,
    ) -> crate::RuntimeHostConfig {
        crate::RuntimeHostConfig::new(self.into_backend(), commit_budget, queued_work_batching)
    }
}

struct StoreLawStores {
    binding: crate::StoreBindingId,
    clock: Arc<dyn crate::Clock>,
}

impl StoreLawStores {
    fn no_second_substrate(port: &str) -> ! {
        panic!("a store law's runtime reaches no {port}: its substrate is the store it was handed")
    }
}

impl crate::StoreSet for StoreLawStores {
    fn durable_store(&self) -> Arc<dyn crate::DurableStore> {
        Self::no_second_substrate("durable store")
    }

    fn worker_recovery(&self) -> Arc<dyn lash_core::store::worker_recovery::WorkerRecoveryStore> {
        Self::no_second_substrate("worker recovery accounting")
    }

    fn binding_identity(&self) -> &crate::StoreBindingId {
        &self.binding
    }

    fn clock(&self) -> Arc<dyn crate::Clock> {
        Arc::clone(&self.clock)
    }

    fn session_store_factory(&self) -> Arc<dyn crate::DeploymentStore> {
        Self::no_second_substrate("session catalog")
    }
    /// Host construction captures the referrer port (the cleanup executor
    /// ends attachment edges through it). Every attachment write is still
    /// refused, by the unavailable attachment store below, so no edge can
    /// exist for these referrers to hold.
    fn attachment_referrers(&self) -> Arc<dyn crate::AttachmentReferrers> {
        Arc::new(crate::attachments::NoopAttachmentReferrers)
    }

    fn process_registry(&self) -> Arc<dyn crate::ProcessRegistry> {
        Self::no_second_substrate("process registry")
    }

    fn process_continuations(&self) -> Arc<dyn crate::ProcessContinuationStore> {
        Self::no_second_substrate("process continuation store")
    }

    fn trigger_store(&self) -> Arc<dyn crate::TriggerStore> {
        Self::no_second_substrate("trigger store")
    }

    fn tool_material_store(&self) -> Arc<dyn crate::store::ToolMaterialStore> {
        Self::no_second_substrate("tool material store")
    }

    fn process_env_store(&self) -> Arc<dyn crate::ProcessExecutionEnvStore> {
        Arc::new(crate::testing::UnavailableProcessExecutionEnvStore)
    }

    fn turn_prelude_store(&self) -> Arc<dyn crate::TurnPreludeStore> {
        Arc::new(crate::testing::UnavailableTurnPreludeStore)
    }

    fn attachment_store(&self) -> Arc<dyn crate::AttachmentStore> {
        Arc::new(crate::attachments::UnavailableAttachmentStore)
    }

    fn module_artifacts(&self) -> Arc<dyn crate::ModuleArtifactStore> {
        Arc::new(UnavailableModuleArtifacts)
    }

    fn definition_store(&self) -> Arc<dyn crate::ProcessDefinitionStore> {
        Arc::new(UnavailableProcessDefinitions)
    }

    fn recovery_leader(&self) -> Arc<dyn crate::store::RecoveryLeaderStore> {
        Self::no_second_substrate("recovery leader lease")
    }

    fn generation_drain(&self) -> Arc<dyn crate::store::generation_drain::GenerationDrainStore> {
        Self::no_second_substrate("generation drain store")
    }

    fn obligation_ledger(
        &self,
        _kind: crate::store::ObligationKind,
    ) -> Arc<dyn crate::store::ObligationLedger> {
        Self::no_second_substrate("obligation ledger")
    }

    fn artifact_cleanup(&self) -> Arc<dyn crate::store::ArtifactCleanupLedger> {
        Arc::new(UnavailableArtifactCleanup)
    }

    fn session_delete_ledger(&self) -> Arc<dyn crate::store::session_delete::SessionDeleteLedger> {
        Self::no_second_substrate("session delete ledger")
    }
}

// Host construction captures these ports even in store-only laws. Any use of
// them still fails at the boundary the law intentionally does not provide.
struct UnavailableModuleArtifacts;

#[async_trait::async_trait]
impl crate::ModuleArtifactStore for UnavailableModuleArtifacts {
    async fn publish_module_artifact(
        &self,
        _: &crate::ReferrerClaim,
        _: &str,
        _: &[u8],
    ) -> Result<(), crate::ArtifactStoreError> {
        StoreLawStores::no_second_substrate("Lashlang artifact store")
    }

    async fn acquire_module_artifact(
        &self,
        _: &crate::ReferrerClaim,
        _: &str,
    ) -> Result<(), crate::ArtifactStoreError> {
        StoreLawStores::no_second_substrate("Lashlang artifact store")
    }

    async fn end_module_referrer(
        &self,
        _: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        StoreLawStores::no_second_substrate("Lashlang artifact store")
    }

    async fn get_module_artifact(
        &self,
        _: &str,
    ) -> Result<Option<Vec<u8>>, crate::ArtifactStoreError> {
        StoreLawStores::no_second_substrate("Lashlang artifact store")
    }
}

struct UnavailableProcessDefinitions;

#[async_trait::async_trait]
impl crate::ProcessDefinitionStore for UnavailableProcessDefinitions {
    async fn publish_process_definition(
        &self,
        _: &crate::ReferrerClaim,
        _: &crate::ProcessDefinitionId,
        _: &[u8],
        _: &[crate::ArtifactName],
    ) -> Result<(), crate::ArtifactStoreError> {
        StoreLawStores::no_second_substrate("process-definition store")
    }

    async fn acquire_process_definition(
        &self,
        _: &crate::ReferrerClaim,
        _: &crate::ProcessDefinitionId,
        _: &[crate::ArtifactName],
    ) -> Result<(), crate::ArtifactStoreError> {
        StoreLawStores::no_second_substrate("process-definition store")
    }

    async fn end_process_definition_referrer(
        &self,
        _: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        StoreLawStores::no_second_substrate("process-definition store")
    }

    async fn get_process_definition(
        &self,
        _: &crate::ProcessDefinitionId,
    ) -> Result<Option<Vec<u8>>, crate::ArtifactStoreError> {
        StoreLawStores::no_second_substrate("process-definition store")
    }
}

struct UnavailableArtifactCleanup;

#[async_trait::async_trait]
impl crate::store::ObligationLedger for UnavailableArtifactCleanup {
    fn kind(&self) -> crate::store::ObligationKind {
        crate::store::ObligationKind::ArtifactCleanup
    }

    async fn arm(
        &self,
        _: &crate::store::ObligationKey,
        _: u64,
    ) -> Result<Option<crate::store::ObligationId>, crate::StoreError> {
        StoreLawStores::no_second_substrate("artifact cleanup ledger")
    }

    async fn claim_due(
        &self,
        _: u64,
        _: u64,
        _: std::num::NonZeroUsize,
    ) -> Result<Vec<crate::store::ClaimedObligation>, crate::StoreError> {
        StoreLawStores::no_second_substrate("artifact cleanup ledger")
    }

    async fn claim(
        &self,
        _: &crate::store::ObligationId,
        _: &crate::store::ClaimToken,
        _: u64,
        _: u64,
    ) -> Result<Option<crate::store::ClaimedObligation>, crate::StoreError> {
        StoreLawStores::no_second_substrate("artifact cleanup ledger")
    }

    async fn settle(
        &self,
        _: &crate::store::ObligationId,
        _: &crate::store::ClaimToken,
        _: crate::store::ObligationSettlement,
        _: u64,
    ) -> Result<crate::store::SettleOutcome, crate::StoreError> {
        StoreLawStores::no_second_substrate("artifact cleanup ledger")
    }

    async fn rearm(
        &self,
        _: &crate::store::ObligationId,
        _: u64,
    ) -> Result<bool, crate::StoreError> {
        StoreLawStores::no_second_substrate("artifact cleanup ledger")
    }

    async fn list_stalled(
        &self,
        _: Option<&crate::store::ObligationId>,
        _: std::num::NonZeroUsize,
    ) -> Result<Vec<crate::store::StalledObligation>, crate::StoreError> {
        StoreLawStores::no_second_substrate("artifact cleanup ledger")
    }

    async fn count_stalled(&self) -> Result<u64, crate::StoreError> {
        StoreLawStores::no_second_substrate("artifact cleanup ledger")
    }

    async fn standing(
        &self,
        _: &crate::store::ObligationId,
    ) -> Result<Option<crate::store::ObligationStanding>, crate::StoreError> {
        StoreLawStores::no_second_substrate("artifact cleanup ledger")
    }
}

#[async_trait::async_trait]
impl crate::store::ArtifactCleanupLedger for UnavailableArtifactCleanup {
    async fn arm_cleanup(
        &self,
        _: &crate::ArtifactCleanup,
        _: u64,
    ) -> Result<crate::store::ObligationId, crate::StoreError> {
        StoreLawStores::no_second_substrate("artifact cleanup ledger")
    }

    async fn nudge(&self, _: &crate::ArtifactReferrer, _: u64) -> Result<bool, crate::StoreError> {
        StoreLawStores::no_second_substrate("artifact cleanup ledger")
    }

    async fn load_cleanup(
        &self,
        _: &crate::store::ObligationId,
    ) -> Result<Option<crate::ArtifactCleanup>, crate::StoreError> {
        StoreLawStores::no_second_substrate("artifact cleanup ledger")
    }
}
