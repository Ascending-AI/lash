use std::num::NonZeroUsize;
use std::sync::Arc;

use lash_core::StoreError;
use lash_core::shift::relay::{DeliveryFailure, ObligationDelivery, ObligationRelay, RelayPolicy};
use lash_core::store::{
    ClaimToken, ClaimedObligation, ObligationId, ObligationKey, ObligationKind, ObligationLedger,
    ObligationSettlement, ObligationStanding, SettleOutcome, StalledObligation,
};
use tokio::sync::mpsc;

use lash_durable_test::SimClock;

pub(super) enum Progress {
    EmptyPass,
    Settled(ObligationSettlement),
}

/// Observe completed passes without replacing the real relay or SQLite ledger.
pub(super) fn observe(
    inner: Arc<dyn ObligationRelay>,
    clock: Arc<SimClock>,
    first_delivery_delay_ms: u64,
) -> (Arc<dyn ObligationRelay>, mpsc::UnboundedReceiver<Progress>) {
    let (settled, receiver) = mpsc::unbounded_channel();
    (
        Arc::new(Settlements {
            inner,
            settled,
            clock,
            first_delivery_delay_ms,
        }),
        receiver,
    )
}

struct Settlements {
    inner: Arc<dyn ObligationRelay>,
    settled: mpsc::UnboundedSender<Progress>,
    clock: Arc<SimClock>,
    first_delivery_delay_ms: u64,
}

#[async_trait::async_trait]
impl ObligationRelay for Settlements {
    fn ledger(&self) -> &dyn ObligationLedger {
        self
    }

    fn policy(&self) -> RelayPolicy {
        self.inner.policy()
    }

    fn consumer_settles(&self) -> bool {
        self.inner.consumer_settles()
    }

    async fn deliver(&self, delivery: ObligationDelivery<'_>) -> Result<(), DeliveryFailure> {
        if delivery.attempt == 1 && self.first_delivery_delay_ms > 0 {
            lash_core::Clock::sleep(
                self.clock.as_ref(),
                std::time::Duration::from_millis(self.first_delivery_delay_ms),
            )
            .await;
        }
        self.inner.deliver(delivery).await
    }
}

#[async_trait::async_trait]
impl ObligationLedger for Settlements {
    fn kind(&self) -> ObligationKind {
        self.inner.ledger().kind()
    }

    async fn arm(
        &self,
        key: &ObligationKey,
        now_ms: u64,
    ) -> Result<Option<ObligationId>, StoreError> {
        self.inner.ledger().arm(key, now_ms).await
    }

    async fn claim_due(
        &self,
        now_ms: u64,
        claim_ttl_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<ClaimedObligation>, StoreError> {
        let claimed = self
            .inner
            .ledger()
            .claim_due(now_ms, claim_ttl_ms, limit)
            .await?;
        if claimed.is_empty() {
            self.settled
                .send(Progress::EmptyPass)
                .expect("pass observer");
        }
        Ok(claimed)
    }

    async fn claim(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> Result<Option<ClaimedObligation>, StoreError> {
        self.inner
            .ledger()
            .claim(id, token, now_ms, claim_ttl_ms)
            .await
    }

    async fn settle(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        settlement: ObligationSettlement,
        now_ms: u64,
    ) -> Result<SettleOutcome, StoreError> {
        let result = self
            .inner
            .ledger()
            .settle(id, token, settlement.clone(), now_ms)
            .await?;
        assert_eq!(result, SettleOutcome::Applied);
        // On the law's current-thread executor, the pass returns in this
        // poll before the `SessionShifts` can receive the completed settlement.
        self.settled
            .send(Progress::Settled(settlement))
            .expect("settlement observer");
        Ok(result)
    }

    async fn rearm(&self, id: &ObligationId, now_ms: u64) -> Result<bool, StoreError> {
        self.inner.ledger().rearm(id, now_ms).await
    }

    async fn list_stalled(
        &self,
        after: Option<&ObligationId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<StalledObligation>, StoreError> {
        self.inner.ledger().list_stalled(after, limit).await
    }

    async fn count_stalled(&self) -> Result<u64, StoreError> {
        self.inner.ledger().count_stalled().await
    }

    async fn standing(&self, id: &ObligationId) -> Result<Option<ObligationStanding>, StoreError> {
        self.inner.ledger().standing(id).await
    }
}
