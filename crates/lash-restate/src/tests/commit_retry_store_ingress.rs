//! The commit-retry store forwards the one session ingress unchanged.

use super::*;

use super::conformance_and_poison::CommitRetryStore;

#[async_trait::async_trait]
impl lash_core::store::SessionIngressStore for CommitRetryStore {
    async fn enqueue_ingress_item(
        &self,
        draft: lash_core::store::IngressItemDraft,
    ) -> Result<lash_core::store::IngressEnqueueOutcome, lash_core::StoreError> {
        self.inner.enqueue_ingress_item(draft).await
    }

    async fn claim_session_commands(
        &self,
        fence: &lash_core::store::DriveFence,
    ) -> Result<Option<lash_core::store::IngressClaim>, lash_core::StoreError> {
        self.inner.claim_session_commands(fence).await
    }

    async fn claim_turn_items(
        &self,
        fence: &lash_core::store::DriveFence,
        mode: lash_core::store::ClaimMode,
        policy: &lash_core::store::IngressClaimPolicy,
    ) -> Result<Option<lash_core::store::IngressClaim>, lash_core::StoreError> {
        self.inner.claim_turn_items(fence, mode, policy).await
    }

    async fn reclaim_ingress_claim(
        &self,
        fence: &lash_core::store::DriveFence,
        claim: &lash_core::store::IngressClaim,
    ) -> Result<lash_core::store::IngressReclaimOutcome, lash_core::StoreError> {
        self.inner.reclaim_ingress_claim(fence, claim).await
    }

    async fn abandon_ingress_claim(
        &self,
        fence: &lash_core::store::DriveFence,
        claim: &lash_core::store::IngressClaim,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.abandon_ingress_claim(fence, claim).await
    }

    async fn withdraw_ingress_items(
        &self,
        session_id: &SessionId,
        targets: &[lash_core::store::IngressWithdrawTarget],
    ) -> Result<Vec<lash_core::store::IngressWithdrawReceipt>, lash_core::StoreError> {
        self.inner.withdraw_ingress_items(session_id, targets).await
    }

    async fn withdraw_ingress_suffix(
        &self,
        session_id: &SessionId,
        anchor: &lash_core::store::IngressWithdrawTarget,
    ) -> Result<lash_core::store::IngressSuffixWithdrawOutcome, lash_core::StoreError> {
        self.inner.withdraw_ingress_suffix(session_id, anchor).await
    }

    async fn list_ingress_items(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core::store::IngressItemRead>, lash_core::StoreError> {
        self.inner.list_ingress_items(session_id).await
    }

    async fn vacuum_session_ingress(
        &self,
        session_id: &SessionId,
    ) -> Result<u64, lash_core::StoreError> {
        self.inner.vacuum_session_ingress(session_id).await
    }
}
