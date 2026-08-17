use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{RuntimeError, RuntimeErrorCode};

use super::super::envelope::{RuntimeEffectEnvelope, RuntimeEffectOutcome};
use super::super::group::{
    EffectGroupHandle, GroupSettlement, LoserDisposition, RuntimeEffectGroup,
};
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
    /// Constructs the stable session-and-turn scope effect-host implementors use to key one turn's
    /// durable effects.
    pub fn turn(session_id: impl Into<String>, turn_id: impl Into<String>) -> Self {
        Self::Turn {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
        }
    }

    /// Constructs the stable process scope effect-host implementors use to key effects that outlive
    /// any one session turn.
    pub fn process(process_id: impl Into<String>) -> Self {
        Self::Process {
            process_id: process_id.into(),
        }
    }

    /// Constructs the stable session-and-drain scope effect-host implementors use to key
    /// queued-work effects outside a turn.
    pub fn queue_drain(session_id: impl Into<String>, drain_id: impl Into<String>) -> Self {
        Self::QueueDrain {
            session_id: session_id.into(),
            drain_id: drain_id.into(),
        }
    }

    /// Constructs the stable session-delete scope effect-host implementors use to journal deletion
    /// work outside a turn.
    pub fn session_delete(session_id: impl Into<String>) -> Self {
        Self::SessionDelete {
            session_id: session_id.into(),
        }
    }

    /// Constructs a runtime-operation scope effect-host implementors can journal when no session or
    /// process owns the work.
    pub fn runtime_operation(operation_id: impl Into<String>) -> Self {
        Self::RuntimeOperation {
            operation_id: operation_id.into(),
        }
    }

    /// Exposes id to store and durable-substrate implementors and effect-host implementors while
    /// snapshotting or restoring durable session state.
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

    /// Exposes session id to store and durable-substrate implementors and effect-host implementors
    /// while snapshotting or restoring durable session state. Returns `None` when no session id is
    /// present.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Turn { session_id, .. }
            | Self::QueueDrain { session_id, .. }
            | Self::SessionDelete { session_id, .. } => Some(session_id),
            Self::Process { .. } | Self::RuntimeOperation { .. } => None,
        }
    }

    /// Exposes turn id to store and durable-substrate implementors and effect-host implementors
    /// while snapshotting or restoring durable session state. Returns `None` when no turn id is
    /// present.
    pub fn turn_id(&self) -> Option<&str> {
        match self {
            Self::Turn { turn_id, .. } => Some(turn_id),
            _ => None,
        }
    }

    /// Reports whether effect-host implementors may validate a trace turn ID against this scope;
    /// only a turn scope carries that identity.
    pub fn validates_turn_trace_id(&self) -> bool {
        matches!(self, Self::Turn { .. })
    }

    /// Rejects empty stable identifiers before store or effect-host implementors persist a scope;
    /// turn and queue-drain scopes require both component IDs.
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
    /// Constructs a session-wide retirement request for effect-host implementors removing every
    /// durable effect journal entry owned by a deleted session.
    pub fn session(session_id: impl Into<String>) -> Self {
        Self::Session {
            session_id: session_id.into(),
        }
    }

    /// Constructs a process-wide retirement request for effect-host implementors removing every
    /// durable effect journal entry owned by a terminal process.
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
    /// Constructs the stable wait identity effect-host implementors use to resolve a deferred tool
    /// call by its call ID.
    pub fn tool_completion(tool_call_id: impl Into<String>) -> Self {
        Self::ToolCompletion {
            tool_call_id: tool_call_id.into(),
        }
    }

    /// Constructs the stable wait identity effect-host implementors use to resolve one named
    /// process signal without colliding with other signals or attempts.
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
                crate::RuntimeErrorCode::InvalidAwaitEventWaitIdentity,
                "await-event wait identity requires non-empty stable ids",
            ));
        }
        Ok(())
    }

    /// Lets effect-host implementors distinguish the reserved turn-control wait from ordinary tool
    /// and application waits.
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
    /// Derives the deterministic promise key effect-host implementors use to rendezvous durable
    /// wait resolution with its execution scope and wait identity.
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
    /// Validates a scope and binds a borrowed controller for effect-host implementors; invalid or
    /// empty scope identities are rejected before execution.
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

    /// Validates a scope and binds an owned controller for effect-host implementors that must move
    /// the scoped host across an asynchronous boundary.
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

    /// Exposes controller to effect-host implementors while scoping and journaling durable effects.
    pub fn controller(&self) -> &dyn RuntimeEffectController {
        match &self.controller {
            ScopedEffectControllerInner::Borrowed(controller) => *controller,
            ScopedEffectControllerInner::Shared(controller) => controller.as_ref(),
        }
    }

    /// Exposes scope id to effect-host implementors while scoping and journaling durable effects.
    pub fn scope_id(&self) -> &str {
        self.scope.id()
    }

    /// Exposes turn id to effect-host implementors while scoping and journaling durable effects.
    /// Returns `None` when no turn id is present.
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

    pub(crate) fn owned_controller(&self) -> Option<Arc<dyn RuntimeEffectController>> {
        match &self.controller {
            ScopedEffectControllerInner::Shared(controller) => Some(Arc::clone(controller)),
            ScopedEffectControllerInner::Borrowed(_) => None,
        }
    }
}

