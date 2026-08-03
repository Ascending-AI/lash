use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{RuntimeError, RuntimeErrorCode};

use super::super::envelope::{RuntimeEffectEnvelope, RuntimeEffectOutcome};
use super::{RuntimeEffectControllerError, RuntimeEffectLocalExecutor};

// =============================================================================
// Effect host + controller trait + scope + error
// =============================================================================

/// Stable semantic identity for one effectful runtime operation.
///
/// The scope is chosen by the host boundary before any nondeterministic work is
/// planned. It is intentionally generic: Restate, an inline test host, or a
/// future durable effect host all receive the same Lash scope vocabulary.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionScope {
    Turn {
        session_id: String,
        turn_id: String,
    },
    Process {
        process_id: String,
    },
    QueueDrain {
        session_id: String,
        drain_id: String,
    },
    SessionDelete {
        session_id: String,
    },
    RuntimeOperation {
        operation_id: String,
    },
}

impl ExecutionScope {
    pub fn turn(session_id: impl Into<String>, turn_id: impl Into<String>) -> Self {
        Self::Turn {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
        }
    }

    pub fn process(process_id: impl Into<String>) -> Self {
        Self::Process {
            process_id: process_id.into(),
        }
    }

    pub fn queue_drain(session_id: impl Into<String>, drain_id: impl Into<String>) -> Self {
        Self::QueueDrain {
            session_id: session_id.into(),
            drain_id: drain_id.into(),
        }
    }

    pub fn session_delete(session_id: impl Into<String>) -> Self {
        Self::SessionDelete {
            session_id: session_id.into(),
        }
    }

    pub fn runtime_operation(operation_id: impl Into<String>) -> Self {
        Self::RuntimeOperation {
            operation_id: operation_id.into(),
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Turn { turn_id, .. } => turn_id,
            Self::Process { process_id } => process_id,
            Self::QueueDrain { drain_id, .. } => drain_id,
            Self::SessionDelete { session_id, .. } => session_id,
            Self::RuntimeOperation { operation_id } => operation_id,
        }
    }

    /// Canonical typed identity persisted by durable effect journals.
    pub fn journal_identity(&self) -> Result<EffectJournalIdentity, RuntimeError> {
        self.validate()?;
        Ok(EffectJournalIdentity::from_scope(self))
    }

    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Turn { session_id, .. }
            | Self::QueueDrain { session_id, .. }
            | Self::SessionDelete { session_id, .. } => Some(session_id),
            Self::Process { .. } | Self::RuntimeOperation { .. } => None,
        }
    }

    pub fn turn_id(&self) -> Option<&str> {
        match self {
            Self::Turn { turn_id, .. } => Some(turn_id),
            _ => None,
        }
    }

    pub fn validates_turn_trace_id(&self) -> bool {
        matches!(self, Self::Turn { .. })
    }

    pub fn validate(&self) -> Result<(), RuntimeError> {
        let missing = match self {
            Self::Turn {
                session_id,
                turn_id,
                ..
            } => session_id.trim().is_empty() || turn_id.trim().is_empty(),
            Self::Process { process_id } => process_id.trim().is_empty(),
            Self::QueueDrain {
                session_id,
                drain_id,
                ..
            } => session_id.trim().is_empty() || drain_id.trim().is_empty(),
            Self::SessionDelete { session_id, .. } => session_id.trim().is_empty(),
            Self::RuntimeOperation { operation_id } => operation_id.trim().is_empty(),
        };
        if missing {
            return Err(RuntimeError::new(
                RuntimeErrorCode::MissingExecutionScopeId,
                "execution scopes require non-empty stable ids",
            ));
        }
        Ok(())
    }
}

/// Durable effect-journal key plus indexed lifecycle join columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectJournalIdentity {
    key: String,
    session_id: Option<String>,
}

impl EffectJournalIdentity {
    fn from_scope(scope: &ExecutionScope) -> Self {
        #[derive(Serialize)]
        struct Wire<'a> {
            version: u8,
            kind: &'static str,
            #[serde(skip_serializing_if = "Option::is_none")]
            session_id: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            execution_id: Option<&'a str>,
        }

