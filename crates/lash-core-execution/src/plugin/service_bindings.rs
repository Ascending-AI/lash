//! Optional callable host ports, shared by preflight and invocation.
use super::*;
use crate::ProcessId;
use crate::ProcessOpScope;
use crate::SessionId;
use crate::StagedProcessStart;
use crate::TurnId;
use lash_execution_contracts::process::ProcessAwaitOutput;
use lash_execution_contracts::process::ProcessHandleView;
use lash_execution_contracts::process::ProcessListMode;
use lash_execution_contracts::process::ProcessStartOptions;
use lash_execution_contracts::process::ProcessStartRegistration;
use lash_execution_contracts::process::ProcessStartRequest;
use lash_execution_contracts::session_types::SessionCreateRequest;
use lash_execution_contracts::session_types::SessionHandle;
use std::sync::Arc;

#[async_trait::async_trait]
pub trait TurnScopePort: Send + Sync {
    async fn turn_scope(
        &self,
        _session_id: &SessionId,
        _turn_id: &TurnId,
    ) -> Result<crate::ExecutionScope, PluginError>;
}

#[async_trait::async_trait]
pub trait CommittedTurnReadPort: Send + Sync {
    async fn committed_turn(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<Option<lash_core_store::transcript::CommittedTurnWithCalls>, CommittedTurnReadError>;
}

#[async_trait::async_trait]
pub trait CurrentSessionSnapshotPort: Send + Sync {
    async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError>;
}

#[async_trait::async_trait]
pub trait NamedSessionSnapshotPort: Send + Sync {
    async fn snapshot_session(
        &self,
        _session_id: &SessionId,
    ) -> Result<SessionSnapshot, PluginError>;
}

#[async_trait::async_trait]
pub trait RecordedToolCatalogPort: Send + Sync {
    async fn tool_catalog(
        &self,
        _session_id: &SessionId,
    ) -> Result<Vec<serde_json::Value>, PluginError>;
}

#[async_trait::async_trait]
pub trait SessionCreatePort: Send + Sync {
    async fn create_session(
        &self,
        _request: SessionCreateRequest,
    ) -> Result<SessionHandle, PluginError>;
}

#[async_trait::async_trait]
pub trait SessionGraphAppendPort: Send + Sync {
    async fn append_session_nodes(
        &self,
        _session_id: &SessionId,
        _request: AppendSessionNodesRequest,
    ) -> Result<AppendSessionNodesOutcome, PluginError>;
}

#[async_trait::async_trait]
pub trait ProcessListPort: Send + Sync {
    async fn list_visible(
        &self,
        _session_id: &SessionId,
        _mode: lash_execution_contracts::process::ProcessListMode,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<lash_execution_contracts::process::ObservedProcess>, PluginError>;
}

#[async_trait::async_trait]
pub trait ProcessListForAttemptPort: Send + Sync {
    async fn list_visible_for_attempt(
        &self,
        owner: &crate::RuntimeOwner,
        mode: ProcessListMode,
    ) -> Result<Vec<lash_execution_contracts::process::ObservedProcess>, PluginError>;
}

#[async_trait::async_trait]
pub trait ProcessStartFromRequestPort: Send + Sync {
    async fn start_from_request(
        &self,
        session_id: &SessionId,
        request: ProcessStartRequest,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessHandleView, PluginError>;
}

#[async_trait::async_trait]
pub trait ProcessRecordedStartPort: Send + Sync {
    async fn stage_recorded_start(
        &self,
        owner: &crate::RuntimeOwner,
        request: ProcessStartRequest,
        scope: ProcessOpScope<'_>,
    ) -> Result<StagedProcessStart, PluginError>;
}

#[async_trait::async_trait]
pub trait ProcessBoundStartPort: Send + Sync {
    async fn stage_bound(
        &self,
        registration: ProcessStartRegistration,
        scope: ProcessOpScope<'_>,
    ) -> Result<StagedProcessStart, PluginError>;
}

#[async_trait::async_trait]
pub trait ProcessBoundCancelPort: Send + Sync {
    async fn cancel_bound(
        &self,
        process_id: &ProcessId,
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError>;
}

#[async_trait::async_trait]
pub trait ProcessStartPort: Send + Sync {
    async fn start(
        &self,
        session_id: &SessionId,
        registration: ProcessStartRegistration,
        options: ProcessStartOptions,
        scope: ProcessOpScope<'_>,
    ) -> Result<lash_execution_contracts::process::ObservedProcess, PluginError>;
}

#[async_trait::async_trait]
pub trait ProcessAwaitPort: Send + Sync {
    async fn await_process(
        &self,
        process_id: &ProcessId,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessAwaitOutput, PluginError>;
}

#[async_trait::async_trait]
pub trait ProcessReleaseHoldPort: Send + Sync {
    async fn release_consumer_hold(
        &self,
        process_id: &ProcessId,
        key: &str,
    ) -> Result<(), PluginError>;
}

#[async_trait::async_trait]
pub trait ProcessAbandonHoldPort: Send + Sync {
    async fn abandon_consumer_hold(
        &self,
        key: &str,
        owner: &crate::ScopeId,
    ) -> Result<Vec<ProcessId>, PluginError>;
}

#[async_trait::async_trait]
pub trait ProcessValidateVisiblePort: Send + Sync {
    async fn validate_visible(
        &self,
        owner: &crate::RuntimeOwner,
        process_ids: &[ProcessId],
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError>;
}

#[async_trait::async_trait]
pub trait ProcessCancelPort: Send + Sync {
    async fn cancel(
        &self,
        owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        scope: ProcessOpScope<'_>,
    ) -> Result<lash_execution_contracts::process::ObservedProcess, PluginError>;
}

#[async_trait::async_trait]
pub trait ProcessRecordedCancelPort: Send + Sync {
    async fn cancel_recorded_intent(
        &self,
        owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        identity: crate::ToolIntentIdentity,
        scope: ProcessOpScope<'_>,
    ) -> Result<lash_execution_contracts::process::ObservedProcess, PluginError>;
}

#[async_trait::async_trait]
pub trait ProcessTransferPort: Send + Sync {
    async fn transfer(
        &self,
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: Vec<ProcessId>,
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError>;
}

#[derive(Clone, Default)]
pub struct ServiceBindings {
    pub turn_scope: Option<Arc<dyn TurnScopePort>>,
    pub snapshot_current: Option<Arc<dyn CurrentSessionSnapshotPort>>,
    pub committed_turn: Option<Arc<dyn CommittedTurnReadPort>>,
    pub snapshot_session: Option<Arc<dyn NamedSessionSnapshotPort>>,
    pub tool_catalog: Option<Arc<dyn RecordedToolCatalogPort>>,
    pub create_session: Option<Arc<dyn SessionCreatePort>>,
    pub append_session_nodes: Option<Arc<dyn SessionGraphAppendPort>>,
    pub list_visible: Option<Arc<dyn ProcessListPort>>,
    pub list_visible_for_attempt: Option<Arc<dyn ProcessListForAttemptPort>>,
    pub start_from_request: Option<Arc<dyn ProcessStartFromRequestPort>>,
    pub stage_recorded_start: Option<Arc<dyn ProcessRecordedStartPort>>,
    pub stage_bound: Option<Arc<dyn ProcessBoundStartPort>>,
    pub cancel_bound: Option<Arc<dyn ProcessBoundCancelPort>>,
    pub start: Option<Arc<dyn ProcessStartPort>>,
    pub await_process: Option<Arc<dyn ProcessAwaitPort>>,
    pub release_consumer_hold: Option<Arc<dyn ProcessReleaseHoldPort>>,
    pub abandon_consumer_hold: Option<Arc<dyn ProcessAbandonHoldPort>>,
    pub validate_visible: Option<Arc<dyn ProcessValidateVisiblePort>>,
    pub cancel: Option<Arc<dyn ProcessCancelPort>>,
    pub cancel_recorded_intent: Option<Arc<dyn ProcessRecordedCancelPort>>,
    pub transfer: Option<Arc<dyn ProcessTransferPort>>,
    graph_trace: Option<Arc<dyn SessionGraphService>>,
}
impl std::fmt::Debug for ServiceBindings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.available()).finish()
    }
}
impl ServiceBindings {
    pub async fn committed_turn(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<Option<lash_core_store::transcript::CommittedTurnWithCalls>, CommittedTurnReadError>
    {
        let service = self.committed_turn.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::CommittedTurnRead,
            })
        })?;
        service.committed_turn(session_id, turn_id).await
    }

