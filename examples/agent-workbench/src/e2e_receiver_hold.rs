//! The receiver-side declaration hold: a process registry decorator whose
//! event appends first post the emitting body delivery to the case's
//! body-callback endpoint and wait for the controller to release it. Holding
//! the realizer's append blocks only the held rank's realization where a wire
//! cut would head-of-line block the shared invocation stream.
//!
//! Every trait method forwards to `inner`, including the concern traits'
//! provided/default methods, so the decorated backend's own overrides stay in
//! effect. Only [`ProcessEventLog`]'s append methods intercept.
use std::num::NonZeroUsize;
use std::result::Result;
use std::sync::Arc;

use crate::e2e_tools::ReceiverHold;
use lash::durability::{PreparedProcessRegistration, RuntimeReplayAttribution};
use lash::persistence::*;
use lash::plugins::PluginError;
use lash::process::*;
use lash::runtime::Clock;
use lash::tools::{
    ToolIntentExecutionOutcome, ToolIntentSubmissionAdmission, ToolIntentSubmissionRecord,
};
use lash::triggers::TriggerStore;
use lash::{ProcessId, SessionId};

/// The store set a held workbench serves: every port is the inner set's
/// except the process registry, which is [`ReceiverHoldRegistry`].
struct ReceiverHoldStores {
    inner: Arc<dyn lash::StoreSet>,
    registry: Arc<dyn lash::process::ProcessRegistry>,
}

impl lash::StoreSet for ReceiverHoldStores {
    fn binding_identity(&self) -> &lash::StoreBindingId {
        self.inner.binding_identity()
    }

    fn clock(&self) -> Arc<dyn Clock> {
        self.inner.clock()
    }

    fn session_store_factory(&self) -> Arc<dyn DeploymentStore> {
        self.inner.session_store_factory()
    }

    fn attachment_referrers(&self) -> Arc<dyn AttachmentReferrers> {
        self.inner.attachment_referrers()
    }

    fn durable_store(&self) -> Arc<dyn lash::durable::DurableStore> {
        self.inner.durable_store()
    }

    fn durable_signals(&self) -> Option<Arc<dyn lash::durable::Signals>> {
        self.inner.durable_signals()
    }
    fn process_registry(&self) -> Arc<dyn lash::process::ProcessRegistry> {
        Arc::clone(&self.registry)
    }

    fn trigger_store(&self) -> Arc<dyn TriggerStore> {
        self.inner.trigger_store()
    }

    fn process_env_store(&self) -> Arc<dyn ProcessExecutionEnvStore> {
        self.inner.process_env_store()
    }

    fn turn_prelude_store(&self) -> Arc<dyn lash::persistence::TurnPreludeStore> {
        self.inner.turn_prelude_store()
    }

    fn tool_material_store(&self) -> Arc<dyn ToolMaterialStore> {
        self.inner.tool_material_store()
    }

    fn definition_store(&self) -> Arc<dyn ProcessDefinitionStore> {
        self.inner.definition_store()
    }

    fn attachment_store(&self) -> Arc<dyn AttachmentStore> {
        self.inner.attachment_store()
    }

    fn module_artifacts(&self) -> Arc<dyn ModuleArtifactStore> {
        self.inner.module_artifacts()
    }

    fn recovery_leader(&self) -> Arc<dyn RecoveryLeaderStore> {
        self.inner.recovery_leader()
    }

    fn obligation_ledger(&self, kind: lash::ObligationKind) -> Arc<dyn ObligationLedger> {
        self.inner.obligation_ledger(kind)
    }

    fn artifact_cleanup(&self) -> Arc<dyn ArtifactCleanupLedger> {
        self.inner.artifact_cleanup()
    }
}

/// A process registry whose appends of the held event type first post the
/// emitting delivery to the body-callback `/DeclarationIssued` endpoint and
/// wait for its release; every other method is the inner registry's.
struct ReceiverHoldRegistry {
    inner: Arc<dyn lash::process::ProcessRegistry>,
    hold: Arc<ReceiverHold>,
}