pub(crate) mod facade_ops {
    use super::*;

    /// Facade-internal operations for [`ScopedEffectController`].
    ///
    /// This is not integrator surface, carries no stability promise, and exists
    /// only for the `lash` facade. See [ADR 0051](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md).
    #[async_trait::async_trait]
    pub trait ScopedEffectControllerFacadeOps {
        fn execution_scope(&self) -> &ExecutionScope;

        /// Executes one facade-owned process effect while making this controller
        /// available to the local process command itself. Borrowed controllers
        /// are proxied across the process task boundary; shared controllers can
        /// be passed through directly.
        #[doc(hidden)]
        async fn execute_process_effect(
            &self,
            envelope: RuntimeEffectEnvelope,
            local_executor: RuntimeEffectLocalExecutor<'static>,
        ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError>;
    }

    #[async_trait::async_trait]
    impl ScopedEffectControllerFacadeOps for ScopedEffectController<'_> {
        fn execution_scope(&self) -> &ExecutionScope {
            &self.scope
        }

        async fn execute_process_effect(
            &self,
            envelope: RuntimeEffectEnvelope,
            local_executor: RuntimeEffectLocalExecutor<'static>,
        ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
            let controller = self.controller();
            let (owned_controller, task_requests) = if let Some(owned) = self.owned_controller() {
                (owned, None)
            } else {
                let (proxy, requests) =
                    EffectTaskController::scoped(controller, self.execution_scope().clone())?;
                (
                    proxy
                        .owned_controller()
                        .expect("effect-task proxy owns its controller"),
                    Some(requests),
                )
            };
            let local_executor = local_executor.with_process_effect_controller(owned_controller);
            if let Some(task_requests) = task_requests {
                drive_effect_controller_task(controller, envelope, local_executor, task_requests)
                    .await
            } else {
                controller.execute_effect(envelope, local_executor).await
            }
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
    durable_workflow_controller: bool,
    journal_addressing: crate::EffectJournalAddressing,
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
            durable_workflow_controller: controller.durable_workflow_controller(),
            journal_addressing: controller.journal_addressing(),
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

    fn durable_workflow_controller(&self) -> bool {
        self.durable_workflow_controller
    }

    fn journal_addressing(&self) -> crate::EffectJournalAddressing {
        self.journal_addressing
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
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "await-event key controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeError::new(
                crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
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
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "await-event resolution controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeError::new(
                crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
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
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "effect controller task is no longer running",
                )
            })?;