    pub async fn turn_scope(
        &self,
        _session_id: &SessionId,
        _turn_id: &TurnId,
    ) -> Result<crate::ExecutionScope, PluginError> {
        let service = self.turn_scope.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::TurnScope,
            })
        })?;
        service.turn_scope(_session_id, _turn_id).await
    }
    pub async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
        let service = self.snapshot_current.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::CurrentSessionSnapshot,
            })
        })?;
        service.snapshot_current().await
    }
    pub async fn snapshot_session(
        &self,
        _session_id: &SessionId,
    ) -> Result<SessionSnapshot, PluginError> {
        let service = self.snapshot_session.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::NamedSessionSnapshot,
            })
        })?;
        service.snapshot_session(_session_id).await
    }
    pub async fn tool_catalog(
        &self,
        _session_id: &SessionId,
    ) -> Result<Vec<serde_json::Value>, PluginError> {
        let service = self.tool_catalog.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::RecordedToolCatalog,
            })
        })?;
        service.tool_catalog(_session_id).await
    }
    pub async fn create_session(
        &self,
        _request: SessionCreateRequest,
    ) -> Result<SessionHandle, PluginError> {
        let service = self.create_session.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::SessionCreate,
            })
        })?;
        service.create_session(_request).await
    }
    pub async fn append_session_nodes(
        &self,
        _session_id: &SessionId,
        _request: AppendSessionNodesRequest,
    ) -> Result<AppendSessionNodesOutcome, PluginError> {
        let service = self.append_session_nodes.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::SessionGraphAppend,
            })
        })?;
        service.append_session_nodes(_session_id, _request).await
    }
    pub async fn list_visible(
        &self,
        _session_id: &SessionId,
        _mode: lash_execution_contracts::process::ProcessListMode,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<lash_execution_contracts::process::ObservedProcess>, PluginError> {
        let service = self.list_visible.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessList,
            })
        })?;
        service.list_visible(_session_id, _mode, _scope).await
    }
    pub async fn list_visible_for_attempt(
        &self,
        owner: &crate::RuntimeOwner,
        mode: ProcessListMode,
    ) -> Result<Vec<lash_execution_contracts::process::ObservedProcess>, PluginError> {
        let service = self.list_visible_for_attempt.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessListForAttempt,
            })
        })?;
        service.list_visible_for_attempt(owner, mode).await
    }
    pub async fn start_from_request(
        &self,
        session_id: &SessionId,
        request: ProcessStartRequest,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessHandleView, PluginError> {
        let service = self.start_from_request.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessStartFromRequest,
            })
        })?;
        service.start_from_request(session_id, request, scope).await
    }
    pub async fn stage_recorded_start(
        &self,
        owner: &crate::RuntimeOwner,
        request: ProcessStartRequest,
        scope: ProcessOpScope<'_>,
    ) -> Result<StagedProcessStart, PluginError> {
        let service = self.stage_recorded_start.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessRecordedStart,
            })
        })?;
        service.stage_recorded_start(owner, request, scope).await
    }
    pub async fn stage_bound(
        &self,
        registration: ProcessStartRegistration,
        scope: ProcessOpScope<'_>,
    ) -> Result<StagedProcessStart, PluginError> {
        let service = self.stage_bound.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessBoundStart,
            })
        })?;
        service.stage_bound(registration, scope).await
    }
    pub async fn cancel_bound(
        &self,
        process_id: &ProcessId,
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        let service = self.cancel_bound.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessBoundCancel,
            })
        })?;
        service.cancel_bound(process_id, scope).await
    }
    pub async fn start(
        &self,
        session_id: &SessionId,
        registration: ProcessStartRegistration,
        options: ProcessStartOptions,
        scope: ProcessOpScope<'_>,
    ) -> Result<lash_execution_contracts::process::ObservedProcess, PluginError> {
        let service = self.start.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessStart,
            })
        })?;
        service
            .start(session_id, registration, options, scope)
            .await
    }
    pub async fn await_process(
        &self,
        process_id: &ProcessId,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        let service = self.await_process.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessAwait,
            })
        })?;
        service.await_process(process_id, scope).await
    }
    pub async fn release_consumer_hold(
        &self,
        process_id: &ProcessId,
        key: &str,
    ) -> Result<(), PluginError> {
        let service = self.release_consumer_hold.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessReleaseHold,
            })
        })?;
        service.release_consumer_hold(process_id, key).await
    }
    pub async fn abandon_consumer_hold(
        &self,
        key: &str,
        owner: &crate::ScopeId,
    ) -> Result<Vec<ProcessId>, PluginError> {
        let service = self.abandon_consumer_hold.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessAbandonHold,
            })
        })?;
        service.abandon_consumer_hold(key, owner).await
    }
    pub async fn validate_visible(
        &self,
        owner: &crate::RuntimeOwner,
        process_ids: &[ProcessId],
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        let service = self.validate_visible.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessValidateVisible,
            })
        })?;
        service.validate_visible(owner, process_ids, scope).await
    }
    pub async fn cancel(
        &self,
        owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        scope: ProcessOpScope<'_>,
    ) -> Result<lash_execution_contracts::process::ObservedProcess, PluginError> {
        let service = self.cancel.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessCancel,
            })
        })?;
        service.cancel(owner, process_id, scope).await
    }
    pub async fn cancel_recorded_intent(
        &self,
        owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        identity: crate::ToolIntentIdentity,
        scope: ProcessOpScope<'_>,
    ) -> Result<lash_execution_contracts::process::ObservedProcess, PluginError> {
        let service = self.cancel_recorded_intent.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessRecordedCancel,
            })
        })?;
        service
            .cancel_recorded_intent(owner, process_id, identity, scope)
            .await
    }
    pub async fn transfer(
        &self,
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: Vec<ProcessId>,
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        let service = self.transfer.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessTransfer,
            })
        })?;
        service
            .transfer(from_session_id, to_session_id, process_ids, scope)
            .await
    }

    /// The callable adapters installed by RuntimeSessionServices for session owners.
    /// Process adapters are installed only when the runtime has its process registry;
    /// actual invocation bindings are checked again before owner construction.
    pub(crate) fn session_runtime_services(
        processes: bool,
    ) -> std::collections::BTreeSet<ServiceKind> {
        use ServiceKind::*;
        let mut services = std::collections::BTreeSet::from([
            TurnScope,
            CurrentSessionSnapshot,
            NamedSessionSnapshot,
            RecordedToolCatalog,
            CommittedTurnRead,
            SessionCreate,
            SessionGraphAppend,
        ]);
        if processes {
            services.extend([
                ProcessList,
                ProcessListForAttempt,
                ProcessStartFromRequest,
                ProcessRecordedStart,
                ProcessBoundStart,
                ProcessBoundCancel,
                ProcessStart,
                ProcessAwait,
                ProcessReleaseHold,
                ProcessAbandonHold,
                ProcessValidateVisible,
                ProcessCancel,
                ProcessRecordedCancel,
                ProcessTransfer,
            ]);
        }
        services
    }
    pub fn available(&self) -> std::collections::BTreeSet<ServiceKind> {
        let mut services = std::collections::BTreeSet::new();
        if self.turn_scope.is_some() {
            services.insert(ServiceKind::TurnScope);
        }
        if self.snapshot_current.is_some() {
            services.insert(ServiceKind::CurrentSessionSnapshot);
        }
        if self.snapshot_session.is_some() {
            services.insert(ServiceKind::NamedSessionSnapshot);
        }
        if self.committed_turn.is_some() {
            services.insert(ServiceKind::CommittedTurnRead);
        }
        if self.tool_catalog.is_some() {
            services.insert(ServiceKind::RecordedToolCatalog);
        }
        if self.create_session.is_some() {
            services.insert(ServiceKind::SessionCreate);
        }
        if self.append_session_nodes.is_some() {
            services.insert(ServiceKind::SessionGraphAppend);
        }
        if self.list_visible.is_some() {
            services.insert(ServiceKind::ProcessList);
        }
        if self.list_visible_for_attempt.is_some() {
            services.insert(ServiceKind::ProcessListForAttempt);
        }
        if self.start_from_request.is_some() {
            services.insert(ServiceKind::ProcessStartFromRequest);
        }
        if self.stage_recorded_start.is_some() {
            services.insert(ServiceKind::ProcessRecordedStart);
        }
        if self.stage_bound.is_some() {
            services.insert(ServiceKind::ProcessBoundStart);
        }
        if self.cancel_bound.is_some() {
            services.insert(ServiceKind::ProcessBoundCancel);
        }
        if self.start.is_some() {
            services.insert(ServiceKind::ProcessStart);
        }
        if self.await_process.is_some() {
            services.insert(ServiceKind::ProcessAwait);
        }
        if self.release_consumer_hold.is_some() {
            services.insert(ServiceKind::ProcessReleaseHold);
        }
        if self.abandon_consumer_hold.is_some() {
            services.insert(ServiceKind::ProcessAbandonHold);
        }
        if self.validate_visible.is_some() {
            services.insert(ServiceKind::ProcessValidateVisible);
        }
        if self.cancel.is_some() {
            services.insert(ServiceKind::ProcessCancel);
        }
        if self.cancel_recorded_intent.is_some() {
            services.insert(ServiceKind::ProcessRecordedCancel);
        }
        if self.transfer.is_some() {
            services.insert(ServiceKind::ProcessTransfer);
        }
        services
    }
    pub fn preflight(
        &self,
        owner: &str,
        requirements: &Requirements,
    ) -> Result<(), PreflightIssue> {
        let available = self.available();
        if let Some(service) = requirements.services.difference(&available).next() {
            return Err(PreflightIssue::MissingService {
                owner: owner.into(),
                service: *service,
            });
        }
        Ok(())
    }
    pub fn with_session_state(mut self, service: Arc<dyn SessionStateService>) -> Self {
        self = self.with_session_reads(service.clone());
        self.turn_scope = Some(Arc::new(SessionStateServiceBindings(service)));
        self
    }
    pub fn with_session_reads(mut self, service: Arc<dyn SessionReadService>) -> Self {
        let service = Arc::new(SessionReadServiceBindings(service));
        self.committed_turn = Some(service.clone());
        self.snapshot_current = Some(service.clone());
        self.snapshot_session = Some(service.clone());
        self.tool_catalog = Some(service.clone());
        self
    }
    pub fn with_session_lifecycle(mut self, service: Arc<dyn SessionLifecycleService>) -> Self {
        let service = Arc::new(SessionLifecycleServiceBindings(service));
        self.create_session = Some(service.clone());
        self
    }
    pub fn with_session_graph(mut self, service: Arc<dyn SessionGraphService>) -> Self {
        let service = Arc::new(SessionGraphServiceBindings(service));
        self.append_session_nodes = Some(service.clone());
        self.graph_trace = Some(Arc::clone(&service.0));
        self
    }
    pub fn with_process_reads(mut self, service: Arc<dyn ProcessReadService>) -> Self {
        let service = Arc::new(ProcessReadServiceBindings(service));
        self.list_visible = Some(service.clone());
        self
    }
    pub fn with_processes(mut self, service: Arc<dyn crate::ProcessService>) -> Self {
        let service = Arc::new(ProcessServiceBindings(service));
        self.list_visible_for_attempt = Some(service.clone());
        self.start_from_request = Some(service.clone());
        self.stage_recorded_start = Some(service.clone());
        self.stage_bound = Some(service.clone());
        self.cancel_bound = Some(service.clone());
        self.start = Some(service.clone());
        self.await_process = Some(service.clone());
        self.release_consumer_hold = Some(service.clone());
        self.abandon_consumer_hold = Some(service.clone());
        self.list_visible = Some(service.clone());
        self.validate_visible = Some(service.clone());
        self.cancel = Some(service.clone());
        self.cancel_recorded_intent = Some(service.clone());
        self.transfer = Some(service.clone());
        self
    }
}
struct SessionStateServiceBindings(Arc<dyn SessionStateService>);
#[async_trait::async_trait]
impl TurnScopePort for SessionStateServiceBindings {
    async fn turn_scope(
        &self,
        _session_id: &SessionId,
        _turn_id: &TurnId,
    ) -> Result<crate::ExecutionScope, PluginError> {
        self.0.turn_scope(_session_id, _turn_id).await
    }
}
impl crate::core_internal::RuntimeService for ServiceBindings {}

