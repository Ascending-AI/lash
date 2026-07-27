use super::*;
use crate::{
    AttachmentStore, AttachmentStoreError, AttachmentStorePersistence, DurabilityTier,
    DurableStoreFacet, InMemoryAttachmentStore, ProcessExecutionEnvRef, ProcessExecutionEnvStore,
    ProcessInput, ProcessRegistration, ProcessRunOutcome, RuntimeEffectController, RuntimeError,
    StoredAttachment, TriggerStore,
};
use lash_sansio::{AttachmentCreateMeta, AttachmentId, AttachmentRef};

/// Effect controller that reports the durable tier; the worker boundary
/// only reads the tier, so the effect path is never exercised here.
#[derive(Default)]
struct DurableController;

impl crate::AwaitEventResolver for DurableController {
    fn durability_tier(&self) -> DurabilityTier {
        DurabilityTier::Durable
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for DurableController {
    async fn execute_effect(
        &self,
        _envelope: crate::RuntimeEffectEnvelope,
        _local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        unreachable!("worker boundary rejects before executing any effect")
    }
}

/// Attachment store reporting a durable tier over in-memory storage.
#[derive(Default)]
struct DurableAttachmentStore {
    inner: InMemoryAttachmentStore,
}

#[async_trait::async_trait]
impl AttachmentStore for DurableAttachmentStore {
    fn persistence(&self) -> AttachmentStorePersistence {
        AttachmentStorePersistence::Durable
    }

    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        self.inner.put(bytes, meta).await
    }

    async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        self.inner.get(id).await
    }

    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        self.inner.delete(id).await
    }

    async fn list(&self) -> Result<Vec<crate::StoredBlobRef>, AttachmentStoreError> {
        self.inner.list().await
    }
}

/// Process env store reporting a durable tier over in-memory storage.
#[derive(Default)]
struct DurableProcessEnvStore {
    inner: crate::InMemoryProcessExecutionEnvStore,
}

#[async_trait::async_trait]
impl ProcessExecutionEnvStore for DurableProcessEnvStore {
    fn durability_tier(&self) -> DurabilityTier {
        DurabilityTier::Durable
    }

    async fn put_process_execution_env(
        &self,
        env_ref: &ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), PluginError> {
        self.inner.put_process_execution_env(env_ref, bytes).await
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, PluginError> {
        self.inner.get_process_execution_env(env_ref).await
    }
}

/// Session store factory whose declared tier is configurable; it never has
/// to create a store because the worker boundary rejects first.
struct TierSessionStoreFactory {
    tier: DurabilityTier,
}

#[async_trait::async_trait]
impl SessionStoreFactory for TierSessionStoreFactory {
    fn durability_tier(&self) -> DurabilityTier {
        self.tier
    }

