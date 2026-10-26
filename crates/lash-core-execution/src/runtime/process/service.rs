use lash_execution_contracts::plugin_error::PluginError;
use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::ProcessId;

use super::op_scope::ProcessOpScope;
use super::start_staging::StagedProcessStart;
use lash_execution_contracts::process::ProcessAwaitOutput;
use lash_execution_contracts::process::ProcessCancelReceipt;
use lash_execution_contracts::process::ProcessHandleView;
use lash_execution_contracts::process::ProcessListMode;
use lash_execution_contracts::process::ProcessStartOptions;
use lash_execution_contracts::process::ProcessStartRegistration;
use lash_execution_contracts::process::ProcessStartRequest;

/// Optional factory-scoped filter for the session process tools only.
///
/// Synchronous, in-process, no I/O, infallible. May only NARROW: called with
/// candidates already visible by observer edges; returns the subset to expose.
/// The decision MUST be pure per `(session, candidate)`: Lash may evaluate
/// candidates independently and the presence of siblings must not change a
/// candidate's result.
///
/// NEVER consulted by: the read model, projections, the wake driver, cleanup,
/// prune, admin/host reads. Tool layer only.
///
/// Lash emits structured decision evidence for each evaluation. Turn-scoped
/// outcomes are durable through normal recorded tool results; replay does not
/// require a separate policy log.
pub trait ProcessToolVisibilityFilter: Send + Sync {
    fn narrow(
        &self,
        session: &super::model::SessionId,
        candidates: &[super::model::ProcessId],
    ) -> Vec<super::model::ProcessId>;
}

#[async_trait::async_trait]
pub trait ProcessService: Send + Sync {
    /// Controller-free read view used by recorded leaf attempts. A session
    /// sees the processes it observes; a process sees the ones it started.
    async fn list_visible_for_attempt(
        &self,
        owner: &lash_vocabulary::identity::RuntimeOwner,
        mode: ProcessListMode,
    ) -> Result<Vec<lash_execution_contracts::process::ObservedProcess>, PluginError>;

    async fn start_from_request(
        &self,
        session_id: &SessionId,
        request: ProcessStartRequest,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessHandleView, PluginError>;

    /// Stage the single process start a recorded tool intent declares, as
    /// a store-local effect of the call that declares it (ADR 0132 §5): it
    /// is admitted and staged from the recorded intent payload alone, and
    /// only the commit that records the call's outcome registers it.
    /// Implementations must not consult live visibility, existence,
    /// terminal, or host policy state.
    ///
    /// A call rerun at its ordinal stages the start again under the same
    /// derived key; its earlier staging registered nothing, because its
    /// outcome never committed.
    async fn stage_recorded_start(
        &self,
        owner: &lash_vocabulary::identity::RuntimeOwner,
        request: ProcessStartRequest,
        scope: ProcessOpScope<'_>,
    ) -> Result<StagedProcessStart, PluginError>;

    /// Stage the registration admitted by a logical Run, as a store-local
    /// effect of the call that declared it, without repeating the tool's
    /// preparation or lifetime policy. The service supplies the process
    /// executor and storage ports inside the owning invocation.
    async fn stage_bound(
        &self,
        registration: ProcessStartRegistration,
        scope: ProcessOpScope<'_>,
    ) -> Result<StagedProcessStart, PluginError>;

    /// Cancel the process a logical Run's declared start launched, as that
    /// start's discharge: the registry's cancel request and its delivery to
    /// the process's live execution, and no journal command. The discharge
    /// runs inside the owner step that records it, and repeats on every
    /// replay that reaches it, so both writes are idempotent; a process that
    /// already ended, took another cancel, or was pruned needs nothing more.
    async fn cancel_bound(
        &self,
        process_id: &ProcessId,
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError>;

    async fn start(
        &self,
        session_id: &SessionId,
        registration: ProcessStartRegistration,
        options: ProcessStartOptions,
        scope: ProcessOpScope<'_>,
    ) -> Result<lash_execution_contracts::process::ObservedProcess, PluginError>;

    async fn await_process(
        &self,
        process_id: &ProcessId,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessAwaitOutput, PluginError>;

    async fn await_process_ref(
        &self,
        process_id: &ProcessId,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        self.await_process(process_id, scope).await
    }

    /// Releases a parked call's hold on the process whose terminal it
    /// consumed (ADR 0116 §3.6), keyed by the call's completion key id.
    ///
    /// Controller-free and idempotent: the release is a reconciliation write that every redrive may repeat, and nothing
    /// about replay depends on it. The default refuses: only a service whose
    /// starts register holds can release them.
    async fn release_consumer_hold(
        &self,
        process_id: &ProcessId,
        key: &str,
    ) -> Result<(), PluginError>;

    /// Marks a call's consumer hold `key`, owned by `owner`, abandoned and
    /// returns the processes it holds that the call owes a cancel,
    /// controller-free: what an opener that cancelled the call drains (ADR
    /// 0116 §3.4). A registration under the key is refused from then on. The
    /// default holds nothing.
    async fn abandon_consumer_hold(
        &self,
        key: &str,
        owner: &crate::ScopeId,
    ) -> Result<Vec<ProcessId>, PluginError>;

    async fn list_visible(
        &self,
        session_id: &SessionId,
        mode: ProcessListMode,
        scope: ProcessOpScope<'_>,
    ) -> Result<Vec<lash_execution_contracts::process::ObservedProcess>, PluginError>;

    /// A session may address the processes it observes; a process, the ones
    /// whose recorded ancestry names it as their immediate starter.
    async fn validate_visible(
        &self,
        owner: &lash_vocabulary::identity::RuntimeOwner,
        process_ids: &[ProcessId],
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError>;

    async fn cancel(
        &self,
        owner: &lash_vocabulary::identity::RuntimeOwner,
        process_id: &ProcessId,
        scope: ProcessOpScope<'_>,
    ) -> Result<lash_execution_contracts::process::ObservedProcess, PluginError>;

    /// Journal-first cancellation used only by the recorded intent protocol.
    async fn cancel_recorded_intent(
        &self,
        owner: &lash_vocabulary::identity::RuntimeOwner,
        process_id: &ProcessId,
        identity: crate::ToolIntentIdentity,
        scope: ProcessOpScope<'_>,
    ) -> Result<lash_execution_contracts::process::ObservedProcess, PluginError>;

    async fn cancel_all_visible(
        &self,
        session_id: &SessionId,
        scope: ProcessOpScope<'_>,
    ) -> Result<Vec<ProcessCancelReceipt>, PluginError> {
        let entries = self
            .list_visible(session_id, ProcessListMode::Live, scope.clone())
            .await?;
        let owner = lash_vocabulary::identity::RuntimeOwner::Session(session_id.clone());
        let mut cancelled = Vec::new();
        for record in entries {
            if record.status().is_terminal() {
                continue;
            }
            cancelled.push(
                self.cancel(&owner, &record.process_id, scope.clone())
                    .await
                    .and_then(ProcessCancelReceipt::from_observed)?,
            );
        }
        Ok(cancelled)
    }

    async fn transfer(
        &self,
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: Vec<ProcessId>,
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError>;
}
