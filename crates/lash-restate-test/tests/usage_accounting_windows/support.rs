use lash_core::*;
use std::num::NonZeroU32;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

pub(super) struct ProjectionStore {
    pub(super) inner: Arc<dyn UsageAccountingStore>,
    pub(super) fail_first: bool,
    pub(super) attempts: Arc<AtomicUsize>,
    pub(super) failures: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl UsageAccountingStore for ProjectionStore {
    async fn admit_usage_run(
        &self,
        admission: &UsageRunAdmission,
    ) -> Result<UsageRunAdmitted, UsageAdmissionError> {
        self.inner.admit_usage_run(admission).await
    }
    async fn settle_usage(
        &self,
        settlement: &UsageSettlement,
        now_ms: u64,
    ) -> Result<UsageSettleReceipt, UsageAppendError> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 && self.fail_first {
            self.failures.fetch_add(1, Ordering::SeqCst);
            let facts = self
                .inner
                .load_usage_fact_page(
                    &settlement.owner,
                    None,
                    NonZeroU32::new(10).expect("nonzero"),
                )
                .await
                .expect("inspect the first projection attempt");
            assert!(
                facts.facts.is_empty(),
                "the first failed projection has written no fact"
            );
            return Err(
                StoreError::Backend("injected first projection failure before SQL".into()).into(),
            );
        }
        self.inner.settle_usage(settlement, now_ms).await
    }
    async fn mark_usage_settlement_conflicted(
        &self,
        settlement: &UsageSettlement,
        conflict: &UsageFactConflict,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        self.inner
            .mark_usage_settlement_conflicted(settlement, conflict, now_ms)
            .await
    }
    async fn append_usage_corrections(
        &self,
        owner: &RuntimeOwner,
        corrections: &[UsageCorrection],
        now_ms: u64,
    ) -> Result<UsageAppendReceipt, UsageAppendError> {
        self.inner
            .append_usage_corrections(owner, corrections, now_ms)
            .await
    }
    async fn retire_usage_execution(
        &self,
        owner: &RuntimeOwner,
        execution_scope_key: &str,
        now_ms: u64,
    ) -> Result<u64, StoreError> {
        self.inner
            .retire_usage_execution(owner, execution_scope_key, now_ms)
            .await
    }
    async fn retire_usage_owner(
        &self,
        owner: &RuntimeOwner,
        now_ms: u64,
    ) -> Result<UsageOwnerRetired, StoreError> {
        self.inner.retire_usage_owner(owner, now_ms).await
    }
    async fn load_owner_usage(&self, owner: &RuntimeOwner) -> Result<OwnerUsage, StoreError> {
        self.inner.load_owner_usage(owner).await
    }
    async fn load_usage_fact_page(
        &self,
        owner: &RuntimeOwner,
        after: Option<&UsageFactCursor>,
        limit: NonZeroU32,
    ) -> Result<UsageFactPage, StoreError> {
        self.inner.load_usage_fact_page(owner, after, limit).await
    }
    async fn load_usage_run_page(
        &self,
        owner: &RuntimeOwner,
        filter: UsageRunFilter,
        after: Option<&UsageRunCursor>,
        limit: NonZeroU32,
    ) -> Result<UsageRunPage, StoreError> {
        self.inner
            .load_usage_run_page(owner, filter, after, limit)
            .await
    }
}

pub(super) struct Stores {
    pub(super) inner: Arc<dyn StoreSet>,
    pub(super) usage: Arc<ProjectionStore>,
}
impl StoreSet for Stores {
    fn usage_accounting(&self) -> Arc<dyn UsageAccountingStore> {
        self.usage.clone()
    }
    fn binding_identity(&self) -> &StoreBindingId {
        self.inner.binding_identity()
    }
    fn clock(&self) -> Arc<dyn Clock> {
        self.inner.clock()
    }
    fn session_store_factory(&self) -> Arc<dyn DeploymentStore> {
        self.inner.session_store_factory()
    }
    fn attachment_referrers(&self) -> Arc<dyn store::AttachmentReferrers> {
        self.inner.attachment_referrers()
    }
    fn process_registry(&self) -> Arc<dyn ProcessRegistry> {
        self.inner.process_registry()
    }
    fn process_continuations(&self) -> Arc<dyn ProcessContinuationStore> {
        self.inner.process_continuations()
    }
    fn trigger_store(&self) -> Arc<dyn TriggerStore> {
        self.inner.trigger_store()
    }
    fn process_env_store(&self) -> Arc<dyn ProcessExecutionEnvStore> {
        self.inner.process_env_store()
    }
    fn definition_store(&self) -> Arc<dyn ProcessDefinitionStore> {
        self.inner.definition_store()
    }
    fn worker_recovery(&self) -> Arc<dyn store::worker_recovery::WorkerRecoveryStore> {
        self.inner.worker_recovery()
    }
    fn attachment_store(&self) -> Arc<dyn AttachmentStore> {
        self.inner.attachment_store()
    }
    fn module_artifacts(&self) -> Arc<dyn ModuleArtifactStore> {
        self.inner.module_artifacts()
    }
    fn recovery_leader(&self) -> Arc<dyn store::RecoveryLeaderStore> {
        self.inner.recovery_leader()
    }
    fn generation_drain(&self) -> Arc<dyn store::generation_drain::GenerationDrainStore> {
        self.inner.generation_drain()
    }
    fn session_delete_ledger(&self) -> Arc<dyn store::session_delete::SessionDeleteLedger> {
        self.inner.session_delete_ledger()
    }
    fn artifact_cleanup(&self) -> Arc<dyn store::ArtifactCleanupLedger> {
        self.inner.artifact_cleanup()
    }
    fn obligation_ledger(&self, kind: store::ObligationKind) -> Arc<dyn store::ObligationLedger> {
        self.inner.obligation_ledger(kind)
    }
}