    async fn create_store(
        &self,
        _request: &crate::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::RuntimePersistence>, String> {
        unreachable!("worker boundary rejects before creating a session store")
    }

    async fn delete_session(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
}

struct TierTriggerStore {
    tier: DurabilityTier,
    inner: crate::InMemoryTriggerStore,
}

impl TierTriggerStore {
    fn new(tier: DurabilityTier) -> Self {
        Self {
            tier,
            inner: crate::InMemoryTriggerStore::default(),
        }
    }
}

#[async_trait::async_trait]
impl TriggerStore for TierTriggerStore {
    fn durability_tier(&self) -> DurabilityTier {
        self.tier
    }

    async fn execute_command(
        &self,
        operation_id: &str,
        command: crate::TriggerCommand,
    ) -> Result<crate::TriggerEffectResult, PluginError> {
        self.inner.execute_command(operation_id, command).await
    }

    async fn list_subscriptions(
        &self,
        filter: crate::TriggerSubscriptionFilter,
    ) -> Result<Vec<crate::TriggerSubscriptionRecord>, PluginError> {
        self.inner.list_subscriptions(filter).await
    }

    async fn delete_session_subscriptions(&self, session_id: &str) -> Result<usize, PluginError> {
        self.inner.delete_session_subscriptions(session_id).await
    }

    async fn ingest_occurrence(
        &self,
        request: crate::TriggerOccurrenceRequest,
    ) -> Result<crate::TriggerIngressResult, PluginError> {
        self.inner.ingest_occurrence(request).await
    }

    async fn list_occurrences(
        &self,
        filter: crate::TriggerOccurrenceFilter,
    ) -> Result<Vec<crate::TriggerOccurrenceRecord>, PluginError> {
        self.inner.list_occurrences(filter).await
    }

    async fn list_deliveries_by_occurrence_id(
        &self,
        occurrence_id: &str,
    ) -> Result<Vec<crate::TriggerDeliveryReservation>, PluginError> {
        self.inner
            .list_deliveries_by_occurrence_id(occurrence_id)
            .await
    }

    async fn list_deliveries_by_subscription_id(
        &self,
        subscription_id: &str,
    ) -> Result<Vec<crate::TriggerDeliveryReservation>, PluginError> {
        self.inner
            .list_deliveries_by_subscription_id(subscription_id)
            .await
    }

    async fn list_deliveries_by_process_id(
        &self,
        process_id: &str,
    ) -> Result<Vec<crate::TriggerDeliveryReservation>, PluginError> {
        self.inner.list_deliveries_by_process_id(process_id).await
    }

    async fn list_deliveries(&self) -> Result<Vec<crate::TriggerDeliveryReservation>, PluginError> {
        self.inner.list_deliveries().await
    }

    async fn prune_mutation_receipts(&self, cutoff_epoch_ms: u64) -> Result<usize, PluginError> {
        self.inner.prune_mutation_receipts(cutoff_epoch_ms).await
    }
}

/// Build a worker whose controller is durable but whose stores can be set
/// per-facet to durable/ephemeral, so each facet's loud rejection can be
/// exercised independently.
fn worker(
    attachment: Arc<dyn AttachmentStore>,
    process_env_store: Arc<dyn ProcessExecutionEnvStore>,
    session_store_tier: DurabilityTier,
) -> DurableProcessWorker {
    worker_with_store_tiers(
        attachment,
        process_env_store,
        session_store_tier,
        DurabilityTier::Durable,
        DurabilityTier::Durable,
    )
}

fn worker_with_store_tiers(
    attachment: Arc<dyn AttachmentStore>,
    process_env_store: Arc<dyn ProcessExecutionEnvStore>,
    session_store_tier: DurabilityTier,
    process_registry_tier: DurabilityTier,
    trigger_store_tier: DurabilityTier,
) -> DurableProcessWorker {
    let mut runtime_host = RuntimeHostConfig::in_memory();
    runtime_host.control.effect_host =
        Arc::new(crate::InlineEffectHost::new(Arc::new(DurableController)));
    runtime_host.durability.attachment_store =
        Arc::new(crate::SessionAttachmentStore::ephemeral(attachment));
    runtime_host.durability.process_env_store = process_env_store;
    let plugin_host = Arc::new(crate::PluginHost::new(Vec::new()));
    let factory: Arc<dyn SessionStoreFactory> = Arc::new(TierSessionStoreFactory {
        tier: session_store_tier,
    });
    let registry: Arc<dyn ProcessRegistry> = Arc::new(
        crate::TestLocalProcessRegistry::default().with_durability_tier(process_registry_tier),
    );
    let trigger_store: Arc<dyn TriggerStore> = Arc::new(TierTriggerStore::new(trigger_store_tier));
    DurableProcessWorker::new(
        DurableProcessWorkerConfig::new(plugin_host, runtime_host, factory, registry)
            .with_trigger_store(trigger_store),
    )
}

fn external_registration() -> ProcessRegistration {
    ProcessRegistration::new(
        "worker-boundary-process",
        ProcessInput::External {
            metadata: serde_json::json!({}),
        },
        crate::RecoveryDisposition::ExternallyOwned,
        crate::ProcessProvenance::host(),
    )
}

async fn run(worker: &DurableProcessWorker) -> Result<ProcessRunOutcome, PluginError> {
    let controller = DurableController;
    let scope = crate::ScopedEffectController::borrowed(
        &controller,
        crate::ExecutionScope::process("worker-boundary-process"),
    )
    .expect("scope process execution");
    worker
        .run_process_segment_with_scoped_effect_controller(
            external_registration(),
            ProcessExecutionContext::default(),
            crate::ProcessExecutionWriteAuthority::testing("external-process"),
            scope,
            CancellationToken::new(),
            None,
        )
        .await
}

fn assert_facet(err: PluginError, facet: DurableStoreFacet) {
    let PluginError::Session(message) = err else {
        panic!("expected PluginError::Session, got {err:?}");
    };
    let expected = RuntimeError::durable_store_required(facet).to_string();
    assert_eq!(message, expected, "worker must reject the {facet:?} facet");
}

#[tokio::test]
async fn durable_worker_rejects_ephemeral_attachment_store() {
    let worker = worker(
        Arc::new(InMemoryAttachmentStore::new()),
        Arc::new(DurableProcessEnvStore::default()),
        DurabilityTier::Durable,
    );
    let err = run(&worker)
        .await
        .expect_err("ephemeral attachment store must be rejected at the worker boundary");
    assert_facet(err, DurableStoreFacet::AttachmentStore);
}

#[tokio::test]
async fn durable_worker_rejects_ephemeral_process_env_store() {
    let worker = worker(
        Arc::new(DurableAttachmentStore::default()),
        Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
        DurabilityTier::Durable,
    );
    let err = run(&worker)
        .await
        .expect_err("ephemeral process env store must be rejected at the worker boundary");
    assert_facet(err, DurableStoreFacet::ProcessEnvStore);
}

#[tokio::test]
async fn durable_worker_rejects_ephemeral_session_store_factory() {
    let worker = worker(
        Arc::new(DurableAttachmentStore::default()),
        Arc::new(DurableProcessEnvStore::default()),
        DurabilityTier::Inline,
    );
    let err = run(&worker)
        .await
        .expect_err("ephemeral session store factory must be rejected at the worker boundary");
    assert_facet(err, DurableStoreFacet::SessionStore);
}

#[tokio::test]
async fn durable_worker_rejects_ephemeral_process_registry() {
    let worker = worker_with_store_tiers(
        Arc::new(DurableAttachmentStore::default()),
        Arc::new(DurableProcessEnvStore::default()),
        DurabilityTier::Durable,
        DurabilityTier::Inline,
        DurabilityTier::Durable,
    );
    let err = run(&worker)
        .await
        .expect_err("ephemeral process registry must be rejected at the worker boundary");
    assert_facet(err, DurableStoreFacet::ProcessRegistry);
}

#[tokio::test]
async fn durable_worker_rejects_ephemeral_trigger_store() {
    let worker = worker_with_store_tiers(
        Arc::new(DurableAttachmentStore::default()),
        Arc::new(DurableProcessEnvStore::default()),
        DurabilityTier::Durable,
        DurabilityTier::Durable,
        DurabilityTier::Inline,
    );
    let err = run(&worker)
        .await
        .expect_err("ephemeral trigger store must be rejected at the worker boundary");
    assert_facet(err, DurableStoreFacet::TriggerStore);
}

/// The store-facet guard passing is observed as the run reaching the
/// disposition guard next: the ExternallyOwned boundary registration is
/// rejected there, not at a facet, proving the facet guard cleared.
fn assert_reached_disposition_guard(err: PluginError) {
    let PluginError::Session(message) = err else {
        panic!("expected PluginError::Session, got {err:?}");
    };
    assert!(
        message.contains("externally-owned"),
        "run must clear the store-facet guard and reach the disposition guard, got: {message}"
    );
}

#[tokio::test]
async fn durable_worker_with_all_durable_stores_passes_store_facet_check() {
    // Positive control: a durable worker wired against fully durable stores
    // clears the store-facet guard and reaches the disposition guard, which
    // rejects the ExternallyOwned boundary registration (ADR 0019).
    let worker = worker(
        Arc::new(DurableAttachmentStore::default()),
        Arc::new(DurableProcessEnvStore::default()),
        DurabilityTier::Durable,
    );
    let err = run(&worker)
        .await
        .expect_err("externally-owned row is rejected after the facet guard");
    assert_reached_disposition_guard(err);
}

#[tokio::test]
async fn inline_worker_passes_store_facet_check_with_ephemeral_stores() {
    // Inline controllers impose no requirement, so an in-memory worker clears
    // the durable-first guard unchanged and reaches the disposition guard.
    let runtime_host = RuntimeHostConfig::in_memory();
    let plugin_host = Arc::new(crate::PluginHost::new(Vec::new()));
    let factory: Arc<dyn SessionStoreFactory> = Arc::new(TierSessionStoreFactory {
        tier: DurabilityTier::Inline,
    });
    let registry: Arc<dyn ProcessRegistry> = Arc::new(crate::TestLocalProcessRegistry::default());
    let worker = DurableProcessWorker::new(DurableProcessWorkerConfig::new(
        plugin_host,
        runtime_host,
        factory,
        registry,
    ));
    let err = run(&worker)
        .await
        .expect_err("externally-owned row is rejected after the facet guard");
    assert_reached_disposition_guard(err);
}