impl ReceiverHoldRegistry {
    async fn held_append(&self, request: &ProcessEventAppendRequest) -> Result<(), PluginError> {
        self.hold
            .before_append(&request.event_type, &request.payload)
            .await
            .map_err(|error| PluginError::Session(error.to_string()))
    }
}

pub(crate) fn hold_stores(
    stores: Arc<dyn lash::StoreSet>,
    hold: ReceiverHold,
) -> Arc<dyn lash::StoreSet> {
    let registry: Arc<dyn lash::process::ProcessRegistry> = Arc::new(ReceiverHoldRegistry {
        inner: stores.process_registry(),
        hold: Arc::new(hold),
    });
    Arc::new(ReceiverHoldStores {
        inner: stores,
        registry,
    })
}

#[async_trait::async_trait]
impl FleetFormatStore for ReceiverHoldRegistry {
    fn fleet_format(&self) -> FleetFormat {
        self.inner.fleet_format()
    }
}

#[async_trait::async_trait]
impl ProcessQuery for ReceiverHoldRegistry {
    async fn require_process_id(&self, process_id: &ProcessId) -> Result<ProcessId, PluginError> {
        self.inner.require_process_id(process_id).await
    }
    async fn get_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<ProcessRecord>, PluginError> {
        self.inner.get_process(process_id).await
    }
    async fn get_process_by_start_key(
        &self,
        start_key: &StartKey,
    ) -> Result<Option<ProcessRecord>, PluginError> {
        self.inner.get_process_by_start_key(start_key).await
    }
    async fn list_processes(
        &self,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        self.inner.list_processes(filter).await
    }
    async fn processes_changed_since(
        &self,
        cursor: ProcessChangeCursor,
        limit: usize,
    ) -> Result<(Vec<ProcessChange>, ProcessChangeCursor), PluginError> {
        self.inner.processes_changed_since(cursor, limit).await
    }
    async fn list_non_terminal_processes_page(
        &self,
        limit: NonZeroUsize,
        continuation: Option<ProcessRegistryCursor>,
    ) -> Result<NonTerminalProcessPage, PluginError> {
        self.inner
            .list_non_terminal_processes_page(limit, continuation)
            .await
    }
    async fn filter_unregistered_process_ids(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<ProcessId>, PluginError> {
        self.inner
            .filter_unregistered_process_ids(process_ids)
            .await
    }
    async fn filter_tombstoned_process_ids(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<ProcessId>, PluginError> {
        self.inner.filter_tombstoned_process_ids(process_ids).await
    }
    async fn live_reference_summary(&self) -> Result<Vec<ProcessLiveReferenceView>, PluginError> {
        self.inner.live_reference_summary().await
    }
    async fn count_non_terminal_processes(&self) -> Result<usize, PluginError> {
        self.inner.count_non_terminal_processes().await
    }
}

#[async_trait::async_trait]
impl ProcessRegistrar for ReceiverHoldRegistry {
    async fn prepare_process_registration(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<PreparedProcessRegistration, PluginError> {
        self.inner
            .prepare_process_registration(registration, observers)
            .await
    }
    async fn commit_process_registration(
        &self,
        prepared: PreparedProcessRegistration,
        anchor: lash::tracing::TraceAnchor,
    ) -> Result<ProcessRegistrationReceipt, PluginError> {
        self.inner
            .commit_process_registration(prepared, anchor)
            .await
    }
    async fn register_process(
        &self,
        registration: ProcessRegistration,
    ) -> Result<ProcessRecord, PluginError> {
        self.inner.register_process(registration).await
    }
    async fn register_process_with_observers(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<ProcessRecord, PluginError> {
        self.inner
            .register_process_with_observers(registration, observers)
            .await
    }
    async fn register_process_reporting_outcome(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<ProcessRegistrationReceipt, PluginError> {
        self.inner
            .register_process_reporting_outcome(registration, observers)
            .await
    }
    async fn set_external_ref(
        &self,
        process_id: &ProcessId,
        external_ref: ProcessExternalRef,
    ) -> Result<ProcessRecord, PluginError> {
        self.inner.set_external_ref(process_id, external_ref).await
    }
}

#[async_trait::async_trait]
impl ProcessObserverRegistry for ReceiverHoldRegistry {
    async fn add_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        self.inner.add_observer(session_id, process_id, by).await
    }
    async fn remove_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        self.inner.remove_observer(session_id, process_id, by).await
    }
    async fn transfer_observers(
        &self,
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: &[ProcessId],
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        self.inner
            .transfer_observers(from_session_id, to_session_id, process_ids, by)
            .await
    }
    async fn list_observed_by(
        &self,
        session_id: &SessionId,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        self.inner.list_observed_by(session_id, filter).await
    }
    async fn list_live_observed_by(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        self.inner.list_live_observed_by(session_id).await
    }
    async fn is_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
    ) -> Result<bool, PluginError> {
        self.inner.is_observer(session_id, process_id).await
    }
    async fn observers_for_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<Vec<SessionId>, PluginError> {
        self.inner.observers_for_process(process_id).await
    }
    async fn retarget_subscription(
        &self,
        process_id: &ProcessId,
        target: Option<&str>,
    ) -> Result<(), PluginError> {
        self.inner.retarget_subscription(process_id, target).await
    }
    async fn delete_session_process_state(
        &self,
        session_id: &SessionId,
    ) -> Result<ProcessSessionDeleteReport, PluginError> {
        self.inner.delete_session_process_state(session_id).await
    }
}

#[async_trait::async_trait]
impl ProcessEventLog for ReceiverHoldRegistry {
    async fn append_event(
        &self,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        self.held_append(&request).await?;
        self.inner.append_event(process_id, request).await
    }
    async fn append_event_with_authority(
        &self,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        self.held_append(&request).await?;
        self.inner
            .append_event_with_authority(process_id, request, authority)
            .await
    }
    async fn append_events(
        &self,
        process_id: &ProcessId,
        requests: Vec<ProcessEventAppendRequest>,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<Vec<ProcessEventAppendReceipt>, PluginError> {
        for request in &requests {
            self.held_append(request).await?;
        }
        self.inner
            .append_events(process_id, requests, authority)
            .await
    }
    async fn event_page_after(
        &self,
        process_id: &ProcessId,
        after_sequence: u64,
        limit: NonZeroUsize,
        mode: ProcessEventQueryMode,
    ) -> Result<ProcessEventReadOutcome<ProcessEventPage>, PluginError> {
        self.inner
            .event_page_after(process_id, after_sequence, limit, mode)
            .await
    }
    async fn event_page(
        &self,
        process_id: &ProcessId,
        limit: NonZeroUsize,
        mode: ProcessEventQueryMode,
    ) -> Result<ProcessEventReadOutcome<ProcessEventPage>, PluginError> {
        self.inner.event_page(process_id, limit, mode).await
    }
    async fn count_events_through(
        &self,
        process_id: &ProcessId,
        event_type: &str,
        up_to_sequence: u64,
    ) -> Result<u64, PluginError> {
        self.inner
            .count_events_through(process_id, event_type, up_to_sequence)
            .await
    }
    async fn recent_events(
        &self,
        process_id: &ProcessId,
        limit: usize,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        self.inner.recent_events(process_id, limit).await
    }
}

#[async_trait::async_trait]
impl ProcessLifecycle for ReceiverHoldRegistry {
    async fn complete_process(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        self.inner
            .complete_process(process_id, await_output, authority)
            .await
    }
    async fn complete_process_with_prelude(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        prelude: Vec<ProcessEventAppendRequest>,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        self.inner
            .complete_process_with_prelude(process_id, await_output, prelude, authority)
            .await
    }
    async fn get_parent_end_plan(
        &self,
        parent: &ScopeId,
    ) -> Result<Option<ParentEndPlan>, PluginError> {
        self.inner.get_parent_end_plan(parent).await
    }
    async fn record_first_started_with_authority(
        &self,
        process_id: &ProcessId,
        started: ProcessStarted,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessStartOutcome, PluginError> {
        self.inner
            .record_first_started_with_authority(process_id, started, authority)
            .await
    }
    async fn request_process_cancel(
        &self,
        process_id: &ProcessId,
        origin: CancelOrigin,
        requester: String,
        attribution: Option<RuntimeReplayAttribution>,
    ) -> Result<ProcessRecord, PluginError> {
        self.inner
            .request_process_cancel(process_id, origin, requester, attribution)
            .await
    }
    async fn request_process_cancel_reporting_realization(
        &self,
        process_id: &ProcessId,
        origin: CancelOrigin,
        requester: String,
        attribution: Option<RuntimeReplayAttribution>,
    ) -> Result<(ProcessRecord, StoreRealization), PluginError> {
        self.inner
            .request_process_cancel_reporting_realization(
                process_id,
                origin,
                requester,
                attribution,
            )
            .await
    }
    async fn set_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        wait: WaitState,
        prelude: Vec<ProcessEventAppendRequest>,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        self.inner
            .set_process_wait_with_authority(process_id, wait, prelude, authority)
            .await
    }
    async fn clear_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        prelude: Vec<ProcessEventAppendRequest>,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        self.inner
            .clear_process_wait_with_authority(process_id, prelude, authority)
            .await
    }
}

#[async_trait::async_trait]
impl ProcessToolIntents for ReceiverHoldRegistry {
    async fn admit_tool_intent_submission(
        &self,
        submission: ToolIntentSubmissionRecord,
    ) -> Result<ToolIntentSubmissionAdmission, PluginError> {
        self.inner.admit_tool_intent_submission(submission).await
    }
    async fn complete_tool_intent_submission(
        &self,
        replay_key: &str,
        outcome: ToolIntentExecutionOutcome,
    ) -> Result<StoreTransition<ToolIntentSubmissionRecord>, PluginError> {
        self.inner
            .complete_tool_intent_submission(replay_key, outcome)
            .await
    }
}

#[async_trait::async_trait]
impl ProcessRetention for ReceiverHoldRegistry {
    async fn release_process_events(
        &self,
        process_id: &ProcessId,
        through: u64,
    ) -> Result<ProcessEventRelease, PluginError> {
        self.inner.release_process_events(process_id, through).await
    }
    async fn compact_process_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: ProjectionWatermark,
        trigger_store: Option<&dyn TriggerStore>,
    ) -> Result<usize, PluginError> {
        self.inner
            .compact_process_tombstones(cutoff_epoch_ms, watermark, trigger_store)
            .await
    }
    async fn prune_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: ProjectionWatermark,
    ) -> Result<ProcessPruneReport, PluginError> {
        self.inner
            .prune_terminal_processes(cutoff_epoch_ms, filter, watermark)
            .await
    }
    async fn prunable_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: ProjectionWatermark,
    ) -> Result<Vec<ProcessId>, PluginError> {
        self.inner
            .prunable_terminal_processes(cutoff_epoch_ms, filter, watermark)
            .await
    }
    async fn release_consumer_hold(
        &self,
        process_id: &ProcessId,
        key: &str,
    ) -> Result<(), PluginError> {
        self.inner.release_consumer_hold(process_id, key).await
    }
    async fn abandon_consumer_hold(
        &self,
        key: &str,
        owner: &ScopeId,
    ) -> Result<Vec<ProcessId>, PluginError> {
        self.inner.abandon_consumer_hold(key, owner).await
    }
}

#[async_trait::async_trait]
impl ProcessClockRebind for ReceiverHoldRegistry {
    fn with_runtime_clock(
        &self,
        clock: Arc<dyn Clock>,
    ) -> Option<Arc<dyn lash::process::ProcessRegistry>> {
        self.inner.with_runtime_clock(clock).map(|inner| {
            Arc::new(ReceiverHoldRegistry {
                inner,
                hold: Arc::clone(&self.hold),
            }) as Arc<dyn lash::process::ProcessRegistry>
        })
    }
}
