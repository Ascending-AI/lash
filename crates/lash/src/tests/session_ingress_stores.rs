use super::*;

// The in-memory doubles run no session drive, so they hold no ingress: every
// ingress operation refuses.

#[async_trait]
impl lash_core::store::SessionIngressStore for SnapshotStore {
    async fn enqueue_ingress_item(
        &self,
        _draft: lash_core::store::IngressItemDraft,
    ) -> std::result::Result<lash_core::store::IngressEnqueueOutcome, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "enqueue_ingress_item",
        })
    }

    async fn claim_session_commands(
        &self,
        _fence: &lash_core::store::DriveFence,
    ) -> std::result::Result<Option<lash_core::store::IngressClaim>, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "claim_session_commands",
        })
    }

    async fn claim_turn_items(
        &self,
        _fence: &lash_core::store::DriveFence,
        _mode: lash_core::store::ClaimMode,
        _policy: &lash_core::store::IngressClaimPolicy,
    ) -> std::result::Result<Option<lash_core::store::IngressClaim>, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "claim_turn_items",
        })
    }

    async fn reclaim_ingress_claim(
        &self,
        _fence: &lash_core::store::DriveFence,
        _claim: &lash_core::store::IngressClaim,
    ) -> std::result::Result<lash_core::store::IngressReclaimOutcome, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "reclaim_ingress_claim",
        })
    }

    async fn abandon_ingress_claim(
        &self,
        _fence: &lash_core::store::DriveFence,
        _claim: &lash_core::store::IngressClaim,
    ) -> std::result::Result<(), lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "abandon_ingress_claim",
        })
    }

    async fn withdraw_ingress_items(
        &self,
        _session_id: &SessionId,
        _targets: &[lash_core::store::IngressWithdrawTarget],
    ) -> std::result::Result<Vec<lash_core::store::IngressWithdrawReceipt>, lash_core::StoreError>
    {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "withdraw_ingress_items",
        })
    }

    async fn withdraw_ingress_suffix(
        &self,
        _session_id: &SessionId,
        _anchor: &lash_core::store::IngressWithdrawTarget,
    ) -> std::result::Result<lash_core::store::IngressSuffixWithdrawOutcome, lash_core::StoreError>
    {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "withdraw_ingress_suffix",
        })
    }

    async fn list_ingress_items(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<Vec<lash_core::store::IngressItemRead>, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "list_ingress_items",
        })
    }

    async fn vacuum_session_ingress(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<u64, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "vacuum_session_ingress",
        })
    }
}

#[async_trait]
impl lash_core::store::SessionIngressStore for BoundSessionStore {
    async fn enqueue_ingress_item(
        &self,
        _draft: lash_core::store::IngressItemDraft,
    ) -> std::result::Result<lash_core::store::IngressEnqueueOutcome, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "enqueue_ingress_item",
        })
    }

    async fn claim_session_commands(
        &self,
        _fence: &lash_core::store::DriveFence,
    ) -> std::result::Result<Option<lash_core::store::IngressClaim>, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "claim_session_commands",
        })
    }

    async fn claim_turn_items(
        &self,
        _fence: &lash_core::store::DriveFence,
        _mode: lash_core::store::ClaimMode,
        _policy: &lash_core::store::IngressClaimPolicy,
    ) -> std::result::Result<Option<lash_core::store::IngressClaim>, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "claim_turn_items",
        })
    }

    async fn reclaim_ingress_claim(
        &self,
        _fence: &lash_core::store::DriveFence,
        _claim: &lash_core::store::IngressClaim,
    ) -> std::result::Result<lash_core::store::IngressReclaimOutcome, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "reclaim_ingress_claim",
        })
    }

    async fn abandon_ingress_claim(
        &self,
        _fence: &lash_core::store::DriveFence,
        _claim: &lash_core::store::IngressClaim,
    ) -> std::result::Result<(), lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "abandon_ingress_claim",
        })
    }

    async fn withdraw_ingress_items(
        &self,
        _session_id: &SessionId,
        _targets: &[lash_core::store::IngressWithdrawTarget],
    ) -> std::result::Result<Vec<lash_core::store::IngressWithdrawReceipt>, lash_core::StoreError>
    {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "withdraw_ingress_items",
        })
    }

    async fn withdraw_ingress_suffix(
        &self,
        _session_id: &SessionId,
        _anchor: &lash_core::store::IngressWithdrawTarget,
    ) -> std::result::Result<lash_core::store::IngressSuffixWithdrawOutcome, lash_core::StoreError>
    {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "withdraw_ingress_suffix",
        })
    }

    async fn list_ingress_items(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<Vec<lash_core::store::IngressItemRead>, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "list_ingress_items",
        })
    }

    async fn vacuum_session_ingress(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<u64, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "vacuum_session_ingress",
        })
    }
}