        tokio::pin!(response_rx);
        loop {
            tokio::select! {
                response = &mut response_rx => {
                    return response.map_err(|_| {
                        RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
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
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "root effect controller response was dropped",
                )
            })?;
        };
        tokio::select! {
            biased;
            response = &mut root_rx => {
                return response.map_err(|_| {
                    RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
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

    /// Whether this boundary is a durable workflow controller whose engine owns
    /// retry pacing for the whole invocation.
    ///
    /// Opting in changes exactly one behaviour: a queued-work drain that finds
    /// the session execution lane held no longer answers "nothing to drain"
    /// (which an engine would read as a settled queue). It waits out a
    /// crashed-looking holder and otherwise fails with the typed retryable
    /// [`RuntimeErrorCode::SessionExecutionLaneBusy`](crate::RuntimeErrorCode::SessionExecutionLaneBusy),
    /// which the controller surfaces to its engine so the engine's retry policy
    /// paces the next attempt.
    ///
    /// This is deliberately a separate question from
    /// [`replay_ownership`](Self::replay_ownership): a store-backed durable
    /// effect host also owns effect replay, yet has no engine-side retry policy
    /// to hand the outcome to, so it must keep the ordinary one-shot drain
    /// contract. Implement this only for a boundary whose *invocation* is
    /// re-driven by a durable engine.
    fn durable_workflow_controller(&self) -> bool {
        false
    }

    /// Describes how this boundary addresses replayed journal entries.
    ///
    /// Keep this capability independent from any future
    /// `durable_workflow_controller()` capability. The two are candidates for
    /// a consolidated controller-traits surface, but durability and journal
    /// addressing answer different questions today.
    fn journal_addressing(&self) -> crate::EffectJournalAddressing {
        crate::EffectJournalAddressing::Runtime
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
            crate::RuntimeErrorCode::AwaitEventUnsupported,
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
            crate::RuntimeErrorCode::AwaitEventUnsupported,
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
            crate::RuntimeErrorCode::AwaitEventUnsupported,
            "this effect boundary does not support await-event waits",
        ))
    }

    async fn revoke_await_events_for_session(&self, _session_id: &str) -> Result<(), RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventUnsupported,
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
            crate::RuntimeErrorCode::AwaitEventCancelUnsupported,
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
            crate::RuntimeErrorCode::EffectJournalRetirementUnsupported,
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

    /// Whether this controller implements durable child completion and
    /// first-settlement wake (FIG-1416).
    ///
    /// Deliberately a different question from
    /// [`supports_concurrent_effects`](Self::supports_concurrent_effects), which
    /// asks "may one coordinator issue overlapping *unstructured*
    /// `execute_effect` calls?". This asks "can this host run a *structured
    /// group* of children durably and tell me, durably, which settled first?".
    /// A single-journal-context engine answers no to the first and yes to the
    /// second — Restate does.
    ///
    /// This is checked once at deployment validation rather than per call: the
    /// group path is the only tool-batch path, so a controller answering `false`
    /// has no batch path at all, and a host wiring one should learn that at
    /// startup instead of mid-turn on the first `Promise.all`. It gates
    /// *admission*, not dispatch.
    ///
    /// A host may answer `true` only if it can also supply `'static` executors
    /// for the children: a child must be able to outlive its caller to honor
    /// [`LoserDisposition::RunToCompletion`], and
    /// [`EffectHost::scoped_static`] is explicitly not universally available.
    /// The two capabilities are one question and must not drift apart.
    fn supports_effect_groups(&self) -> bool {
        false
    }

    /// Open — or replay — a group of independently journaled child effects.
    ///
    /// Returns once the group is durably recorded, **not** when a child settles.
    ///
    /// `executors` is a per-child *factory* rather than a borrowed executor
    /// precisely because children must outlive the caller's future under
    /// [`LoserDisposition::RunToCompletion`] — the borrow-scoped
    /// `RuntimeEffectLocalExecutor<'_>` taken by
    /// [`execute_effect`](Self::execute_effect) carries the one lifetime this
    /// contract exists to break.
    ///
    /// The default errors loudly rather than mis-executing a group, matching
    /// [`AwaitEventResolver::cancel_await_events_for_session`]: an out-of-tree
    /// controller that has not implemented groups fails closed with a named
    /// error.
    async fn open_effect_group(
        &self,
        _group: RuntimeEffectGroup,
        _executors: Arc<dyn Fn(usize) -> RuntimeEffectLocalExecutor<'static> + Send + Sync>,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::EffectGroupUnsupported,
            "this effect controller does not implement durable effect groups",
        ))
    }

    /// Await the next settlement in the group's durable settlement order.
    ///
    /// The obligation, stated engine-portably: **settlement `n` of a group is a
    /// durable fact, and every replay observes the same child at position `n`.**
    /// A host must not re-derive position `n` by racing live children once `n`
    /// has been decided. How the fact is stored is the host's business — a SQL
    /// row, a Restate journal entry, a Temporal history event.
    ///
    /// Settlements are served by *rank* — the child holding the
    /// `(consumed + 1)`-th smallest sequence — never by literal sequence
    /// equality, because sequences are monotonic without being gapless.
    async fn await_next_settlement(
        &self,
        _handle: &EffectGroupHandle,
        _cancel: CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::EffectGroupUnsupported,
            "this effect controller does not implement durable effect groups",
        ))
    }

    /// Release the caller's interest in the group.
    ///
    /// Under [`LoserDisposition::RunToCompletion`] the remaining children keep
    /// running under host ownership and journal their own settlements; the host
    /// owns their redrive. Under [`LoserDisposition::Cancel`] the host cancels
    /// them and journals each cancellation as that child's terminal. Either way
    /// the caller may not observe further settlements.
    async fn close_effect_group(
        &self,
        _handle: EffectGroupHandle,
        _disposition: LoserDisposition,
    ) -> Result<(), RuntimeEffectControllerError> {
        Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::EffectGroupUnsupported,
            "this effect controller does not implement durable effect groups",
        ))
    }
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
