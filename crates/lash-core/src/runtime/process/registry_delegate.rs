//! Wholesale per-concern delegation for registry decorators.
//!
//! A decorator over an inner [`ProcessRegistry`](super::registry::ProcessRegistry)
//! intercepts only the concern(s) whose behavior it changes and composes each
//! remaining concern with one macro invocation instead of hand-forwarding its
//! methods. Every method is forwarded explicitly (including provided ones) so
//! the inner backend's optimized overrides stay in effect through the
//! decorator.

/// Implement [`ProcessQuery`](super::registry_concerns::ProcessQuery) for `$wrapper` by
/// forwarding every method to the registry held in its `$inner` field.
macro_rules! delegate_process_query {
    ($wrapper:ty, $inner:ident) => {
        #[async_trait::async_trait]
        impl $crate::runtime::process::registry_concerns::ProcessQuery for $wrapper {
            async fn resolve_process_ref(
                &self,
                process_id: &str,
            ) -> Result<$crate::ProcessRef, $crate::PluginError> {
                self.$inner.resolve_process_ref(process_id).await
            }

            async fn get_process_ref(
                &self,
                process_ref: &$crate::ProcessRef,
            ) -> Result<Option<$crate::ProcessRecord>, $crate::PluginError> {
                self.$inner.get_process_ref(process_ref).await
            }

            async fn get_process(
                &self,
                process_id: &str,
            ) -> Result<Option<$crate::ProcessRecord>, $crate::PluginError> {
                self.$inner.get_process(process_id).await
            }

            async fn list_processes(
                &self,
                filter: &$crate::ProcessListFilter,
            ) -> Result<Vec<$crate::ProcessRecord>, $crate::PluginError> {
                self.$inner.list_processes(filter).await
            }

            async fn processes_changed_since(
                &self,
                cursor: $crate::ProcessChangeCursor,
                limit: usize,
            ) -> Result<
                (Vec<$crate::ProcessChange>, $crate::ProcessChangeCursor),
                $crate::PluginError,
            > {
                self.$inner.processes_changed_since(cursor, limit).await
            }

            async fn list_non_terminal_page(
                &self,
                limit: std::num::NonZeroUsize,
                continuation: Option<$crate::ProcessWorklistCursor>,
            ) -> Result<$crate::ProcessWorklistPage, $crate::PluginError> {
                self.$inner
                    .list_non_terminal_page(limit, continuation)
                    .await
            }

            async fn filter_unregistered_process_ids(
                &self,
                process_ids: &[String],
            ) -> Result<Vec<String>, $crate::PluginError> {
                self.$inner
                    .filter_unregistered_process_ids(process_ids)
                    .await
            }

            async fn filter_tombstoned_process_ids(
                &self,
                process_ids: &[String],
            ) -> Result<Vec<String>, $crate::PluginError> {
                self.$inner.filter_tombstoned_process_ids(process_ids).await
            }

            async fn live_reference_summary(
                &self,
            ) -> Result<Vec<$crate::ProcessLiveReferenceView>, $crate::PluginError> {
                self.$inner.live_reference_summary().await
            }

            async fn count_non_terminal_processes(&self) -> Result<usize, $crate::PluginError> {
                self.$inner.count_non_terminal_processes().await
            }
        }
    };
}
pub(crate) use delegate_process_query;