#[async_trait::async_trait]
impl SessionStateService for ServiceBindings {
    async fn turn_scope(
        &self,
        _session_id: &SessionId,
        _turn_id: &TurnId,
    ) -> Result<crate::ExecutionScope, PluginError> {
        let service = self.turn_scope.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::TurnScope,
            })
        })?;
        service.turn_scope(_session_id, _turn_id).await
    }
}
struct SessionReadServiceBindings(Arc<dyn SessionReadService>);
#[async_trait::async_trait]
impl CommittedTurnReadPort for SessionReadServiceBindings {
    async fn committed_turn(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<Option<lash_core_store::transcript::CommittedTurnWithCalls>, CommittedTurnReadError>
    {
        self.0.committed_turn(session_id, turn_id).await
    }
}
#[async_trait::async_trait]
impl CurrentSessionSnapshotPort for SessionReadServiceBindings {
    async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
        self.0.snapshot_current().await
    }
}
#[async_trait::async_trait]
impl NamedSessionSnapshotPort for SessionReadServiceBindings {
    async fn snapshot_session(
        &self,
        _session_id: &SessionId,
    ) -> Result<SessionSnapshot, PluginError> {
        self.0.snapshot_session(_session_id).await
    }
}
#[async_trait::async_trait]
impl RecordedToolCatalogPort for SessionReadServiceBindings {
    async fn tool_catalog(
        &self,
        _session_id: &SessionId,
    ) -> Result<Vec<serde_json::Value>, PluginError> {
        self.0.tool_catalog(_session_id).await
    }
}
#[async_trait::async_trait]
impl SessionReadService for ServiceBindings {
    async fn committed_turn(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<Option<lash_core_store::transcript::CommittedTurnWithCalls>, CommittedTurnReadError>
    {
        let service = self.committed_turn.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::CommittedTurnRead,
            })
        })?;
        service.committed_turn(session_id, turn_id).await
    }

    async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
        let service = self.snapshot_current.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::CurrentSessionSnapshot,
            })
        })?;
        service.snapshot_current().await
    }
    async fn snapshot_session(
        &self,
        _session_id: &SessionId,
    ) -> Result<SessionSnapshot, PluginError> {
        let service = self.snapshot_session.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::NamedSessionSnapshot,
            })
        })?;
        service.snapshot_session(_session_id).await
    }
    async fn tool_catalog(
        &self,
        _session_id: &SessionId,
    ) -> Result<Vec<serde_json::Value>, PluginError> {
        let service = self.tool_catalog.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::RecordedToolCatalog,
            })
        })?;
        service.tool_catalog(_session_id).await
    }
}
struct SessionLifecycleServiceBindings(Arc<dyn SessionLifecycleService>);
#[async_trait::async_trait]
impl SessionCreatePort for SessionLifecycleServiceBindings {
    async fn create_session(
        &self,
        _request: SessionCreateRequest,
    ) -> Result<SessionHandle, PluginError> {
        self.0.create_session(_request).await
    }
}
#[async_trait::async_trait]
impl SessionLifecycleService for ServiceBindings {
    async fn create_session(
        &self,
        _request: SessionCreateRequest,
    ) -> Result<SessionHandle, PluginError> {
        let service = self.create_session.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::SessionCreate,
            })
        })?;
        service.create_session(_request).await
    }
}
struct SessionGraphServiceBindings(Arc<dyn SessionGraphService>);
#[async_trait::async_trait]
impl SessionGraphAppendPort for SessionGraphServiceBindings {
    async fn append_session_nodes(
        &self,
        _session_id: &SessionId,
        _request: AppendSessionNodesRequest,
    ) -> Result<AppendSessionNodesOutcome, PluginError> {
        self.0.append_session_nodes(_session_id, _request).await
    }
}
#[async_trait::async_trait]
impl SessionGraphService for ServiceBindings {
    async fn append_session_nodes(
        &self,
        _session_id: &SessionId,
        _request: AppendSessionNodesRequest,
    ) -> Result<AppendSessionNodesOutcome, PluginError> {
        let service = self.append_session_nodes.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::SessionGraphAppend,
            })
        })?;
        service.append_session_nodes(_session_id, _request).await
    }
    async fn emit_trace_event(
        &self,
        context: lash_trace::TraceContext,
        event: lash_trace::TraceEvent,
    ) -> Result<(), PluginError> {
        match &self.graph_trace {
            Some(service) => service.emit_trace_event(context, event).await,
            None => Ok(()),
        }
    }
}
struct ProcessReadServiceBindings(Arc<dyn ProcessReadService>);
#[async_trait::async_trait]
impl ProcessListPort for ProcessReadServiceBindings {
    async fn list_visible(
        &self,
        _session_id: &SessionId,
        _mode: lash_execution_contracts::process::ProcessListMode,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<lash_execution_contracts::process::ObservedProcess>, PluginError> {
        self.0.list_visible(_session_id, _mode, _scope).await
    }
}
#[async_trait::async_trait]
impl ProcessReadService for ServiceBindings {
    async fn list_visible(
        &self,
        _session_id: &SessionId,
        _mode: lash_execution_contracts::process::ProcessListMode,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<lash_execution_contracts::process::ObservedProcess>, PluginError> {
        let service = self.list_visible.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessList,
            })
        })?;
        service.list_visible(_session_id, _mode, _scope).await
    }
}
struct ProcessServiceBindings(Arc<dyn crate::ProcessService>);
#[async_trait::async_trait]
impl ProcessListForAttemptPort for ProcessServiceBindings {
    async fn list_visible_for_attempt(
        &self,
        owner: &crate::RuntimeOwner,
        mode: ProcessListMode,
    ) -> Result<Vec<lash_execution_contracts::process::ObservedProcess>, PluginError> {
        self.0.list_visible_for_attempt(owner, mode).await
    }
}
#[async_trait::async_trait]
impl ProcessStartFromRequestPort for ProcessServiceBindings {
    async fn start_from_request(
        &self,
        session_id: &SessionId,
        request: ProcessStartRequest,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessHandleView, PluginError> {
        self.0.start_from_request(session_id, request, scope).await
    }
}
#[async_trait::async_trait]
impl ProcessRecordedStartPort for ProcessServiceBindings {
    async fn stage_recorded_start(
        &self,
        owner: &crate::RuntimeOwner,
        request: ProcessStartRequest,
        scope: ProcessOpScope<'_>,
    ) -> Result<StagedProcessStart, PluginError> {
        self.0.stage_recorded_start(owner, request, scope).await
    }
}
#[async_trait::async_trait]
impl ProcessBoundStartPort for ProcessServiceBindings {
    async fn stage_bound(
        &self,
        registration: ProcessStartRegistration,
        scope: ProcessOpScope<'_>,
    ) -> Result<StagedProcessStart, PluginError> {
        self.0.stage_bound(registration, scope).await
    }
}
#[async_trait::async_trait]
impl ProcessBoundCancelPort for ProcessServiceBindings {
    async fn cancel_bound(
        &self,
        process_id: &ProcessId,
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        self.0.cancel_bound(process_id, scope).await
    }
}
#[async_trait::async_trait]
impl ProcessStartPort for ProcessServiceBindings {
    async fn start(
        &self,
        session_id: &SessionId,
        registration: ProcessStartRegistration,
        options: ProcessStartOptions,
        scope: ProcessOpScope<'_>,
    ) -> Result<lash_execution_contracts::process::ObservedProcess, PluginError> {
        self.0.start(session_id, registration, options, scope).await
    }
}
#[async_trait::async_trait]
impl ProcessAwaitPort for ProcessServiceBindings {
    async fn await_process(
        &self,
        process_id: &ProcessId,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        self.0.await_process(process_id, scope).await
    }
}
#[async_trait::async_trait]
impl ProcessReleaseHoldPort for ProcessServiceBindings {
    async fn release_consumer_hold(
        &self,
        process_id: &ProcessId,
        key: &str,
    ) -> Result<(), PluginError> {
        self.0.release_consumer_hold(process_id, key).await
    }
}
#[async_trait::async_trait]
impl ProcessAbandonHoldPort for ProcessServiceBindings {
    async fn abandon_consumer_hold(
        &self,
        key: &str,
        owner: &crate::ScopeId,
    ) -> Result<Vec<ProcessId>, PluginError> {
        self.0.abandon_consumer_hold(key, owner).await
    }
}
#[async_trait::async_trait]
impl ProcessListPort for ProcessServiceBindings {
    async fn list_visible(
        &self,
        session_id: &SessionId,
        mode: ProcessListMode,
        scope: ProcessOpScope<'_>,
    ) -> Result<Vec<lash_execution_contracts::process::ObservedProcess>, PluginError> {
        self.0.list_visible(session_id, mode, scope).await
    }
}
#[async_trait::async_trait]
impl ProcessValidateVisiblePort for ProcessServiceBindings {
    async fn validate_visible(
        &self,
        owner: &crate::RuntimeOwner,
        process_ids: &[ProcessId],
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        self.0.validate_visible(owner, process_ids, scope).await
    }
}
#[async_trait::async_trait]
impl ProcessCancelPort for ProcessServiceBindings {
    async fn cancel(
        &self,
        owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        scope: ProcessOpScope<'_>,
    ) -> Result<lash_execution_contracts::process::ObservedProcess, PluginError> {
        self.0.cancel(owner, process_id, scope).await
    }
}
#[async_trait::async_trait]
impl ProcessRecordedCancelPort for ProcessServiceBindings {
    async fn cancel_recorded_intent(
        &self,
        owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        identity: crate::ToolIntentIdentity,
        scope: ProcessOpScope<'_>,
    ) -> Result<lash_execution_contracts::process::ObservedProcess, PluginError> {
        self.0
            .cancel_recorded_intent(owner, process_id, identity, scope)
            .await
    }
}
#[async_trait::async_trait]
impl ProcessTransferPort for ProcessServiceBindings {
    async fn transfer(
        &self,
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: Vec<ProcessId>,
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        self.0
            .transfer(from_session_id, to_session_id, process_ids, scope)
            .await
    }
}
#[async_trait::async_trait]
impl crate::ProcessService for ServiceBindings {
    async fn list_visible_for_attempt(
        &self,
        owner: &crate::RuntimeOwner,
        mode: ProcessListMode,
    ) -> Result<Vec<lash_execution_contracts::process::ObservedProcess>, PluginError> {
        let service = self.list_visible_for_attempt.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessListForAttempt,
            })
        })?;
        service.list_visible_for_attempt(owner, mode).await
    }
    async fn start_from_request(
        &self,
        session_id: &SessionId,
        request: ProcessStartRequest,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessHandleView, PluginError> {
        let service = self.start_from_request.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessStartFromRequest,
            })
        })?;
        service.start_from_request(session_id, request, scope).await
    }
    async fn stage_recorded_start(
        &self,
        owner: &crate::RuntimeOwner,
        request: ProcessStartRequest,
        scope: ProcessOpScope<'_>,
    ) -> Result<StagedProcessStart, PluginError> {
        let service = self.stage_recorded_start.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessRecordedStart,
            })
        })?;
        service.stage_recorded_start(owner, request, scope).await
    }
    async fn stage_bound(
        &self,
        registration: ProcessStartRegistration,
        scope: ProcessOpScope<'_>,
    ) -> Result<StagedProcessStart, PluginError> {
        let service = self.stage_bound.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessBoundStart,
            })
        })?;
        service.stage_bound(registration, scope).await
    }
    async fn cancel_bound(
        &self,
        process_id: &ProcessId,
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        let service = self.cancel_bound.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessBoundCancel,
            })
        })?;
        service.cancel_bound(process_id, scope).await
    }
    async fn start(
        &self,
        session_id: &SessionId,
        registration: ProcessStartRegistration,
        options: ProcessStartOptions,
        scope: ProcessOpScope<'_>,
    ) -> Result<lash_execution_contracts::process::ObservedProcess, PluginError> {
        let service = self.start.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessStart,
            })
        })?;
        service
            .start(session_id, registration, options, scope)
            .await
    }
    async fn await_process(
        &self,
        process_id: &ProcessId,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        let service = self.await_process.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessAwait,
            })
        })?;
        service.await_process(process_id, scope).await
    }
    async fn release_consumer_hold(
        &self,
        process_id: &ProcessId,
        key: &str,
    ) -> Result<(), PluginError> {
        let service = self.release_consumer_hold.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessReleaseHold,
            })
        })?;
        service.release_consumer_hold(process_id, key).await
    }
    async fn abandon_consumer_hold(
        &self,
        key: &str,
        owner: &crate::ScopeId,
    ) -> Result<Vec<ProcessId>, PluginError> {
        let service = self.abandon_consumer_hold.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessAbandonHold,
            })
        })?;
        service.abandon_consumer_hold(key, owner).await
    }
    async fn list_visible(
        &self,
        session_id: &SessionId,
        mode: ProcessListMode,
        scope: ProcessOpScope<'_>,
    ) -> Result<Vec<lash_execution_contracts::process::ObservedProcess>, PluginError> {
        let service = self.list_visible.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessList,
            })
        })?;
        service.list_visible(session_id, mode, scope).await
    }
    async fn validate_visible(
        &self,
        owner: &crate::RuntimeOwner,
        process_ids: &[ProcessId],
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        let service = self.validate_visible.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessValidateVisible,
            })
        })?;
        service.validate_visible(owner, process_ids, scope).await
    }
    async fn cancel(
        &self,
        owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        scope: ProcessOpScope<'_>,
    ) -> Result<lash_execution_contracts::process::ObservedProcess, PluginError> {
        let service = self.cancel.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessCancel,
            })
        })?;
        service.cancel(owner, process_id, scope).await
    }
    async fn cancel_recorded_intent(
        &self,
        owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        identity: crate::ToolIntentIdentity,
        scope: ProcessOpScope<'_>,
    ) -> Result<lash_execution_contracts::process::ObservedProcess, PluginError> {
        let service = self.cancel_recorded_intent.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessRecordedCancel,
            })
        })?;
        service
            .cancel_recorded_intent(owner, process_id, identity, scope)
            .await
    }
    async fn transfer(
        &self,
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: Vec<ProcessId>,
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        let service = self.transfer.as_ref().ok_or_else(|| {
            PluginError::Preflight(PreflightIssue::MissingService {
                owner: "invocation".into(),
                service: ServiceKind::ProcessTransfer,
            })
        })?;
        service
            .transfer(from_session_id, to_session_id, process_ids, scope)
            .await
    }
}
