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
                process_id: &ProcessId,
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
                process_id: &ProcessId,
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
                process_ids: &[ProcessId],
            ) -> Result<Vec<ProcessId>, $crate::PluginError> {
                self.$inner
                    .filter_unregistered_process_ids(process_ids)
                    .await
            }

            async fn filter_tombstoned_process_ids(
                &self,
                process_ids: &[ProcessId],
            ) -> Result<Vec<ProcessId>, $crate::PluginError> {
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

/// Implement [`ProcessRegistrar`](super::registry_concerns::ProcessRegistrar) for `$wrapper` by
/// forwarding every method to the registry held in its `$inner` field.
///
/// The supplied hooks wrap the forwarded registration and event-producing
/// operations so a decorator can retain its side effects without replacing
/// the delegation itself.
macro_rules! delegate_process_registrar {
    (
        $wrapper:ty,
        $inner:ident,
        registration |$registration_self:ident, $registration_process_id:ident, $registration_call:ident| $registration_hook:block,
        event |$event_self:ident, $event_process_id:ident, $event_call:ident| $event_hook:block
    ) => {
        #[async_trait::async_trait]
        impl $crate::runtime::process::registry_concerns::ProcessRegistrar for $wrapper {
            async fn register_process(
                &self,
                registration: $crate::ProcessRegistration,
            ) -> Result<$crate::ProcessRecord, $crate::PluginError> {
                let $registration_process_id = registration.id.clone();
                let $registration_self = self;
                let $registration_call = self.$inner.register_process(registration);
                $registration_hook
            }

            async fn register_process_with_observers(
                &self,
                registration: $crate::ProcessRegistration,
                observers: &[$crate::SessionId],
            ) -> Result<$crate::ProcessRecord, $crate::PluginError> {
                let $registration_process_id = registration.id.clone();
                let $registration_self = self;
                let $registration_call = self
                    .$inner
                    .register_process_with_observers(registration, observers);
                $registration_hook
            }

            fn bind_effect_host(&self, effect_host: &std::sync::Arc<dyn $crate::EffectHost>) {
                self.$inner.bind_effect_host(effect_host);
            }

            async fn set_external_ref(
                &self,
                process_id: &$crate::ProcessId,
                external_ref: $crate::ProcessExternalRef,
            ) -> Result<$crate::ProcessRecord, $crate::PluginError> {
                let $event_process_id = process_id;
                let $event_self = self;
                let $event_call = self.$inner.set_external_ref(process_id, external_ref);
                $event_hook
            }
        }
    };
}
pub(crate) use delegate_process_registrar;

/// Implement [`ProcessEventLog`](super::registry_concerns::ProcessEventLog) for `$wrapper` by
/// forwarding every method to the registry held in its `$inner` field.
///
/// The supplied hook wraps each event-producing operation. Incarnation-pinned
/// operations are still forwarded directly to the inner registry, so its
/// atomic pair check remains authoritative through the decorator.
macro_rules! delegate_process_event_log {
    (
        $wrapper:ty,
        $inner:ident,
        event |$event_self:ident, $event_process_id:ident, $event_call:ident| $event_hook:block
    ) => {
        #[async_trait::async_trait]
        impl $crate::runtime::process::registry_concerns::ProcessEventLog for $wrapper {
            async fn append_event(
                &self,
                process_id: &$crate::ProcessId,
                request: $crate::ProcessEventAppendRequest,
            ) -> Result<$crate::ProcessEventAppendReceipt, $crate::PluginError> {
                let $event_process_id = process_id;
                let $event_self = self;
                let $event_call = self.$inner.append_event(process_id, request);
                $event_hook
            }

            async fn append_event_ref(
                &self,
                process_ref: &$crate::ProcessRef,
                request: $crate::ProcessEventAppendRequest,
            ) -> Result<$crate::ProcessEventAppendReceipt, $crate::PluginError> {
                let $event_process_id = &process_ref.process_id;
                let $event_self = self;
                let $event_call = self.$inner.append_event_ref(process_ref, request);
                $event_hook
            }

            async fn append_event_with_authority(
                &self,
                process_id: &$crate::ProcessId,
                request: $crate::ProcessEventAppendRequest,
                authority: &$crate::ProcessExecutionWriteAuthority,
            ) -> Result<$crate::ProcessEventAppendReceipt, $crate::PluginError> {
                let $event_process_id = process_id;
                let $event_self = self;
                let $event_call = self
                    .$inner
                    .append_event_with_authority(process_id, request, authority);
                $event_hook
            }

            async fn events_after(
                &self,
                process_id: &$crate::ProcessId,
                after_sequence: u64,
            ) -> Result<Vec<$crate::ProcessEvent>, $crate::PluginError> {
                self.$inner.events_after(process_id, after_sequence).await
            }

            async fn events_after_ref(
                &self,
                process_ref: &$crate::ProcessRef,
                after_sequence: u64,
            ) -> Result<Vec<$crate::ProcessEvent>, $crate::PluginError> {
                self.$inner
                    .events_after_ref(process_ref, after_sequence)
                    .await
            }

            async fn count_events_through(
                &self,
                process_id: &$crate::ProcessId,
                event_type: &str,
                up_to_sequence: u64,
            ) -> Result<u64, $crate::PluginError> {
                self.$inner
                    .count_events_through(process_id, event_type, up_to_sequence)
                    .await
            }

            async fn count_events_through_ref(
                &self,
                process_ref: &$crate::ProcessRef,
                event_type: &str,
                up_to_sequence: u64,
            ) -> Result<u64, $crate::PluginError> {
                self.$inner
                    .count_events_through_ref(process_ref, event_type, up_to_sequence)
                    .await
            }

            async fn recent_events(
                &self,
                process_id: &$crate::ProcessId,
                limit: usize,
            ) -> Result<Vec<$crate::ProcessEvent>, $crate::PluginError> {
                self.$inner.recent_events(process_id, limit).await
            }
        }
    };
}
pub(crate) use delegate_process_event_log;

/// Implement [`ProcessLifecycle`](super::registry_concerns::ProcessLifecycle) for `$wrapper` by
/// forwarding every method to the registry held in its `$inner` field.
///
/// The supplied hook wraps operations that append lifecycle events. Plan reads
/// and settlement are forwarded without a hook because they do not append to
/// the process event log.
macro_rules! delegate_process_lifecycle {
    (
        $wrapper:ty,
        $inner:ident,
        event |$event_self:ident, $event_process_id:ident, $event_call:ident| $event_hook:block
    ) => {
        #[async_trait::async_trait]
        impl $crate::runtime::process::registry_concerns::ProcessLifecycle for $wrapper {
            async fn complete_process(
                &self,
                process_id: &$crate::ProcessId,
                await_output: $crate::ProcessAwaitOutput,
                authority: $crate::ProcessCompletionAuthority,
            ) -> Result<$crate::ProcessCompletionOutcome, $crate::PluginError> {
                let $event_process_id = process_id;
                let $event_self = self;
                let $event_call = self
                    .$inner
                    .complete_process(process_id, await_output, authority);
                $event_hook
            }

            async fn complete_process_with_parent_end(
                &self,
                process_id: &$crate::ProcessId,
                await_output: $crate::ProcessAwaitOutput,
                authority: $crate::ProcessCompletionAuthority,
                actions: Vec<$crate::ToolIntentParentEndAction>,
            ) -> Result<$crate::ProcessCompletionOutcome, $crate::PluginError> {
                let $event_process_id = process_id;
                let $event_self = self;
                let $event_call = self.$inner.complete_process_with_parent_end(
                    process_id,
                    await_output,
                    authority,
                    actions,
                );
                $event_hook
            }

            async fn complete_process_with_lease(
                &self,
                lease: &$crate::ProcessLease,
                await_output: $crate::ProcessAwaitOutput,
            ) -> Result<$crate::ProcessCompletionOutcome, $crate::PluginError> {
                let $event_process_id = &lease.process_id;
                let $event_self = self;
                let $event_call = self.$inner.complete_process_with_lease(lease, await_output);
                $event_hook
            }

            async fn complete_process_with_lease_and_parent_end(
                &self,
                lease: &$crate::ProcessLease,
                await_output: $crate::ProcessAwaitOutput,
                actions: Vec<$crate::ToolIntentParentEndAction>,
            ) -> Result<$crate::ProcessCompletionOutcome, $crate::PluginError> {
                let $event_process_id = &lease.process_id;
                let $event_self = self;
                let $event_call = self.$inner.complete_process_with_lease_and_parent_end(
                    lease,
                    await_output,
                    actions,
                );
                $event_hook
            }

            async fn list_pending_parent_end_plans(
                &self,
                limit: std::num::NonZeroUsize,
            ) -> Result<Vec<$crate::ProcessParentEndPlan>, $crate::PluginError> {
                self.$inner.list_pending_parent_end_plans(limit).await
            }

            async fn get_pending_parent_end_plan(
                &self,
                process_id: &$crate::ProcessId,
            ) -> Result<Option<$crate::ProcessParentEndPlan>, $crate::PluginError> {
                self.$inner.get_pending_parent_end_plan(process_id).await
            }

            async fn complete_parent_end_plan(
                &self,
                process_id: &$crate::ProcessId,
            ) -> Result<(), $crate::PluginError> {
                self.$inner.complete_parent_end_plan(process_id).await
            }

            async fn record_first_started_with_authority(
                &self,
                process_id: &$crate::ProcessId,
                started: $crate::ProcessStarted,
                authority: &$crate::ProcessExecutionWriteAuthority,
            ) -> Result<$crate::ProcessStartOutcome, $crate::PluginError> {
                let $event_process_id = process_id;
                let $event_self = self;
                let $event_call = self
                    .$inner
                    .record_first_started_with_authority(process_id, started, authority);
                $event_hook
            }

            async fn request_process_abandon(
                &self,
                process_id: &$crate::ProcessId,
                request: $crate::AbandonRequest,
            ) -> Result<$crate::ProcessRecord, $crate::PluginError> {
                let $event_process_id = process_id;
                let $event_self = self;
                let $event_call = self.$inner.request_process_abandon(process_id, request);
                $event_hook
            }

            async fn record_caller_departure(
                &self,
                process_id: &$crate::ProcessId,
            ) -> Result<$crate::ProcessRecord, $crate::PluginError> {
                let $event_process_id = process_id;
                let $event_self = self;
                let $event_call = self.$inner.record_caller_departure(process_id);
                $event_hook
            }

            async fn set_process_wait_with_authority(
                &self,
                process_id: &$crate::ProcessId,
                wait: $crate::WaitState,
                authority: &$crate::ProcessExecutionWriteAuthority,
            ) -> Result<$crate::ProcessRecord, $crate::PluginError> {
                let $event_process_id = process_id;
                let $event_self = self;
                let $event_call = self
                    .$inner
                    .set_process_wait_with_authority(process_id, wait, authority);
                $event_hook
            }

            async fn clear_process_wait_with_authority(
                &self,
                process_id: &$crate::ProcessId,
                authority: &$crate::ProcessExecutionWriteAuthority,
            ) -> Result<$crate::ProcessRecord, $crate::PluginError> {
                let $event_process_id = process_id;
                let $event_self = self;
                let $event_call = self
                    .$inner
                    .clear_process_wait_with_authority(process_id, authority);
                $event_hook
            }
        }
    };
}
pub(crate) use delegate_process_lifecycle;

/// Implement [`ProcessObserverRegistry`](super::registry_concerns::ProcessObserverRegistry) for `$wrapper` by
/// forwarding every method to the registry held in its `$inner` field.
macro_rules! delegate_process_observer_registry {
    ($wrapper:ty, $inner:ident) => {
        #[async_trait::async_trait]
        impl $crate::runtime::process::registry_concerns::ProcessObserverRegistry for $wrapper {
            async fn add_observer(
                &self,
                session_id: &SessionId,
                process_id: &ProcessId,
                by: $crate::ProcessObserverBy,
            ) -> Result<(), $crate::PluginError> {
                self.$inner.add_observer(session_id, process_id, by).await
            }

            async fn add_observer_ref(
                &self,
                session_id: &SessionId,
                process_ref: &$crate::ProcessRef,
                by: $crate::ProcessObserverBy,
            ) -> Result<(), $crate::PluginError> {
                self.$inner
                    .add_observer_ref(session_id, process_ref, by)
                    .await
            }

            async fn remove_observer(
                &self,
                session_id: &SessionId,
                process_id: &ProcessId,
                by: $crate::ProcessObserverBy,
            ) -> Result<(), $crate::PluginError> {
                self.$inner
                    .remove_observer(session_id, process_id, by)
                    .await
            }

            async fn transfer_observers(
                &self,
                from_session_id: &SessionId,
                to_session_id: &SessionId,
                process_ids: &[ProcessId],
                by: $crate::ProcessObserverBy,
            ) -> Result<(), $crate::PluginError> {
                self.$inner
                    .transfer_observers(from_session_id, to_session_id, process_ids, by)
                    .await
            }

            async fn list_observed_by(
                &self,
                session_id: &SessionId,
                filter: &$crate::ProcessListFilter,
            ) -> Result<Vec<$crate::ProcessRecord>, $crate::PluginError> {
                self.$inner.list_observed_by(session_id, filter).await
            }

            async fn list_live_observed_by(
                &self,
                session_id: &SessionId,
            ) -> Result<Vec<$crate::ProcessRecord>, $crate::PluginError> {
                self.$inner.list_live_observed_by(session_id).await
            }

            async fn is_observer(
                &self,
                session_id: &SessionId,
                process_id: &ProcessId,
            ) -> Result<bool, $crate::PluginError> {
                self.$inner.is_observer(session_id, process_id).await
            }

            async fn observers_for_process(
                &self,
                process_id: &ProcessId,
            ) -> Result<Vec<$crate::SessionId>, $crate::PluginError> {
                self.$inner.observers_for_process(process_id).await
            }

            async fn retarget_subscription(
                &self,
                process_id: &ProcessId,
                target: Option<&str>,
            ) -> Result<(), $crate::PluginError> {
                self.$inner.retarget_subscription(process_id, target).await
            }

            async fn delete_session_process_state(
                &self,
                session_id: &SessionId,
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
                session_id: &SessionId,
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
                process_id: &ProcessId,
                owner: &$crate::LeaseOwnerIdentity,
                lease_ttl_ms: u64,
            ) -> Result<$crate::ProcessLeaseClaimOutcome, $crate::PluginError> {
                self.$inner
                    .claim_process_lease(process_id, owner, lease_ttl_ms)
                    .await
            }

            async fn reclaim_process_lease(
                &self,
                process_id: &ProcessId,
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
                process_id: &ProcessId,
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
            async fn pending_process_artifact_cleanup(
                &self,
            ) -> Result<Vec<$crate::ProcessArtifactCleanup>, $crate::PluginError> {
                self.$inner.pending_process_artifact_cleanup().await
            }

            async fn complete_process_artifact_cleanup(
                &self,
                process_id: &$crate::ProcessId,
                incarnation: $crate::ProcessIncarnation,
            ) -> Result<$crate::ProcessArtifactCleanupAck, $crate::PluginError> {
                self.$inner
                    .complete_process_artifact_cleanup(process_id, incarnation)
                    .await
            }

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
            ) -> Result<Vec<ProcessId>, $crate::PluginError> {
                self.$inner
                    .prunable_terminal_processes(cutoff_epoch_ms, filter, watermark)
                    .await
            }
        }
    };
}
pub(crate) use delegate_process_retention;
