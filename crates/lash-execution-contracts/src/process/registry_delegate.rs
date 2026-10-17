//! Wholesale per-concern delegation for registry decorators.
//!
//! A decorator over an inner [`ProcessRegistry`](super::registry::ProcessRegistry)
//! intercepts only the concern(s) whose behavior it changes and composes each
//! remaining concern with one macro invocation instead of hand-forwarding its
//! methods. Every method is forwarded explicitly (including provided ones) so
//! the inner backend's optimized overrides stay in effect through the
//! decorator.

#[macro_export]
macro_rules! delegate_process_query {
    ($wrapper:ty, $inner:ident) => {
        #[async_trait::async_trait]
        impl $crate::process::registry_concerns::ProcessQuery for $wrapper {
            async fn get_process(
                &self,
                process_id: &ProcessId,
            ) -> Result<Option<$crate::ProcessRecord>, $crate::PluginError> {
                self.$inner.get_process(process_id).await
            }

            async fn get_process_by_start_key(
                &self,
                start_key: &lash_core_store::process_identity::StartKey,
            ) -> Result<Option<$crate::ProcessRecord>, $crate::PluginError> {
                self.$inner.get_process_by_start_key(start_key).await
            }

            async fn list_processes_page(
                &self,
                filter: &$crate::ProcessListFilter,
                limit: std::num::NonZeroUsize,
                continuation: Option<$crate::ProcessRosterCursor>,
            ) -> Result<$crate::ProcessRosterRecords, $crate::PluginError> {
                self.$inner
                    .list_processes_page(filter, limit, continuation)
                    .await
            }

            async fn process_change_bounds(
                &self,
            ) -> Result<$crate::ProcessChangeBounds, $crate::PluginError> {
                self.$inner.process_change_bounds().await
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

            async fn list_non_terminal_processes_page(
                &self,
                limit: std::num::NonZeroUsize,
                continuation: Option<$crate::ProcessRegistryCursor>,
            ) -> Result<$crate::NonTerminalProcessPage, $crate::PluginError> {
                self.$inner
                    .list_non_terminal_processes_page(limit, continuation)
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
pub use delegate_process_query;

/// The minted process id a registration answered with.
///
/// A registration carries no id until the registrar mints one, so a decorator
/// that keys a side effect by process reads it off the forwarded result.
pub trait RegisteredProcess {
    fn registered_process_id(&self) -> &crate::ProcessId;
}

impl RegisteredProcess for crate::ProcessRecord {
    fn registered_process_id(&self) -> &crate::ProcessId {
        &self.id
    }
}

impl RegisteredProcess for crate::ProcessRegistrationReceipt {
    fn registered_process_id(&self) -> &crate::ProcessId {
        &self.record.id
    }
}

/// The supplied hooks wrap the forwarded registration and event-producing
/// operations so a decorator can retain its side effects without replacing
/// the delegation itself. A registration hook learns the minted id from the
/// forwarded result, through [`RegisteredProcess`].
#[macro_export]
macro_rules! delegate_process_registrar {
    (
        $wrapper:ty,
        $inner:ident,
        registration |$registration_self:ident, $registration_call:ident| $registration_hook:block,
        event |$event_self:ident, $event_process_id:ident, $event_call:ident| $event_hook:block
    ) => {
        #[async_trait::async_trait]
        impl $crate::process::registry_concerns::ProcessRegistrar for $wrapper {
            async fn register_process(
                &self,
                registration: $crate::ProcessRegistration,
            ) -> Result<$crate::ProcessRecord, $crate::PluginError> {
                let $registration_self = self;
                let $registration_call = self.$inner.register_process(registration);
                $registration_hook
            }

            async fn register_process_with_observers(
                &self,
                registration: $crate::ProcessRegistration,
                observers: &[$crate::SessionId],
            ) -> Result<$crate::ProcessRecord, $crate::PluginError> {
                let $registration_self = self;
                let $registration_call = self
                    .$inner
                    .register_process_with_observers(registration, observers);
                $registration_hook
            }

            async fn register_process_reporting_outcome(
                &self,
                registration: $crate::ProcessRegistration,
                observers: &[$crate::SessionId],
            ) -> Result<$crate::ProcessRegistrationReceipt, $crate::PluginError> {
                let $registration_self = self;
                let $registration_call = self
                    .$inner
                    .register_process_reporting_outcome(registration, observers);
                $registration_hook
            }

            async fn prepare_process_registration(
                &self,
                registration: $crate::ProcessRegistration,
                observers: &[$crate::SessionId],
            ) -> Result<$crate::PreparedProcessRegistration, $crate::PluginError> {
                self.$inner
                    .prepare_process_registration(registration, observers)
                    .await
            }
            async fn commit_process_registration(
                &self,
                prepared: $crate::PreparedProcessRegistration,
            ) -> Result<$crate::ProcessRegistrationReceipt, $crate::PluginError> {
                let $registration_self = self;
                let $registration_call = self.$inner.commit_process_registration(prepared);
                $registration_hook
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
pub use delegate_process_registrar;

/// The event log's reads, forwarded: lash appends every lifecycle event an
/// activation commits through its owner transaction, never through a registry.
#[macro_export]
macro_rules! delegate_process_event_log {
    ($wrapper:ty, $inner:ident) => {
        #[async_trait::async_trait]
        impl $crate::process::registry_concerns::ProcessEventLog for $wrapper {
            async fn event_by_replay_key(
                &self,
                process_id: &$crate::ProcessId,
                replay_key: &str,
                through_sequence: u64,
            ) -> Result<
                $crate::ProcessEventReadOutcome<Option<$crate::ProcessEvent>>,
                $crate::PluginError,
            > {
                self.$inner
                    .event_by_replay_key(process_id, replay_key, through_sequence)
                    .await
            }

            async fn event_page_after(
                &self,
                process_id: &$crate::ProcessId,
                after_sequence: u64,
                limit: std::num::NonZeroUsize,
                mode: $crate::ProcessEventQueryMode,
            ) -> Result<
                $crate::ProcessEventReadOutcome<$crate::ProcessEventPage>,
                $crate::PluginError,
            > {
                self.$inner
                    .event_page_after(process_id, after_sequence, limit, mode)
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
pub use delegate_process_event_log;

/// The supplied hook wraps operations that append lifecycle events. Plan reads
/// and settlement are forwarded without a hook because they do not append to
/// the process event log.
#[macro_export]
macro_rules! delegate_process_lifecycle {
    (
        $wrapper:ty,
        $inner:ident,
        event |$event_self:ident, $event_process_id:ident, $event_call:ident| $event_hook:block
    ) => {
        #[async_trait::async_trait]
        impl $crate::process::registry_concerns::ProcessLifecycle for $wrapper {
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

            async fn complete_process_with_prelude(
                &self,
                process_id: &$crate::ProcessId,
                await_output: $crate::ProcessAwaitOutput,
                prelude: Vec<$crate::ProcessEventAppendRequest>,
                authority: $crate::ProcessCompletionAuthority,
            ) -> Result<$crate::ProcessCompletionOutcome, $crate::PluginError> {
                let $event_process_id = process_id;
                let $event_self = self;
                let $event_call = self.$inner.complete_process_with_prelude(
                    process_id,
                    await_output,
                    prelude,
                    authority,
                );
                $event_hook
            }

            async fn get_parent_end_plan(
                &self,
                parent: &lash_core_store::scope_identity::ScopeId,
            ) -> Result<Option<$crate::ParentEndPlan>, $crate::PluginError> {
                self.$inner.get_parent_end_plan(parent).await
            }

            async fn request_process_cancel(
                &self,
                process_id: &$crate::ProcessId,
                origin: lash_protocol_types::CancelOrigin,
                requester: String,
                attribution: Option<lash_core_store::effect_identity::RuntimeReplayAttribution>,
            ) -> Result<$crate::ProcessRecord, $crate::PluginError> {
                let $event_process_id = process_id;
                let $event_self = self;
                let $event_call =
                    self.$inner
                        .request_process_cancel(process_id, origin, requester, attribution);
                $event_hook
            }

            async fn request_process_cancel_reporting_realization(
                &self,
                process_id: &$crate::ProcessId,
                origin: lash_protocol_types::CancelOrigin,
                requester: String,
                attribution: Option<lash_core_store::effect_identity::RuntimeReplayAttribution>,
            ) -> Result<($crate::ProcessRecord, $crate::StoreRealization), $crate::PluginError>
            {
                let $event_process_id = process_id;
                let $event_self = self;
                let $event_call = self.$inner.request_process_cancel_reporting_realization(
                    process_id,
                    origin,
                    requester,
                    attribution,
                );
                $event_hook
            }
        }
    };
}
pub use delegate_process_lifecycle;

#[macro_export]
macro_rules! delegate_process_observer_registry {
    ($wrapper:ty, $inner:ident $(, $delete:ident)?) => {
        #[async_trait::async_trait]
        impl  $crate::process::registry_concerns::ProcessObserverRegistry for $wrapper {
            async fn add_observer(
                &self,
                session_id: &SessionId,
                process_id: &ProcessId,
                by: $crate::ProcessObserverBy,
            ) -> Result<(), $crate::PluginError> {
                self.$inner.add_observer(session_id, process_id, by).await
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

            async fn delete_session_process_state(
                &self,
                session_id: &SessionId,
            ) -> Result<$crate::ProcessSessionDeleteReport, $crate::PluginError> {
                delegate_process_observer_registry!(@delete self, session_id, $inner $(, $delete)?)
            }
        }
    };
    (@delete $this:ident, $session:ident, $inner:ident) => {
        $this.$inner.delete_session_process_state($session).await
    };
    (@delete $this:ident, $session:ident, $inner:ident, $delete:ident) => {
        $this.$delete($session).await
    };
}
pub use delegate_process_observer_registry;

#[macro_export]
macro_rules! delegate_process_tool_intents {
    ($wrapper:ty, $inner:ident) => {
        #[async_trait::async_trait]
        impl $crate::process::registry_concerns::ProcessToolIntents for $wrapper {
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
            ) -> Result<
                lash_core_store::store::StoreTransition<$crate::ToolIntentSubmissionRecord>,
                $crate::PluginError,
            > {
                self.$inner
                    .complete_tool_intent_submission(replay_key, outcome)
                    .await
            }
        }
    };
}
pub use delegate_process_tool_intents;

#[macro_export]
macro_rules! delegate_process_retention {
    ($wrapper:ty, $inner:ident) => {
        #[async_trait::async_trait]
        impl $crate::process::registry_concerns::ProcessRetention for $wrapper {
            async fn compact_process_tombstones(
                &self,
                cutoff_epoch_ms: u64,
                watermark: $crate::ProjectionWatermark,
            ) -> Result<usize, $crate::PluginError> {
                self.$inner
                    .compact_process_tombstones(cutoff_epoch_ms, watermark)
                    .await
            }

            async fn release_process_events(
                &self,
                process_id: &$crate::ProcessId,
                through: u64,
            ) -> Result<$crate::ProcessEventRelease, $crate::PluginError> {
                self.$inner
                    .release_process_events(process_id, through)
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

            async fn release_consumer_hold(
                &self,
                process_id: &$crate::ProcessId,
                key: &str,
            ) -> Result<(), $crate::PluginError> {
                self.$inner.release_consumer_hold(process_id, key).await
            }

            async fn abandon_consumer_hold(
                &self,
                key: &str,
                owner: &lash_core_store::scope_identity::ScopeId,
            ) -> Result<Vec<$crate::ProcessId>, $crate::PluginError> {
                self.$inner.abandon_consumer_hold(key, owner).await
            }
        }
    };
}

pub use delegate_process_retention;
