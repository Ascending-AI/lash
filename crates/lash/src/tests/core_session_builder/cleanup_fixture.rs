use super::*;
use lash_core::*;

pub(super) struct TriggerFault {
    pub inner: Arc<dyn TriggerStore>,
    pub error: StdMutex<Option<PluginError>>,
}
#[async_trait]
impl TriggerStore for TriggerFault {
    async fn execute_command(
        &self,
        operation_id: &str,
        command: TriggerCommand,
    ) -> std::result::Result<TriggerEffectResult, PluginError> {
        self.inner.execute_command(operation_id, command).await
    }
    async fn list_subscriptions(
        &self,
        filter: TriggerSubscriptionFilter,
    ) -> std::result::Result<Vec<TriggerSubscriptionRecord>, PluginError> {
        self.inner.list_subscriptions(filter).await
    }
    async fn delete_session_subscriptions(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<usize, PluginError> {
        let error = self.error.lock_recover().take();
        if let Some(error) = error {
            return Err(error);
        }
        self.inner.delete_session_subscriptions(session_id).await
    }
    async fn ingest_occurrence(
        &self,
        request: TriggerOccurrenceRequest,
    ) -> std::result::Result<TriggerIngressReceipt, PluginError> {
        self.inner.ingest_occurrence(request).await
    }
    async fn list_occurrences(
        &self,
        filter: TriggerOccurrenceFilter,
    ) -> std::result::Result<Vec<TriggerOccurrenceRecord>, PluginError> {
        self.inner.list_occurrences(filter).await
    }
    async fn list_deliveries_by_occurrence_id(
        &self,
        occurrence_id: &str,
    ) -> std::result::Result<Vec<TriggerDeliveryReservation>, PluginError> {
        self.inner
            .list_deliveries_by_occurrence_id(occurrence_id)
            .await
    }
    async fn list_deliveries_by_subscription_id(
        &self,
        subscription_id: &str,
    ) -> std::result::Result<Vec<TriggerDeliveryReservation>, PluginError> {
        self.inner
            .list_deliveries_by_subscription_id(subscription_id)
            .await
    }
    async fn list_deliveries_by_process_id(
        &self,
        process_id: &ProcessId,
    ) -> std::result::Result<Vec<TriggerDeliveryReservation>, PluginError> {
        self.inner.list_deliveries_by_process_id(process_id).await
    }
    async fn list_deliveries(
        &self,
    ) -> std::result::Result<Vec<TriggerDeliveryReservation>, PluginError> {
        self.inner.list_deliveries().await
    }
    async fn bind_delivery_process(
        &self,
        occurrence_id: &str,
        subscription_id: &str,
        process_id: &ProcessId,
    ) -> std::result::Result<(), PluginError> {
        self.inner
            .bind_delivery_process(occurrence_id, subscription_id, process_id)
            .await
    }
    async fn list_delivery_process_ids(&self) -> std::result::Result<Vec<ProcessId>, PluginError> {
        self.inner.list_delivery_process_ids().await
    }
    async fn list_delivery_retention_candidates(
        &self,
    ) -> std::result::Result<Vec<TriggerDeliveryRetentionCandidate>, PluginError> {
        self.inner.list_delivery_retention_candidates().await
    }
    async fn list_session_owner_ids_for_retention(
        &self,
    ) -> std::result::Result<Vec<SessionId>, PluginError> {
        self.inner.list_session_owner_ids_for_retention().await
    }
    async fn reconcile_trigger_retention(
        &self,
        candidates: &[TriggerDeliveryRetentionCandidate],
        deleted_session_ids: &[SessionId],
    ) -> std::result::Result<TriggerRetentionReconciliationReport, PluginError> {
        self.inner
            .reconcile_trigger_retention(candidates, deleted_session_ids)
            .await
    }
    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[TriggerDeliveryRetentionCandidate],
    ) -> std::result::Result<usize, PluginError> {
        self.inner
            .delete_delivery_retention_candidates(candidates)
            .await
    }
    async fn reclaim_trigger_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> TriggerOccurrenceReclamationResult {
        self.inner
            .reclaim_trigger_occurrences(cutoff_epoch_ms)
            .await
    }
    async fn prune_mutation_receipts(
        &self,
        cutoff_epoch_ms: u64,
    ) -> std::result::Result<usize, PluginError> {
        self.inner.prune_mutation_receipts(cutoff_epoch_ms).await
    }
    async fn prune_non_fired_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> std::result::Result<usize, PluginError> {
        self.inner
            .prune_non_fired_occurrences(cutoff_epoch_ms)
            .await
    }
}
