use super::*;
use lash_core::store::{
    ClaimToken, ClaimedObligation, ObligationId, ObligationKey, ObligationKind, ObligationLedger,
    ObligationSettlement, ObligationStanding, SettleOutcome, StalledObligation,
};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

struct PausedAskLedger {
    inner: Arc<dyn ObligationLedger>,
    armed: Arc<AtomicBool>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl ObligationLedger for PausedAskLedger {
    fn kind(&self) -> ObligationKind {
        self.inner.kind()
    }

    async fn arm(
        &self,
        key: &ObligationKey,
        now_ms: u64,
    ) -> std::result::Result<Option<ObligationId>, lash_core::StoreError> {
        self.inner.arm(key, now_ms).await
    }

    async fn claim_due(
        &self,
        now_ms: u64,
        claim_ttl_ms: u64,
        limit: NonZeroUsize,
    ) -> std::result::Result<Vec<ClaimedObligation>, lash_core::StoreError> {
        self.inner.claim_due(now_ms, claim_ttl_ms, limit).await
    }

    async fn claim(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> std::result::Result<Option<ClaimedObligation>, lash_core::StoreError> {
        self.inner.claim(id, token, now_ms, claim_ttl_ms).await
    }

    async fn settle(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        settlement: ObligationSettlement,
        now_ms: u64,
    ) -> std::result::Result<SettleOutcome, lash_core::StoreError> {
        self.inner.settle(id, token, settlement, now_ms).await
    }

    async fn rearm(
        &self,
        id: &ObligationId,
        now_ms: u64,
    ) -> std::result::Result<bool, lash_core::StoreError> {
        self.inner.rearm(id, now_ms).await
    }

    async fn list_stalled(
        &self,
        after: Option<&ObligationId>,
        limit: NonZeroUsize,
    ) -> std::result::Result<Vec<StalledObligation>, lash_core::StoreError> {
        self.inner.list_stalled(after, limit).await
    }

    async fn count_stalled(&self) -> std::result::Result<u64, lash_core::StoreError> {
        self.inner.count_stalled().await
    }

    async fn standing(
        &self,
        id: &ObligationId,
    ) -> std::result::Result<Option<ObligationStanding>, lash_core::StoreError> {
        let pause = self.armed.load(Ordering::SeqCst) && id.as_str().starts_with("ingress:qwb:");
        if pause {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.inner.standing(id).await
    }
}

#[tokio::test]
async fn cancelled_config_command_before_current_ask_is_typed() -> Result<()> {
    let armed = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(
        crate::tests::double_backend_explicit_reconcile().await,
    )
    .map_obligation_ledgers({
        let armed = Arc::clone(&armed);
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        move |kind, inner| {
            if kind == ObligationKind::Ingress {
                Arc::new(PausedAskLedger {
                    inner,
                    armed: Arc::clone(&armed),
                    entered: Arc::clone(&entered),
                    release: Arc::clone(&release),
                }) as Arc<dyn ObligationLedger>
            } else {
                inner
            }
        }
    })
    .with_session_work(Arc::new(lash_core::NoSessionWork::new()))
    .into_backend();
    let core = crate::tests::explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
    ))
    .provider(
        crate::testing::TestProvider::builder()
            .kind("admin-cancel-test")
            .build()
            .into_handle(),
    )
    .model(crate::tests::mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session_id = SessionId::from("cancelled-config-before-ask");
    let session = core.session(&session_id).open().await?;
    let store = lash_core::runtime::live_session_view(&core.store_factory, &session_id)
        .await?
        .expect("open session store");
    armed.store(true, Ordering::SeqCst);
    let admin = session.admin();
    let setter = tokio::spawn(async move {
        admin
            .config()
            .update(SessionConfigPatch {
                model: Some(crate::tests::model_spec(
                    "cancelled-next-model",
                    None,
                    64_000,
                )),
                ..SessionConfigPatch::default()
            })
            .await
    });
    entered.notified().await;
    let batch = store
        .list_queued_work()
        .await?
        .into_iter()
        .find(lash_core::runtime::QueuedWorkBatch::is_session_command_work)
        .expect("config setter enqueued its command");
    assert!(
        store
            .cancel_queued_work_batch(&batch.batch_id)
            .await?
            .is_some()
    );
    assert!(!store.queued_work_batch_completed(&batch.batch_id).await?);
    release.notify_one();
    let error = setter
        .await
        .expect("setter task")
        .expect_err("cancelled setter");
    assert!(
        matches!(error, EmbedError::Session(SessionError::SessionCommandCancelled(ref got)) if got.batch_id == batch.batch_id),
        "cancelled config setter returned {error:?}"
    );
    Ok(())
}