/// Implement [`ProcessObserverRegistry`](super::registry_concerns::ProcessObserverRegistry) for `$wrapper` by
/// forwarding every method to the registry held in its `$inner` field.
macro_rules! delegate_process_observer_registry {
    ($wrapper:ty, $inner:ident) => {
        #[async_trait::async_trait]
        impl $crate::runtime::process::registry_concerns::ProcessObserverRegistry for $wrapper {
            async fn add_observer(
                &self,
                session_id: &str,
                process_id: &str,
                by: $crate::ProcessObserverBy,
            ) -> Result<(), $crate::PluginError> {
                self.$inner.add_observer(session_id, process_id, by).await
            }

            async fn add_observer_ref(
                &self,
                session_id: &str,
                process_ref: &$crate::ProcessRef,
                by: $crate::ProcessObserverBy,
            ) -> Result<(), $crate::PluginError> {
                self.$inner
                    .add_observer_ref(session_id, process_ref, by)
                    .await
            }

            async fn remove_observer(
                &self,
                session_id: &str,
                process_id: &str,
                by: $crate::ProcessObserverBy,
            ) -> Result<(), $crate::PluginError> {
                self.$inner
                    .remove_observer(session_id, process_id, by)
                    .await
            }

            async fn transfer_observers(
                &self,
                from_session_id: &str,
                to_session_id: &str,
                process_ids: &[String],
                by: $crate::ProcessObserverBy,
            ) -> Result<(), $crate::PluginError> {
                self.$inner
                    .transfer_observers(from_session_id, to_session_id, process_ids, by)
                    .await
            }

            async fn list_observed_by(
                &self,
                session_id: &str,
                filter: &$crate::ProcessListFilter,
            ) -> Result<Vec<$crate::ProcessRecord>, $crate::PluginError> {
                self.$inner.list_observed_by(session_id, filter).await
            }

            async fn list_live_observed_by(
                &self,
                session_id: &str,
            ) -> Result<Vec<$crate::ProcessRecord>, $crate::PluginError> {
                self.$inner.list_live_observed_by(session_id).await
            }

            async fn is_observer(
                &self,
                session_id: &str,
                process_id: &str,
            ) -> Result<bool, $crate::PluginError> {
                self.$inner.is_observer(session_id, process_id).await
            }

            async fn observers_for_process(
                &self,
                process_id: &str,
            ) -> Result<Vec<$crate::SessionId>, $crate::PluginError> {
                self.$inner.observers_for_process(process_id).await
            }

            async fn retarget_subscription(
                &self,
                process_id: &str,
                target: Option<&str>,
            ) -> Result<(), $crate::PluginError> {
                self.$inner.retarget_subscription(process_id, target).await
            }

            async fn delete_session_process_state(
                &self,
                session_id: &str,
            ) -> Result<$crate::ProcessSessionDeleteReport, $crate::PluginError> {
                self.$inner.delete_session_process_state(session_id).await
            }
        }
    };
}
pub(crate) use delegate_process_observer_registry;

/// Implement [`ProcessToolIntents`](super::registry_concerns::ProcessToolIntents) for `$wrapper` by
/// forwarding every method to the registry held in its `$inner` field.
macro_rules! delegate_process_tool_intents {
    ($wrapper:ty, $inner:ident) => {
        #[async_trait::async_trait]
        impl $crate::runtime::process::registry_concerns::ProcessToolIntents for $wrapper {
            async fn admit_tool_intent_submission(
                &self,
                submission: $crate::ToolIntentSubmissionRecord,
            ) -> Result<$crate::ToolIntentSubmissionAdmission, $crate::PluginError> {
                self.$inner.admit_tool_intent_submission(submission).await
            }

            async fn complete_tool_intent_submission(
                &self,
                replay_key: &str,
                outcome: $crate::ToolIntentExecutionOutcome,
            ) -> Result<$crate::ToolIntentSubmissionRecord, $crate::PluginError> {
                self.$inner
                    .complete_tool_intent_submission(replay_key, outcome)
                    .await
            }

            async fn pending_tool_intent_parent_end(
                &self,
                session_id: &str,
                execution_scope_id: &str,
            ) -> Result<Vec<$crate::ToolIntentSubmissionRecord>, $crate::PluginError> {
                self.$inner
                    .pending_tool_intent_parent_end(session_id, execution_scope_id)
                    .await
            }

            async fn complete_tool_intent_parent_end(
                &self,
                replay_key: &str,
            ) -> Result<(), $crate::PluginError> {
                self.$inner
                    .complete_tool_intent_parent_end(replay_key)
                    .await
            }
        }
    };
}
pub(crate) use delegate_process_tool_intents;

/// Implement [`ProcessWakeOutbox`](super::registry_concerns::ProcessWakeOutbox) for `$wrapper` by
/// forwarding every method to the registry held in its `$inner` field.
macro_rules! delegate_process_wake_outbox {
    ($wrapper:ty, $inner:ident) => {
        #[async_trait::async_trait]
        impl $crate::runtime::process::registry_concerns::ProcessWakeOutbox for $wrapper {
            fn wake_delivery_config(&self) -> $crate::WakeDeliveryConfig {
                self.$inner.wake_delivery_config()
            }

            async fn claim_pending_wake_deliveries(
                &self,
                limit: usize,
            ) -> Result<Vec<$crate::WakeDelivery>, $crate::PluginError> {
                self.$inner.claim_pending_wake_deliveries(limit).await
            }

            async fn list_wake_deliveries(
                &self,
                state: Option<$crate::WakeDeliveryState>,
            ) -> Result<Vec<$crate::WakeDelivery>, $crate::PluginError> {
                self.$inner.list_wake_deliveries(state).await
            }

            async fn wake_delivery_report(
                &self,
            ) -> Result<$crate::WakeDeliveryReport, $crate::PluginError> {
                self.$inner.wake_delivery_report().await
            }

            async fn mark_wake_enqueued(
                &self,
                delivery_id: &str,
                claim_token: &str,
            ) -> Result<$crate::WakeDeliveryClaimOutcome, $crate::PluginError> {
                self.$inner
                    .mark_wake_enqueued(delivery_id, claim_token)
                    .await
            }

            async fn discard_wake_delivery(
                &self,
                delivery_id: &str,
                claim_token: &str,
                reason: $crate::WakeDiscardReason,
            ) -> Result<$crate::WakeDeliveryClaimOutcome, $crate::PluginError> {
                self.$inner
                    .discard_wake_delivery(delivery_id, claim_token, reason)
                    .await
            }

            async fn redrive_wake_delivery(
                &self,
                delivery_id: &str,
            ) -> Result<(), $crate::PluginError> {
                self.$inner.redrive_wake_delivery(delivery_id).await
            }

            async fn defer_wake_delivery(
                &self,
                delivery_id: &str,
                claim_token: &str,
                next_attempt_at_ms: u64,
            ) -> Result<$crate::WakeDeliveryClaimOutcome, $crate::PluginError> {
                self.$inner
                    .defer_wake_delivery(delivery_id, claim_token, next_attempt_at_ms)
                    .await
            }
        }
    };
}
pub(crate) use delegate_process_wake_outbox;