        let (kind, session_id, execution_id) = match scope {
            ExecutionScope::Turn {
                session_id,
                turn_id,
            } => ("turn", Some(session_id.as_str()), Some(turn_id.as_str())),
            ExecutionScope::QueueDrain {
                session_id,
                drain_id,
            } => ("drain", Some(session_id.as_str()), Some(drain_id.as_str())),
            ExecutionScope::SessionDelete { session_id } => {
                ("delete", Some(session_id.as_str()), None)
            }
            ExecutionScope::Process { process_id } => ("process", None, Some(process_id.as_str())),
            ExecutionScope::RuntimeOperation { operation_id } => {
                ("op", None, Some(operation_id.as_str()))
            }
        };
        let key = serde_json::to_string(&Wire {
            version: 2,
            kind,
            session_id,
            execution_id,
        })
        .expect("effect journal identity contains only infallible string fields");
        Self {
            key,
            session_id: session_id.map(str::to_string),
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectJournalRetirement {
    Session { session_id: String },
    Process { process_id: String },
}

impl EffectJournalRetirement {
    pub fn session(session_id: impl Into<String>) -> Self {
        Self::Session {
            session_id: session_id.into(),
        }
    }

    pub fn process(process_id: impl Into<String>) -> Self {
        Self::Process {
            process_id: process_id.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AwaitEventWaitIdentity {
    ToolCompletion {
        tool_call_id: String,
    },
    ProcessSignal {
        process_id: String,
        signal_name: String,
        ordinal: u64,
    },
    /// Reserved first-writer-wins cancellation-versus-completion gate for a
    /// foreground turn.
    TurnCancelGate,
    /// Reserved terminal publication promise for a foreground turn.
    TurnTerminal,
    Custom {
        key: String,
    },
}

impl AwaitEventWaitIdentity {
    pub fn tool_completion(tool_call_id: impl Into<String>) -> Self {
        Self::ToolCompletion {
            tool_call_id: tool_call_id.into(),
        }
    }

    pub fn process_signal(
        process_id: impl Into<String>,
        signal_name: impl Into<String>,
        ordinal: u64,
    ) -> Self {
        Self::ProcessSignal {
            process_id: process_id.into(),
            signal_name: signal_name.into(),
            ordinal,
        }
    }

    pub(in crate::runtime::effect) fn validate(&self) -> Result<(), RuntimeError> {
        let invalid = match self {
            Self::ToolCompletion { tool_call_id } => tool_call_id.trim().is_empty(),
            Self::ProcessSignal {
                process_id,
                signal_name,
                ordinal,
            } => process_id.trim().is_empty() || signal_name.trim().is_empty() || *ordinal == 0,
            Self::TurnCancelGate | Self::TurnTerminal => false,
            Self::Custom { key } => key.trim().is_empty(),
        };
        if invalid {
            return Err(RuntimeError::new(
                "invalid_await_event_wait_identity",
                "await-event wait identity requires non-empty stable ids",
            ));
        }
        Ok(())
    }

    pub fn is_turn_control(&self) -> bool {
        matches!(self, Self::TurnCancelGate | Self::TurnTerminal)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AwaitEventKey {
    pub scope: ExecutionScope,
    pub wait: AwaitEventWaitIdentity,
    pub key_id: String,
    pub signature: String,
}

impl AwaitEventKey {
    pub fn promise_key(&self) -> String {
        format!("lash-await-event:{}", self.key_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalCompletionError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

impl ExternalCompletionError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            raw: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "payload", rename_all = "snake_case")]
pub enum Resolution {
    Ok(serde_json::Value),
    Err(ExternalCompletionError),
    Timeout,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResolveOutcome {
    Accepted,
    AlreadyResolved { terminal: Resolution },
    UnknownOrRevoked,
}

pub(super) enum ScopedEffectControllerInner<'run> {
    Borrowed(&'run dyn RuntimeEffectController),
    Shared(Arc<dyn RuntimeEffectController>),
}

impl Clone for ScopedEffectControllerInner<'_> {
    fn clone(&self) -> Self {
        match self {
            Self::Borrowed(controller) => Self::Borrowed(*controller),
            Self::Shared(controller) => Self::Shared(Arc::clone(controller)),
        }
    }
}

/// Scoped low-level controller plus the semantic execution scope it is serving.
#[derive(Clone)]
pub struct ScopedEffectController<'run> {
    pub(super) controller: ScopedEffectControllerInner<'run>,
    pub(super) scope: ExecutionScope,
}

impl<'run> ScopedEffectController<'run> {
    pub fn borrowed(
        controller: &'run dyn RuntimeEffectController,
        scope: ExecutionScope,
    ) -> Result<Self, RuntimeError> {
        scope.validate()?;
        Ok(Self {
            controller: ScopedEffectControllerInner::Borrowed(controller),
            scope,
        })
    }

    pub fn shared(
        controller: Arc<dyn RuntimeEffectController>,
        scope: ExecutionScope,
    ) -> Result<Self, RuntimeError> {
        scope.validate()?;
        Ok(Self {
            controller: ScopedEffectControllerInner::Shared(controller),
            scope,
        })
    }

    pub fn controller(&self) -> &dyn RuntimeEffectController {
        match &self.controller {
            ScopedEffectControllerInner::Borrowed(controller) => *controller,
            ScopedEffectControllerInner::Shared(controller) => controller.as_ref(),
        }
    }

    pub fn scope_id(&self) -> &str {
        self.scope.id()
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.scope.turn_id()
    }

    pub(crate) fn to_static(&self) -> Option<ScopedEffectController<'static>> {
        let ScopedEffectControllerInner::Shared(controller) = &self.controller else {
            return None;
        };
        Some(ScopedEffectController {
            controller: ScopedEffectControllerInner::Shared(Arc::clone(controller)),
            scope: self.scope.clone(),
        })
    }
}

pub(crate) mod facade_ops {
    use super::*;

    /// Facade-internal operations for [`ScopedEffectController`].
    ///
    /// This is not integrator surface, carries no stability promise, and exists
    /// only for the `lash` facade. See [ADR 0051](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md).
    pub trait ScopedEffectControllerFacadeOps {
        fn execution_scope(&self) -> &ExecutionScope;
    }

    impl ScopedEffectControllerFacadeOps for ScopedEffectController<'_> {
        fn execution_scope(&self) -> &ExecutionScope {
            &self.scope
        }
    }
}

type EffectControllerTaskFuture<'run> = Pin<Box<dyn Future<Output = ()> + Send + 'run>>;

pub(crate) enum EffectControllerTaskRequest {
    Execute {
        envelope: Box<RuntimeEffectEnvelope>,
        local_executor: Box<RuntimeEffectLocalExecutor<'static>>,
        response: oneshot::Sender<Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>,
    },
    AwaitEventKey {
        scope: ExecutionScope,
        wait: AwaitEventWaitIdentity,
        response: oneshot::Sender<Result<AwaitEventKey, RuntimeError>>,
    },
    ResolveAwaitEvent {
        key: AwaitEventKey,
        resolution: Resolution,
        response: oneshot::Sender<Result<ResolveOutcome, RuntimeError>>,
    },
}

impl EffectControllerTaskRequest {
    fn into_future<'run>(
        self,
        controller: &'run dyn RuntimeEffectController,
    ) -> EffectControllerTaskFuture<'run> {
        match self {
            Self::Execute {
                envelope,
                local_executor,
                response,
            } => Box::pin(async move {
                let _ = response.send(controller.execute_effect(*envelope, *local_executor).await);
            }),
            Self::AwaitEventKey {
                scope,
                wait,
                response,
            } => Box::pin(async move {
                let _ = response.send(controller.await_event_key(&scope, wait).await);
            }),
            Self::ResolveAwaitEvent {
                key,
                resolution,
                response,
            } => Box::pin(async move {
                let _ = response.send(controller.resolve_await_event(&key, resolution).await);
            }),
        }
    }
}

pub(super) struct RemoteLocalExecutionRequest {
    pub(super) envelope: RuntimeEffectEnvelope,
    pub(super) response:
        oneshot::Sender<Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>,
}

#[derive(Clone)]
pub(crate) struct EffectTaskController {
    requests: mpsc::UnboundedSender<EffectControllerTaskRequest>,
    replay_ownership: crate::EffectReplayOwnership,
    allows_process_lifetime_completion_keys: bool,
    supports_concurrent_effects: bool,
}

impl EffectTaskController {
    pub(crate) fn scoped(
        controller: &dyn RuntimeEffectController,
        scope: ExecutionScope,
    ) -> Result<
        (
            ScopedEffectController<'static>,
            mpsc::UnboundedReceiver<EffectControllerTaskRequest>,
        ),
        RuntimeError,
    > {
        let (requests, request_rx) = mpsc::unbounded_channel();
        let proxy = Self {
            requests,
            replay_ownership: controller.replay_ownership(),
            allows_process_lifetime_completion_keys: controller
                .allows_process_lifetime_completion_keys(),
            supports_concurrent_effects: controller.supports_concurrent_effects(),
        };
        Ok((
            ScopedEffectController::shared(Arc::new(proxy), scope)?,
            request_rx,
        ))
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for EffectTaskController {
    fn replay_ownership(&self) -> crate::EffectReplayOwnership {
        self.replay_ownership
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        self.allows_process_lifetime_completion_keys
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::AwaitEventKey {
                scope: scope.clone(),
                wait,
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeError::new(
                    "runtime_effect_controller_task_closed",
                    "await-event key controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeError::new(
                "runtime_effect_controller_task_closed",
                "await-event key controller response was dropped",
            )
        })?
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::ResolveAwaitEvent {
                key: key.clone(),
                resolution,
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeError::new(
                    "runtime_effect_controller_task_closed",
                    "await-event resolution controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeError::new(
                "runtime_effect_controller_task_closed",
                "await-event resolution controller response was dropped",
            )
        })?
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for EffectTaskController {
    fn supports_concurrent_effects(&self) -> bool {
        self.supports_concurrent_effects
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let (local_executor, mut local_execution) = local_executor.into_remote_execution();
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::Execute {
                envelope: Box::new(envelope),
                local_executor: Box::new(local_executor),
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeEffectControllerError::new(
                    "runtime_effect_controller_task_closed",
                    "effect controller task is no longer running",
                )
            })?;

        tokio::pin!(response_rx);
        loop {
            tokio::select! {
                response = &mut response_rx => {
                    return response.map_err(|_| {
                        RuntimeEffectControllerError::new(
                            "runtime_effect_controller_task_closed",
                            "effect controller response was dropped",
                        )
                    })?;
                }
                request = async {
                    match local_execution.as_mut() {
                        Some((_, requests)) => requests.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let Some(request) = request else {
                        // Replay-aware controllers may return a recorded
                        // outcome without invoking local execution. Dropping
                        // the remote executor closes this channel by design;
                        // keep waiting for the controller response.
                        local_execution = None;
                        continue;
                    };
                    let Some((executor, _)) = local_execution.take() else {
                        unreachable!("local execution request requires a local executor");
                    };
                    let result = executor.execute_forwarded(request.envelope).await;
                    let _ = request.response.send(result);
                }
            }
        }
    }
}

pub(crate) async fn drive_effect_controller_task(
    controller: &dyn RuntimeEffectController,
    envelope: RuntimeEffectEnvelope,
    local_executor: RuntimeEffectLocalExecutor<'static>,
    mut requests: mpsc::UnboundedReceiver<EffectControllerTaskRequest>,
) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
    let (root_tx, root_rx) = oneshot::channel();
    let root = EffectControllerTaskRequest::Execute {
        envelope: Box::new(envelope),
        local_executor: Box::new(local_executor),
        response: root_tx,
    };
    let mut stack = vec![root.into_future(controller)];
    let mut requests_open = true;
    tokio::pin!(root_rx);

    loop {
        let Some(active) = stack.last_mut() else {
            return root_rx.await.map_err(|_| {
                RuntimeEffectControllerError::new(
                    "runtime_effect_controller_task_closed",
                    "root effect controller response was dropped",
                )
            })?;
        };
        tokio::select! {
            biased;
            response = &mut root_rx => {
                return response.map_err(|_| {
                    RuntimeEffectControllerError::new(
                        "runtime_effect_controller_task_closed",
                        "root effect controller response was dropped",
                    )
                })?;
            }
            () = active => {
                stack.pop();
            }
            request = async {
                if requests_open {
                    requests.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                match request {
                    Some(request) => stack.push(request.into_future(controller)),
                    None => requests_open = false,
                }
            }
        }
    }
}

/// Shared effect replay and AwaitEvent contract for effect boundaries.
///
/// Both the deployment-level [`EffectHost`] factory and the per-run
/// [`RuntimeEffectController`] resolve AwaitEvents and describe which layer
/// owns effect replay.
#[async_trait::async_trait]
pub trait AwaitEventResolver: Send + Sync {
    fn replay_ownership(&self) -> crate::EffectReplayOwnership {
        crate::EffectReplayOwnership::Runtime
    }

    /// Whether [`ToolContext::completion_key`](crate::ToolContext::completion_key)
    /// may issue an externally routable key through this resolver.
    ///
    /// Replay ownership is only a routing fact and does not imply that another
    /// process can resolve a key after this one exits. Implementations must opt
    /// in explicitly either because their await-event ingress and state survive
    /// process loss or because the deployment explicitly accepts process-local
    /// key lifetime.
    fn allows_process_lifetime_completion_keys(&self) -> bool {
        false
    }

    async fn await_event_key(
        &self,
        _scope: &ExecutionScope,
        _wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        Err(RuntimeError::new(
            "await_event_unsupported",
            "this effect boundary does not support await-event keys",
        ))
    }

    async fn resolve_await_event(
        &self,
        _key: &AwaitEventKey,
        _resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        Ok(ResolveOutcome::UnknownOrRevoked)
    }

    /// Read a keyed promise without waiting for or resolving it.
    ///
    /// Turn owners use this as a synchronous start gate before beginning a
    /// new effect. Durable owners must perform that read through their
    /// handler-scoped, replay-aware controller: its result affects subsequent
    /// command order and therefore must replay identically after an owner
    /// crash. An unresolved promise returns `None` and remains open.
    async fn peek_await_event(
        &self,
        _key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        Err(RuntimeError::new(
            "await_event_unsupported",
            "this effect boundary does not support await-event reads",
        ))
    }

    async fn await_await_event(
        &self,
        _key: &AwaitEventKey,
        _cancel: CancellationToken,
        _deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        Err(RuntimeError::new(
            "await_event_unsupported",
            "this effect boundary does not support await-event waits",
        ))
    }

    async fn revoke_await_events_for_session(&self, _session_id: &str) -> Result<(), RuntimeError> {
        Err(RuntimeError::new(
            "await_event_unsupported",
            "this effect boundary does not support revoking await-event waits",
        ))
    }

    /// Cancel every *outstanding* durable wait for `session_id` without
    /// deleting the session: each waiter receives a terminal
    /// [`Resolution::Cancelled`] instead of hanging, late resolves observe
    /// that terminal, and waits registered afterwards behave normally — in
    /// contrast to [`revoke_await_events_for_session`](Self::revoke_await_events_for_session),
    /// which tombstones the session's waits forever.
    ///
    /// The default errors loudly: an effect boundary that tracks durable waits
    /// must implement this to honor the host lever, and one that cannot must
    /// not silently claim success.
    async fn cancel_await_events_for_session(&self, _session_id: &str) -> Result<(), RuntimeError> {
        Err(RuntimeError::new(
            "await_event_cancel_unsupported",
            "this effect boundary does not support cancelling durable waits",
        ))
    }
}

/// Deployment-level factory for scoped effect controllers.
#[async_trait::async_trait]
pub trait EffectHost: AwaitEventResolver {
    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError>;

    fn scoped_static(
        &self,
        _scope: ExecutionScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        Ok(None)
    }

    /// Retire durable effect history after its owning lifecycle is no longer
    /// executable.
    async fn retire_effect_journal(
        &self,
        _retirement: EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        Err(RuntimeError::new(
            "effect_journal_retirement_unsupported",
            "this effect host does not implement effect-journal retirement",
        ))
    }
}

/// Boundary for nondeterministic runtime work.
#[async_trait::async_trait]
pub trait RuntimeEffectController: AwaitEventResolver {
    /// Advises an engine to end the current in-process execution segment at a
    /// quiescent point. Engines may decline when live state is not capturable,
    /// but must make progress before returning another decline. In particular,
    /// an engine must not repeatedly return the same boundary and unchanged
    /// durable-wait state in one invocation; a host may bound and retry such a
    /// non-progressing invocation.
    fn wants_segment_boundary(&self, _progress: &SegmentProgress) -> Option<BoundaryReason> {
        None
    }

    /// Whether this controller can safely accept overlapping `execute_effect`
    /// calls from one runtime coordinator.
    ///
    /// Local and store-backed controllers can usually fan out independent
    /// effects. Some workflow substrates expose a single ordered journal
    /// context where native operations must be awaited immediately before the
    /// next context call is issued. Those controllers should return `false` so
    /// coordinators serialize child effects while still replaying each child by
    /// its own stable key.
    fn supports_concurrent_effects(&self) -> bool {
        true
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError>;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SegmentProgress {
    pub effects_executed: u64,
    pub journaled_bytes_estimate: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryReason {
    JournalBudget,
    DurationCap,
}

/// Runtime-internal handle for effect-controller references carried through
/// per-turn execution contexts.
#[derive(Clone)]
pub(crate) enum RuntimeEffectControllerHandle<'run> {
    Borrowed(ScopedEffectController<'run>),
    #[cfg(any(test, feature = "testing"))]
    Shared {
        controller: Arc<dyn RuntimeEffectController>,
        scope: ExecutionScope,
    },
}

impl<'run> RuntimeEffectControllerHandle<'run> {
    pub(crate) fn borrowed(scoped: ScopedEffectController<'run>) -> Self {
        Self::Borrowed(scoped)
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn shared(controller: Arc<dyn RuntimeEffectController>) -> Self {
        Self::Shared {
            controller,
            scope: ExecutionScope::runtime_operation("test-runtime-effect-controller"),
        }
    }

    pub(crate) fn controller(&self) -> &dyn RuntimeEffectController {
        match self {
            Self::Borrowed(scoped) => scoped.controller(),
            #[cfg(any(test, feature = "testing"))]
            Self::Shared { controller, .. } => controller.as_ref(),
        }
    }

    pub(crate) fn scoped(&self) -> ScopedEffectController<'_> {
        match self {
            Self::Borrowed(scoped) => scoped.clone(),
            #[cfg(any(test, feature = "testing"))]
            Self::Shared { controller, scope } => {
                ScopedEffectController::shared(Arc::clone(controller), scope.clone())
                    .expect("runtime effect controller handle carries a valid scope")
            }
        }
    }

    pub(crate) fn clone_scoped(&self) -> RuntimeEffectControllerHandle<'run> {
        self.clone()
    }

    pub(crate) fn to_static(&self) -> Option<RuntimeEffectControllerHandle<'static>> {
        match self {
            Self::Borrowed(scoped) => scoped
                .to_static()
                .map(RuntimeEffectControllerHandle::Borrowed),
            #[cfg(any(test, feature = "testing"))]
            Self::Shared { controller, scope } => Some(RuntimeEffectControllerHandle::Shared {
                controller: Arc::clone(controller),
                scope: scope.clone(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_identity_is_typed_and_session_qualified() {
        let scopes = [
            ExecutionScope::turn("session", "shared"),
            ExecutionScope::queue_drain("session", "shared"),
            ExecutionScope::session_delete("session"),
            ExecutionScope::process("shared"),
            ExecutionScope::runtime_operation("shared"),
        ];
        let identities = scopes
            .iter()
            .map(|scope| scope.journal_identity().expect("durable identity"))
            .collect::<Vec<_>>();
        let keys = identities
            .iter()
            .map(EffectJournalIdentity::key)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(keys.len(), scopes.len());
        for identity in &identities[..3] {
            assert_eq!(identity.session_id(), Some("session"));
        }
        for identity in &identities[3..] {
            assert_eq!(identity.session_id(), None);
        }
    }
}
