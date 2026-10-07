use crate::ProcessId;
use crate::SessionId;
use crate::plugin::PluginError;

use super::events::{ProcessAwaitOutput, ProcessEvent, ProcessSignal};
use super::model::{
    ProcessCancelReceipt, ProcessHandleView, ProcessListMode, ProcessRecord, ProcessStartOptions,
    ProcessStartRegistration, ProcessStartRequest,
};
use super::op_scope::ProcessOpScope;
use super::start_staging::StagedProcessStart;
use crate::runtime::actor::round::StoreLocalEffect;

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
        owner: &crate::RuntimeOwner,
        mode: ProcessListMode,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        let _ = (owner, mode);
        Err(PluginError::Session(
            "controller-free process reads are unavailable in this service".to_string(),
        ))
    }

    async fn start_from_request(
        &self,
        session_id: &SessionId,
        request: ProcessStartRequest,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessHandleView, PluginError> {
        let _ = (session_id, request, scope);
        Err(PluginError::Session(
            "process start request composition is unavailable in this service".to_string(),
        ))
    }

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
        owner: &crate::RuntimeOwner,
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
    ) -> Result<StagedProcessStart, PluginError> {
        let _ = (registration, scope);
        Err(PluginError::Session(
            "bound process starts are unavailable in this runtime".to_owned(),
        ))
    }

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
    ) -> Result<(), PluginError> {
        let _ = (process_id, scope);
        Err(PluginError::Session(
            "bound process cancellation is unavailable in this runtime".to_owned(),
        ))
    }

    async fn start(
        &self,
        session_id: &SessionId,
        registration: ProcessStartRegistration,
        options: ProcessStartOptions,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessRecord, PluginError>;

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

    /// Returns an observed terminal, or `None` once the boundary has armed
    /// `key`. An observed terminal lets the caller settle without opening a wait.
    ///
    /// The default refuses: a service that cannot observe process terminals
    /// must not silently accept responsibility for a wait it will never
    /// resolve, which would hang the parked call forever.
    async fn attach_process_terminal(
        &self,
        process_id: &ProcessId,
        key: &crate::AwaitEventKey,
        scope: ProcessOpScope<'_>,
    ) -> Result<Option<ProcessAwaitOutput>, PluginError> {
        let _ = (process_id, key, scope);
        Err(PluginError::Session(
            "arming a process terminal is unavailable in this service".to_string(),
        ))
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
    ) -> Result<(), PluginError> {
        let _ = (process_id, key);
        Err(PluginError::Session(
            "process consumer holds are unavailable in this service".to_string(),
        ))
    }

    /// Marks a call's consumer hold `key`, owned by `owner`, abandoned and
    /// returns the processes it holds that the call owes a cancel,
    /// controller-free: what an opener that cancelled the call drains (ADR
    /// 0116 §3.4). A registration under the key is refused from then on. The
    /// default holds nothing.
    async fn abandon_consumer_hold(
        &self,
        key: &str,
        owner: &crate::ScopeId,
    ) -> Result<Vec<ProcessId>, PluginError> {
        let _ = (key, owner);
        Ok(Vec::new())
    }

    async fn list_visible(
        &self,
        session_id: &SessionId,
        mode: ProcessListMode,
        scope: ProcessOpScope<'_>,
    ) -> Result<Vec<ProcessRecord>, PluginError>;

    /// A session may address the processes it observes; a process, the ones
    /// whose recorded ancestry names it as their immediate starter.
    async fn validate_visible(
        &self,
        owner: &crate::RuntimeOwner,
        process_ids: &[ProcessId],
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError>;

    async fn cancel(
        &self,
        owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessRecord, PluginError>;

    /// Journal-first cancellation used only by the recorded intent protocol.
    async fn cancel_recorded_intent(
        &self,
        owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        identity: crate::ToolIntentIdentity,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessRecord, PluginError>;

    async fn cancel_all_visible(
        &self,
        session_id: &SessionId,
        scope: ProcessOpScope<'_>,
    ) -> Result<Vec<ProcessCancelReceipt>, PluginError> {
        let entries = self
            .list_visible(session_id, ProcessListMode::Live, scope.clone())
            .await?;
        let owner = crate::RuntimeOwner::Session(session_id.clone());
        let mut cancelled = Vec::new();
        for record in entries {
            if record.is_terminal() {
                continue;
            }
            cancelled.push(
                self.cancel(&owner, &record.id, scope.clone())
                    .await
                    .and_then(ProcessCancelReceipt::from_record)?,
            );
        }
        Ok(cancelled)
    }

    /// Stage `signal`, a recorded tool intent's, as a store-local effect of
    /// the call that sends it (ADR 0132 §5): its append is admitted against
    /// the target as it stands, and only the commit that records the call's
    /// outcome appends and mails it.
    async fn stage_recorded_signal(
        &self,
        owner: &crate::RuntimeOwner,
        signal: &ProcessSignal,
        scope: ProcessOpScope<'_>,
    ) -> Result<StoreLocalEffect, PluginError>;

    async fn emit_event(
        &self,
        _session_id: &SessionId,
        _process_id: &ProcessId,
        _event_type: String,
        _replay_key: String,
        _payload: serde_json::Value,
        _scope: ProcessOpScope<'_>,
    ) -> Result<ProcessEvent, PluginError> {
        Err(PluginError::Session(
            "process event emission is unavailable in this runtime".to_string(),
        ))
    }

    /// Journal-first event emission used only by the recorded intent protocol.
    ///
    /// Called from shift code, so a replaying engine calls it again for an
    /// event the drain already landed, under the same `replay_key`; the effect
    /// controller answers the repeat with the recorded event. As for
    /// [`Self::stage_recorded_start`], anything an implementation does
    /// outside that boundary is keyed by the replay key.
    async fn emit_event_recorded_intent(
        &self,
        owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        event_type: String,
        replay_key: String,
        payload: serde_json::Value,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessEvent, PluginError>;

    async fn signal_possessed(
        &self,
        owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessEvent, PluginError>;

    async fn transfer(
        &self,
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: Vec<ProcessId>,
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError>;
}

pub struct UnavailableProcessService;

#[async_trait::async_trait]
impl ProcessService for UnavailableProcessService {
    async fn stage_recorded_start(
        &self,
        _owner: &crate::RuntimeOwner,
        _request: ProcessStartRequest,
        _scope: ProcessOpScope<'_>,
    ) -> Result<StagedProcessStart, PluginError> {
        Err(PluginError::Session(
            "processes are unavailable in this runtime".to_string(),
        ))
    }

    async fn start(
        &self,
        _session_id: &SessionId,
        _registration: ProcessStartRegistration,
        _options: ProcessStartOptions,
        _scope: ProcessOpScope<'_>,
    ) -> Result<ProcessRecord, PluginError> {
        Err(PluginError::Session(
            "processes are unavailable in this runtime".to_string(),
        ))
    }

    async fn await_process(
        &self,
        _process_id: &ProcessId,
        _scope: ProcessOpScope<'_>,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        Err(PluginError::Session(
            "process awaiting is unavailable in this runtime".to_string(),
        ))
    }

    async fn list_visible(
        &self,
        _session_id: &SessionId,
        _mode: ProcessListMode,
        _scope: ProcessOpScope<'_>,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        Err(PluginError::Session(
            "process registry is unavailable in this runtime".to_string(),
        ))
    }

    async fn validate_visible(
        &self,
        _owner: &crate::RuntimeOwner,
        _process_ids: &[ProcessId],
        _scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        Err(PluginError::Session(
            "process handle validation is unavailable in this runtime".to_string(),
        ))
    }

    async fn cancel(
        &self,
        _owner: &crate::RuntimeOwner,
        _process_id: &ProcessId,
        _scope: ProcessOpScope<'_>,
    ) -> Result<ProcessRecord, PluginError> {
        Err(PluginError::Session(
            "process registry is unavailable in this runtime".to_string(),
        ))
    }

    async fn cancel_recorded_intent(
        &self,
        _owner: &crate::RuntimeOwner,
        _process_id: &ProcessId,
        _identity: crate::ToolIntentIdentity,
        _scope: ProcessOpScope<'_>,
    ) -> Result<ProcessRecord, PluginError> {
        Err(PluginError::Session(
            "processes are unavailable in this runtime".to_string(),
        ))
    }

    async fn signal_possessed(
        &self,
        _owner: &crate::RuntimeOwner,
        _process_id: &ProcessId,
        _signal_name: String,
        _signal_id: String,
        _payload: serde_json::Value,
        _scope: ProcessOpScope<'_>,
    ) -> Result<ProcessEvent, PluginError> {
        Err(PluginError::Session(
            "process signalling is unavailable in this runtime".to_string(),
        ))
    }

    async fn stage_recorded_signal(
        &self,
        _owner: &crate::RuntimeOwner,
        _signal: &ProcessSignal,
        _scope: ProcessOpScope<'_>,
    ) -> Result<StoreLocalEffect, PluginError> {
        Err(PluginError::Session(
            "processes are unavailable in this runtime".to_string(),
        ))
    }

    async fn emit_event_recorded_intent(
        &self,
        _owner: &crate::RuntimeOwner,
        _process_id: &ProcessId,
        _event_type: String,
        _replay_key: String,
        _payload: serde_json::Value,
        _scope: ProcessOpScope<'_>,
    ) -> Result<ProcessEvent, PluginError> {
        Err(PluginError::Session(
            "processes are unavailable in this runtime".to_string(),
        ))
    }

    async fn transfer(
        &self,
        _from_session_id: &SessionId,
        _to_session_id: &SessionId,
        process_ids: Vec<ProcessId>,
        _scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        if process_ids.is_empty() {
            return Ok(());
        }
        Err(PluginError::Session(
            "process handle transfer is unavailable in this runtime".to_string(),
        ))
    }
}