/// Implement [`ProcessLeases`](super::registry_concerns::ProcessLeases) for `$wrapper` by
/// forwarding every method to the registry held in its `$inner` field.
macro_rules! delegate_process_leases {
    ($wrapper:ty, $inner:ident) => {
        #[async_trait::async_trait]
        impl $crate::runtime::process::registry_concerns::ProcessLeases for $wrapper {
            async fn claim_process_lease(
                &self,
                process_id: &str,
                owner: &$crate::LeaseOwnerIdentity,
                lease_ttl_ms: u64,
            ) -> Result<$crate::ProcessLeaseClaimOutcome, $crate::PluginError> {
                self.$inner
                    .claim_process_lease(process_id, owner, lease_ttl_ms)
                    .await
            }

            async fn reclaim_process_lease(
                &self,
                process_id: &str,
                owner: &$crate::LeaseOwnerIdentity,
                observed_holder: &$crate::ProcessLease,
                lease_ttl_ms: u64,
            ) -> Result<$crate::ProcessLeaseClaimOutcome, $crate::PluginError> {
                self.$inner
                    .reclaim_process_lease(process_id, owner, observed_holder, lease_ttl_ms)
                    .await
            }

            async fn renew_process_lease(
                &self,
                lease: &$crate::ProcessLease,
                lease_ttl_ms: u64,
            ) -> Result<$crate::ProcessLease, $crate::PluginError> {
                self.$inner.renew_process_lease(lease, lease_ttl_ms).await
            }

            async fn get_process_lease(
                &self,
                process_id: &str,
            ) -> Result<Option<$crate::ProcessLease>, $crate::PluginError> {
                self.$inner.get_process_lease(process_id).await
            }

            async fn get_process_leases(
                &self,
                process_ids: &[$crate::ProcessId],
            ) -> Result<Vec<Option<$crate::ProcessLease>>, $crate::PluginError> {
                self.$inner.get_process_leases(process_ids).await
            }

            async fn complete_process_lease(
                &self,
                completion: &$crate::ProcessLeaseCompletion,
            ) -> Result<(), $crate::PluginError> {
                self.$inner.complete_process_lease(completion).await
            }
        }
    };
}
pub(crate) use delegate_process_leases;

/// Implement [`ProcessRetention`](super::registry_concerns::ProcessRetention) for `$wrapper` by
/// forwarding every method to the registry held in its `$inner` field.
macro_rules! delegate_process_retention {
    ($wrapper:ty, $inner:ident) => {
        #[async_trait::async_trait]
        impl $crate::runtime::process::registry_concerns::ProcessRetention for $wrapper {
            async fn compact_process_tombstones(
                &self,
                cutoff_epoch_ms: u64,
                watermark: $crate::ProjectionWatermark,
                trigger_store: Option<&dyn $crate::TriggerStore>,
            ) -> Result<usize, $crate::PluginError> {
                self.$inner
                    .compact_process_tombstones(cutoff_epoch_ms, watermark, trigger_store)
                    .await
            }

            async fn prune_terminal_processes(
                &self,
                cutoff_epoch_ms: u64,
                filter: Option<$crate::ProcessListFilter>,
                watermark: $crate::ProjectionWatermark,
            ) -> Result<$crate::ProcessPruneReport, $crate::PluginError> {
                self.$inner
                    .prune_terminal_processes(cutoff_epoch_ms, filter, watermark)
                    .await
            }

            async fn prunable_terminal_processes(
                &self,
                cutoff_epoch_ms: u64,
                filter: Option<$crate::ProcessListFilter>,
                watermark: $crate::ProjectionWatermark,
            ) -> Result<Vec<String>, $crate::PluginError> {
                self.$inner
                    .prunable_terminal_processes(cutoff_epoch_ms, filter, watermark)
                    .await
            }
        }
    };
}
pub(crate) use delegate_process_retention;
