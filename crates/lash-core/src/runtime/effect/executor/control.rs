use crate::ProcessId;
use crate::SessionId;
use crate::TurnId;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::runtime::native_substrate::lane_wait as queued_lane_wait;
use crate::{RuntimeError, RuntimeErrorCode};

use super::super::envelope::{RuntimeEffectEnvelope, RuntimeEffectOutcome};
use super::super::group::{EffectGroupHandle, GroupSettlement, LoserPolicy, RuntimeEffectGroup};
use super::await_event_support::await_event_scope_not_retirable;
use super::{RuntimeEffectControllerError, RuntimeEffectLocalExecutor, TurnCancelWait};
use super::{TurnControlAuthorityOwner, TurnControlBinding, TurnControlParticipation};

// =============================================================================
// Effect host + controller trait + scope + error
// =============================================================================

pub use lash_sansio::{EffectJournalIdentity, ExecutionScope};

/// Who proves that a scope-exact retirement can no longer be reached.
///
/// Retirement deletes journal rows and fences the scope forever, so it must
/// rest on proven unreachability (ADR 0049 reclaim model). There are exactly
/// two proofs, and the caller names which one it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectRetirementGate {
    /// The scope's owner is terminal by the owner's own record: the process
    /// registry pruned the row, so no redrive can ever run under the scope
    /// again. The store deletes whatever is journaled, in-flight rows
    /// included — a finalizer that arrives later is refused by the fence.
    OwnerTerminal,
    /// The store itself must witness that nothing is live: no effect row is
    /// still `in_progress` under the scope (grouped children draining after a
    /// run-to-completion close count). A live scope is left untouched and the
    /// retirement reports `effect_scope_not_quiescent`, so the caller retries
    /// once the work settles.
    WhenQuiescent,
}

/// One retirement request against the durable effect journal.
///
/// `Session` names a family of scopes (every turn, drain, and delete scope the
/// session owns); `Process` and `RuntimeOperation` each name one exact
/// non-session scope. Retiring an exact scope deletes its effect children, its
/// groups, and its await-event promise rows in one transaction and leaves a
/// scope-retirement fence behind, so the scope can never be re-admitted — not
/// by a late redrive, not after a restart. A process fence lasts until the
/// same process id is registered again (ADR 0049); a runtime-operation fence
/// is permanent.
///
/// Every scope-exact retirement carries an [`EffectRetirementGate`]: the
/// constructors build the owner-terminal form, and
/// [`when_quiescent`](Self::when_quiescent) asks the store to prove
/// unreachability instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectJournalRetirement {
    Session {
        session_id: SessionId,
    },
    Process {
        process_id: ProcessId,
        gate: EffectRetirementGate,
    },
    RuntimeOperation {
        operation_id: String,
        gate: EffectRetirementGate,
    },
}

impl EffectJournalRetirement {
    /// Constructs a session-wide retirement request for effect-host implementors removing every
    /// durable effect journal entry owned by a deleted session.
    pub fn session(session_id: impl Into<SessionId>) -> Self {
        Self::Session {
            session_id: session_id.into(),
        }
    }

    /// Constructs a process-wide retirement request for effect-host implementors removing every
    /// durable effect journal entry owned by a terminal process. The gate is
    /// [`EffectRetirementGate::OwnerTerminal`]: the process registry's prune is
    /// the proof, so in-flight rows go too.
    pub fn process(process_id: impl Into<ProcessId>) -> Self {
        Self::Process {
            process_id: process_id.into(),
            gate: EffectRetirementGate::OwnerTerminal,
        }
    }

    /// Constructs a runtime-operation retirement request for effect-host implementors removing
    /// every durable effect journal entry and await-event promise a terminal runtime operation
    /// owns. The gate is [`EffectRetirementGate::OwnerTerminal`]; a caller that
    /// holds no such proof asks for [`when_quiescent`](Self::when_quiescent).
    pub fn runtime_operation(operation_id: impl Into<String>) -> Self {
        Self::RuntimeOperation {
            operation_id: operation_id.into(),
            gate: EffectRetirementGate::OwnerTerminal,
        }
    }

    /// Gate this scope-exact retirement on the store's own quiescence proof:
    /// it succeeds only when no effect is still in progress under the scope,
    /// and otherwise fails with `effect_scope_not_quiescent` without deleting
    /// or fencing anything. A session-wide retirement is returned unchanged.
    #[must_use]
    pub fn when_quiescent(self) -> Self {
        match self {
            Self::Session { .. } => self,
            Self::Process { process_id, .. } => Self::Process {
                process_id,
                gate: EffectRetirementGate::WhenQuiescent,
            },
            Self::RuntimeOperation { operation_id, .. } => Self::RuntimeOperation {
                operation_id,
                gate: EffectRetirementGate::WhenQuiescent,
            },
        }
    }

    /// The proof this scope-exact retirement rests on, or `None` for a
    /// session-wide family, which is always owner-terminal by construction.
    pub fn gate(&self) -> Option<EffectRetirementGate> {
        match self {
            Self::Session { .. } => None,
            Self::Process { gate, .. } | Self::RuntimeOperation { gate, .. } => Some(*gate),
        }
    }

    /// The exact scope this retirement fences, or `None` for a session-wide
    /// family. The scope-retirement fence is keyed by this scope's journal
    /// identity, which is why the two non-session variants and their
    /// [`ExecutionScope`] twins must never drift apart.
    pub fn retired_scope(&self) -> Option<ExecutionScope> {
        match self {
            Self::Session { .. } => None,
            Self::Process { process_id, .. } => Some(ExecutionScope::process(process_id.clone())),
            Self::RuntimeOperation { operation_id, .. } => {
                Some(ExecutionScope::runtime_operation(operation_id.clone()))
            }
        }
    }

    /// The retirement that fences exactly `scope`, or `None` for a
    /// session-bearing scope, which is retired as a family through
    /// [`EffectJournalRetirement::session`].
    pub fn for_scope(scope: &ExecutionScope) -> Option<Self> {
        match scope {
            ExecutionScope::Process { process_id } => Some(Self::process(process_id.clone())),
            ExecutionScope::RuntimeOperation { operation_id } => {
                Some(Self::runtime_operation(operation_id.clone()))
            }
            ExecutionScope::Turn { .. }
            | ExecutionScope::QueueDrain { .. }
            | ExecutionScope::SessionDelete { .. } => None,
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
        process_id: ProcessId,
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
    /// Reserved first-writer-wins escalation promise for a foreground turn:
    /// written only by an immediate request that found the cancellation gate
    /// already holding an after-step request.
    TurnCancelEscalation,
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
        process_id: impl Into<ProcessId>,
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
            Self::TurnCancelGate | Self::TurnTerminal | Self::TurnCancelEscalation => false,
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
        matches!(
            self,
            Self::TurnCancelGate | Self::TurnTerminal | Self::TurnCancelEscalation
        )
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

/// A controller built for one scope that can build itself for another: what
/// an engine-side controller that must know the scope of every effect it runs
/// hands to [`ScopedEffectController::owned`], so the runtime's rescoping (a
/// turn under a process, a child under its parent) keeps the scope exact.
pub trait ScopeBoundController: RuntimeEffectController {
    /// This controller, bound to `scope` instead; it lives as long as this one does.
    fn for_scope<'a>(&self, scope: ExecutionScope) -> Arc<dyn ScopeBoundController + 'a>
    where
        Self: 'a;
}

pub(super) enum ScopedEffectControllerInner<'run> {
    Borrowed(&'run dyn RuntimeEffectController),
    Shared(Arc<dyn RuntimeEffectController>),
    /// A controller built for this scope alone and living no longer than the
    /// borrow it wraps: an engine-side controller that records the scope's
    /// executing effects under the scope (FIG-2499).
    Owned(Arc<dyn ScopeBoundController + 'run>),
}

impl Clone for ScopedEffectControllerInner<'_> {
    fn clone(&self) -> Self {
        match self {
            Self::Borrowed(controller) => Self::Borrowed(*controller),
            Self::Shared(controller) => Self::Shared(Arc::clone(controller)),
            Self::Owned(controller) => Self::Owned(Arc::clone(controller)),
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
    /// Returns the execution scope this controller has admitted.
    pub fn execution_scope(&self) -> &ExecutionScope {
        &self.scope
    }

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

    /// Validates a scope and binds a controller built for that scope and bounded by the borrow it
    /// wraps, for effect-host implementors whose engine-side controller must know the scope of
    /// every effect it runs.
    pub fn owned(
        controller: Arc<dyn ScopeBoundController + 'run>,
        scope: ExecutionScope,
    ) -> Result<Self, RuntimeError> {
        scope.validate()?;
        Ok(Self {
            controller: ScopedEffectControllerInner::Owned(controller),
            scope,
        })
    }

    /// Exposes controller to effect-host implementors while scoping and journaling durable effects.
    pub fn controller(&self) -> &dyn RuntimeEffectController {
        match &self.controller {
            ScopedEffectControllerInner::Borrowed(controller) => *controller,
            ScopedEffectControllerInner::Shared(controller) => controller.as_ref(),
            ScopedEffectControllerInner::Owned(controller) => controller.as_ref(),
        }
    }

    fn validate_envelope_scope(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Result<(), RuntimeEffectControllerError> {
        envelope.invocation.validate_execution_scope(&self.scope)
    }

    /// Executes an effect only after proving that its address belongs to this
    /// controller's admitted scope.
    pub async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.validate_envelope_scope(&envelope)?;
        self.controller()
            .execute_effect(envelope, local_executor)
            .await
    }

    /// Exposes scope id to effect-host implementors while scoping and journaling durable effects.
    pub fn scope_id(&self) -> &str {
        self.scope.id()
    }

    /// Exposes turn id to effect-host implementors while scoping and journaling durable effects.
    /// Returns `None` when no turn id is present.
    pub fn turn_id(&self) -> Option<&TurnId> {
        self.scope.turn_id()
    }

    /// The complete turn-cancel trio for a wait built directly against this
    /// scope, for the wait sites that have no `RuntimeExecutionContext` to ask.
    /// The trio it yields is always observing: a scope alone cannot say whether
    /// the enclosing execution opted out.
    ///
    /// That is exact for the turn-driver and test-dispatch sites that call this
    /// producer: both only run work that observes turn cancellation. Process
    /// bodies instead build one unobserved trio at their execution boundary and
    /// carry it through retry sleeps and deferred-tool awaits whole.
    pub(crate) fn turn_cancel_wait(&self, cancellation: CancellationToken) -> TurnCancelWait {
        TurnCancelWait::observing(cancellation, self.scope.clone())
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
            ScopedEffectControllerInner::Borrowed(_) | ScopedEffectControllerInner::Owned(_) => {
                None
            }
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
            self.validate_envelope_scope(&envelope)?;
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
                drive_effect_controller_task(
                    controller,
                    self.execution_scope().clone(),
                    envelope,
                    local_executor,
                    task_requests,
                )
                .await
            } else {
                self.execute_effect(envelope, local_executor).await
            }
        }
    }
}

type EffectControllerTaskFuture<'run> = Pin<Box<dyn Future<Output = ()> + Send + 'run>>;

/// Guard on the acquired lane. Opaque newtype over `SessionExecutionLeaseGuard`.
pub struct QueuedLaneGuard(crate::runtime::session_execution_lease::SessionExecutionLeaseGuard);

impl QueuedLaneGuard {
    pub(crate) fn new(
        guard: crate::runtime::session_execution_lease::SessionExecutionLeaseGuard,
    ) -> Self {
        Self(guard)
    }

    pub(crate) fn into_inner(
        self,
    ) -> crate::runtime::session_execution_lease::SessionExecutionLeaseGuard {
        self.0
    }
}

/// Opaque guard holding one facade tool-intent submission gate.
#[allow(dead_code)]
pub struct ToolIntentSubmissionGuard(tokio::sync::OwnedMutexGuard<()>);

impl ToolIntentSubmissionGuard {
    /// Wrap the owned mutex guard supplied by the facade's submission-gate
    /// collaborator.
    #[doc(hidden)]
    pub fn from_owned_mutex_guard(guard: tokio::sync::OwnedMutexGuard<()>) -> Self {
        Self(guard)
    }
}

/// Core-provided collaborator for durable tool-intent admission and outcome
/// recording. The effect-host seam never learns which registry or lock table
/// backs these operations.
#[async_trait::async_trait]
pub trait ToolIntentOutcomeSink: Send + Sync {
    async fn lock_submission_gate(&self, replay_key: &str) -> ToolIntentSubmissionGuard;

    async fn admit(
        &self,
        record: crate::ToolIntentSubmissionRecord,
    ) -> Result<crate::ToolIntentSubmissionAdmission, RuntimeError>;

    async fn complete_submission(
        &self,
        identity: &crate::ToolIntentIdentity,
        outcome: crate::ToolIntentExecutionOutcome,
    ) -> Result<(), RuntimeError>;

    async fn retain_in_journal(
        &self,
        identity: &crate::ToolIntentIdentity,
        submitted: crate::ToolIntent,
        outcome: crate::ToolIntentExecutionOutcome,
    ) -> Result<(), RuntimeError>;
}

/// Preparation chosen by the effect host before one tool intent is realized.
pub enum ToolIntentPreparation {
    /// The controller journal owns the submission.
    ControllerOwned,
    /// The runtime registry owns the submission row and the gate remains held
    /// until realization and outcome recording finish.
    RuntimeOwned {
        admission: crate::ToolIntentSubmissionAdmission,
        _guard: ToolIntentSubmissionGuard,
    },
}

/// Whether a runtime-effect failure should end the controller invocation or
/// be recorded as an ordinary failed turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeEffectFailureDisposition {
    AbortInvocation,
    RecordTurnFailure,
}

/// Result of preparing an externally routable tool completion key.
pub enum CompletionKeyPreparation {
    NotNeeded,
    Unsupported,
    Issued(AwaitEventKey),
}

/// One attempt at the durable lane a queued drain must own, plus the facts a
/// bounded wait needs. Opaque: no store type, no lease timings, no guard
/// internals cross the seam.
#[derive(Clone)]
pub struct QueuedLaneHolder(crate::store::SessionExecutionLease);

/// Prints only the [`QueuedLaneHolder::describe`] facts. The inner store row
/// carries the lease token, which must never reach logs or panic messages.
impl std::fmt::Debug for QueuedLaneHolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("QueuedLaneHolder")
            .field(&self.describe())
            .finish()
    }
}

impl QueuedLaneHolder {
    pub fn new(holder: crate::store::SessionExecutionLease) -> Self {
        Self(holder)
    }

    pub fn lease(&self) -> &crate::store::SessionExecutionLease {
        &self.0
    }

    /// The holder's own persisted lease term. The only honest budget unit for
    /// waiting one out — see `native_substrate/lane_wait.rs`.
    pub fn lease_term_ms(&self) -> u64 {
        self.0.lease_term_ms
    }

    /// True iff this observation proves the holder renewed under an unchanged
    /// identity triple since `previous`: alive, not crashed.
    pub fn renewed_since(&self, previous: &QueuedLaneHolder) -> bool {
        self.0.owner == previous.0.owner
            && self.0.executor_id == previous.0.executor_id
            && self.0.expires_at_epoch_ms > previous.0.expires_at_epoch_ms
    }

    /// Describes the persisted holder identity and lease facts for diagnostics.
    pub fn describe(&self) -> String {
        format!(
            "owner `{}` incarnation `{}` executor `{}` (fencing generation {}, expires at {})",
            self.0.owner.owner_id,
            self.0.owner.incarnation_id,
            self.0.executor_id,
            self.0.fencing_token,
            self.0.expires_at_epoch_ms,
        )
    }
}

/// Result of one attempt to acquire the queued-work execution lane.
pub enum QueuedLaneAttempt {
    Acquired(QueuedLaneGuard),
    Busy(QueuedLaneHolder),
}

/// Result of applying a boundary's queued-lane acquisition policy.
pub enum QueuedLaneAcquisition {
    Acquired(QueuedLaneGuard),
    NotAcquired,
}

/// Core-provided probe. Owns everything it needs — `Arc<dyn RuntimePersistence>`,
/// copied owner/executor identity, copied `LeaseTimings`, `Arc<dyn Clock>` — so
/// it can cross an owned channel. The substrate decides how many times and how
/// long to try; it never learns what it is trying.
#[async_trait::async_trait]
pub trait QueuedLaneProbe: Send + Sync {
    async fn try_acquire(&self) -> Result<QueuedLaneAttempt, RuntimeError>;
    /// Sleep `slice` through the runtime's injected clock.
    async fn pause(&self, slice: std::time::Duration);
}

pub(crate) enum EffectControllerTaskRequest {
    Execute {
        scope: ExecutionScope,
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
    AcquireQueuedLane {
        lane: Arc<dyn QueuedLaneProbe>,
        cancel: CancellationToken,
        response: oneshot::Sender<Result<QueuedLaneAcquisition, RuntimeError>>,
    },
    PrepareCompletionKey {
        scope: ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
        response: oneshot::Sender<Result<CompletionKeyPreparation, RuntimeError>>,
    },
    RuntimeEffectFailureDisposition {
        code: RuntimeErrorCode,
        response: oneshot::Sender<Result<RuntimeEffectFailureDisposition, RuntimeError>>,
    },
    TurnControlParticipation {
        response: oneshot::Sender<Result<TurnControlParticipation, RuntimeError>>,
    },
}

impl EffectControllerTaskRequest {
    fn into_future<'run>(
        self,
        controller: &'run dyn RuntimeEffectController,
    ) -> EffectControllerTaskFuture<'run> {
        match self {
            Self::Execute {
                scope,
                envelope,
                local_executor,
                response,
            } => Box::pin(async move {
                let result = if envelope.invocation.execution_scope() != &scope {
                    Err(RuntimeEffectControllerError::new(
                        RuntimeErrorCode::RuntimeEffectScopeMismatch,
                        format!(
                            "proxied effect address scope {:?} does not match admitted controller scope {scope:?}",
                            envelope.invocation.execution_scope()
                        ),
                    ))
                } else {
                    controller.execute_effect(*envelope, *local_executor).await
                };
                let _ = response.send(result);
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
            Self::AcquireQueuedLane {
                lane,
                cancel,
                response,
            } => Box::pin(async move {
                let _ = response.send(controller.acquire_queued_lane(lane, cancel).await);
            }),
            Self::PrepareCompletionKey {
                scope,
                wait,
                may_defer,
                response,
            } => Box::pin(async move {
                let _ = response.send(
                    controller
                        .prepare_completion_key(&scope, wait, may_defer)
                        .await,
                );
            }),
            Self::RuntimeEffectFailureDisposition { code, response } => Box::pin(async move {
                let _ = response.send(controller.runtime_effect_failure_disposition(code).await);
            }),
            Self::TurnControlParticipation { response } => Box::pin(async move {
                let _ = response.send(controller.turn_control_participation().await);
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
    scope: ExecutionScope,
    supports_concurrent_effects: bool,
    owns_commit_backpressure: bool,
    await_event_authority_binding_id: Option<String>,
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
            scope: scope.clone(),
            supports_concurrent_effects: controller.supports_concurrent_effects(),
            owns_commit_backpressure: controller.owns_commit_backpressure(),
            await_event_authority_binding_id: controller.await_event_authority_binding_id(),
        };
        Ok((
            ScopedEffectController::shared(Arc::new(proxy), scope)?,
            request_rx,
        ))
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for EffectTaskController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.await_event_authority_binding_id.clone()
    }

    async fn acquire_queued_lane(
        &self,
        lane: Arc<dyn QueuedLaneProbe>,
        cancel: CancellationToken,
    ) -> Result<QueuedLaneAcquisition, RuntimeError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::AcquireQueuedLane {
                lane,
                cancel,
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeError::new(
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "queued-lane controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeError::new(
                crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "queued-lane controller response was dropped",
            )
        })?
    }

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::PrepareCompletionKey {
                scope: scope.clone(),
                wait,
                may_defer,
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeError::new(
                    RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "completion-key controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeError::new(
                RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "completion-key controller response was dropped",
            )
        })?
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
    fn owns_commit_backpressure(&self) -> bool {
        self.owns_commit_backpressure
    }

    fn supports_concurrent_effects(&self) -> bool {
        self.supports_concurrent_effects
    }

    async fn runtime_effect_failure_disposition(
        &self,
        code: RuntimeErrorCode,
    ) -> Result<RuntimeEffectFailureDisposition, RuntimeError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(
                EffectControllerTaskRequest::RuntimeEffectFailureDisposition {
                    code,
                    response: response_tx,
                },
            )
            .map_err(|_| {
                RuntimeError::new(
                    RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "effect-failure disposition controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeError::new(
                RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "effect-failure disposition controller response was dropped",
            )
        })?
    }

    async fn turn_control_participation(&self) -> Result<TurnControlParticipation, RuntimeError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::TurnControlParticipation {
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeError::new(
                    RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "turn-control participation controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeError::new(
                RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "turn-control participation controller response was dropped",
            )
        })?
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if envelope.invocation.execution_scope() != &self.scope {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectScopeMismatch,
                format!(
                    "proxied effect address scope {:?} does not match admitted controller scope {:?}",
                    envelope.invocation.execution_scope(),
                    self.scope
                ),
            ));
        }
        let (local_executor, mut local_execution) = local_executor.into_remote_execution();
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::Execute {
                scope: self.scope.clone(),
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
    scope: ExecutionScope,
    envelope: RuntimeEffectEnvelope,
    local_executor: RuntimeEffectLocalExecutor<'static>,
    mut requests: mpsc::UnboundedReceiver<EffectControllerTaskRequest>,
) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
    let (root_tx, root_rx) = oneshot::channel();
    let root = EffectControllerTaskRequest::Execute {
        scope,
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

/// Shared AwaitEvent contract for effect boundaries.
///
/// Both the deployment-level [`EffectHost`] factory and the per-run
/// [`RuntimeEffectController`] resolve AwaitEvents.
#[async_trait::async_trait]
pub trait AwaitEventResolver: Send + Sync {
    /// Stable identity of the durable authority that minted keys accepted by
    /// this resolver. Durable turn-control composition uses this to prevent a
    /// host label from being paired with another owner's controller and keys.
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }

    /// Acquire the authoritative session-execution lane a durable queued drain
    /// needs before it may claim work.
    ///
    /// The default is the one-shot contract every non-re-driven boundary owes:
    /// one attempt, `Busy` reported as `NotAcquired`, the durable row left
    /// pending. A boundary whose *invocation* is re-driven by a durable engine
    /// overrides with [`wait_out_crashed_lane_holder`](Self::wait_out_crashed_lane_holder):
    /// it waits out a crashed-looking holder and otherwise fails with the typed
    /// retryable `RuntimeErrorCode::SessionExecutionLaneBusy` so the engine's
    /// retry policy — not a sleep inside one invocation — paces the next
    /// attempt. Neither path bypasses or forges the holder's lease.
    ///
    /// Arguments are owned so this can be proxied across
    /// `EffectControllerTaskRequest`.
    async fn acquire_queued_lane(
        &self,
        lane: Arc<dyn QueuedLaneProbe>,
        _cancel: CancellationToken,
    ) -> Result<QueuedLaneAcquisition, RuntimeError> {
        match lane.try_acquire().await? {
            QueuedLaneAttempt::Acquired(guard) => Ok(QueuedLaneAcquisition::Acquired(guard)),
            QueuedLaneAttempt::Busy(_) => Ok(QueuedLaneAcquisition::NotAcquired),
        }
    }

    /// Bounded, aliveness-aware wait for engine-re-driven boundaries. Provided
    /// so `lash-restate` adopts the policy without lash-core exporting the
    /// policy types or a second free function.
    async fn wait_out_crashed_lane_holder(
        &self,
        lane: Arc<dyn QueuedLaneProbe>,
        cancel: CancellationToken,
    ) -> Result<QueuedLaneAcquisition, RuntimeError> {
        let mut wait = queued_lane_wait::QueuedLaneWait::default();
        #[cfg(feature = "otel-trace")]
        let mut contention_started: Option<tokio::time::Instant> = None;
        loop {
            let acquisition = match lane.try_acquire().await {
                Ok(acquisition) => acquisition,
                Err(error) => {
                    #[cfg(feature = "otel-trace")]
                    if let Some(started) = contention_started {
                        crate::operational_metrics::record_session_lane_contention_wait(
                            started.elapsed(),
                            "error",
                        );
                    }
                    return Err(error);
                }
            };
            match acquisition {
                QueuedLaneAttempt::Acquired(guard) => {
                    #[cfg(feature = "otel-trace")]
                    if let Some(started) = contention_started {
                        crate::operational_metrics::record_session_lane_contention_wait(
                            started.elapsed(),
                            "acquired",
                        );
                    }
                    return Ok(QueuedLaneAcquisition::Acquired(guard));
                }
                QueuedLaneAttempt::Busy(holder) => {
                    #[cfg(feature = "otel-trace")]
                    let started = *contention_started.get_or_insert_with(tokio::time::Instant::now);
                    let slice_ms = match wait.observe(&holder) {
                        queued_lane_wait::QueuedLaneWaitStep::Wait { slice_ms } => slice_ms,
                        queued_lane_wait::QueuedLaneWaitStep::GiveUp(give_up) => {
                            let waited_ms = wait.waited_ms();
                            #[cfg(feature = "otel-trace")]
                            crate::operational_metrics::record_session_lane_contention_wait(
                                started.elapsed(),
                                "gave_up",
                            );
                            crate::operational_metrics::record_session_lane_give_up(
                                give_up.as_str(),
                            );
                            queued_lane_wait::trace_busy_gave_up(&holder, give_up, waited_ms);
                            return Err(queued_lane_wait::lane_busy_error(
                                &holder, give_up, waited_ms,
                            ));
                        }
                    };
                    queued_lane_wait::trace_busy_wait(&holder, slice_ms, wait.waited_ms());
                    let sleep = lane.pause(std::time::Duration::from_millis(slice_ms));
                    tokio::select! {
                        () = sleep => {}
                        () = cancel.cancelled() => {
                            let give_up = queued_lane_wait::QueuedLaneGiveUp::CancelledWhileWaiting;
                            let waited_ms = wait.waited_ms();
                            #[cfg(feature = "otel-trace")]
                            crate::operational_metrics::record_session_lane_contention_wait(
                                started.elapsed(),
                                "gave_up",
                            );
                            crate::operational_metrics::record_session_lane_give_up(
                                give_up.as_str(),
                            );
                            queued_lane_wait::trace_busy_gave_up(
                                &holder,
                                give_up,
                                waited_ms,
                            );
                            return Err(queued_lane_wait::lane_busy_error(
                                &holder,
                                give_up,
                                waited_ms,
                            ));
                        },
                    }
                }
            }
        }
    }

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        let _ = (scope, wait);
        if may_defer {
            Ok(CompletionKeyPreparation::Unsupported)
        } else {
            Ok(CompletionKeyPreparation::NotNeeded)
        }
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

    async fn revoke_await_events_for_session(
        &self,
        _session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
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
    async fn cancel_await_events_for_session(
        &self,
        _session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventCancelUnsupported,
            "this effect boundary does not support cancelling durable waits",
        ))
    }

    /// Drop every promise of the terminal non-session `scope` and fence the
    /// scope permanently: later mints, resolves, peeks, and waits under it
    /// report `await_event_unknown_or_revoked`, including after a restart on
    /// durable hosts. Session-bearing scopes are refused with
    /// `await_event_scope_not_retirable`; they are revoked as a family through
    /// [`revoke_await_events_for_session`](Self::revoke_await_events_for_session).
    ///
    /// This is the promise half of [`EffectHost::retire_effect_journal`]: a
    /// durable host performs both halves in one transaction there and answers
    /// this lever from the same code path.
    async fn retire_await_events_for_scope(
        &self,
        _scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventUnsupported,
            "this effect boundary does not support retiring await-event scopes",
        ))
    }

    /// [`retire_await_events_for_scope`](Self::retire_await_events_for_scope)
    /// only when no waiter is parked on a promise under `scope`, answering
    /// whether it retired: `Ok(false)` leaves the scope untouched and unfenced.
    /// The proof and the fence must land under one lock. Resolvers that prove
    /// quiescence elsewhere (a durable journal reads its wait rows in the
    /// retirement transaction) retire unconditionally here and answer `true`.
    async fn retire_await_events_for_scope_if_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.retire_await_events_for_scope(scope)
            .await
            .map(|()| true)
    }

    /// Lift the fence
    /// [`retire_await_events_for_scope`](Self::retire_await_events_for_scope)
    /// left on a non-session `scope`, because its owner is registered again.
    /// Resolvers that never fence have nothing to lift and answer `Ok`.
    async fn reinstate_await_event_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        if scope.session_id().is_some() {
            return Err(await_event_scope_not_retirable(scope));
        }
        Ok(())
    }

    /// Whether `scope` is fenced by a scope-exact retirement this resolver
    /// holds. Every admission path that runs effects for a scope consults it
    /// before executing, so a retired scope is refused even where no journal
    /// claim exists to refuse it (the in-process host). Resolvers whose
    /// journal already refuses retired scopes at claim time may answer
    /// `false`.
    async fn await_event_scope_is_retired(
        &self,
        _scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        Ok(false)
    }
}

/// Deployment-level factory for scoped effect controllers.
#[async_trait::async_trait]
pub trait EffectHost: AwaitEventResolver {
    /// Stable identity of the physical authority that owns this host's reserved
    /// turn-control promises. Implementors must preserve it across client or
    /// handler recreation for as long as issued keys remain recoverable.
    fn turn_control_binding_id(&self) -> String;
    /// Declares the owner of reserved turn-control promises for this host.
    fn turn_control_authority_owner(&self) -> TurnControlAuthorityOwner {
        TurnControlAuthorityOwner::EffectHost
    }

    /// List the registered, unresolved await-event keys owned by `session_id`.
    ///
    /// This is a deployment-administrative snapshot, not a replay-sensitive
    /// per-run observation. A key may resolve concurrently after it is
    /// returned; callers must handle the existing first-writer-wins
    /// [`ResolveOutcome`] when they act on it. Possession of a returned key is
    /// resolution authority, so hosts must expose this read only to callers
    /// authorized to resolve that session's waits.
    ///
    /// Hosts that cannot enumerate their registry fail explicitly rather than
    /// reporting an empty session.
    async fn list_outstanding_await_event_keys(
        &self,
        _session_id: &SessionId,
    ) -> Result<Vec<AwaitEventKey>, RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventUnsupported,
            "this effect host does not support listing outstanding await-event keys",
        ))
    }

    /// Project the terminal attachment owned by this same effect deployment.
    /// Durable hosts override this projection; native hosts use keyed promises through the host.
    fn turn_attach(&self) -> Option<Arc<dyn crate::TurnAttach>> {
        None
    }
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

    /// Projects this host to the resolver that owns its await-event registry.
    fn await_event_resolver(&self) -> &dyn AwaitEventResolver;

    async fn turn_control_binding<'a>(
        &'a self,
        scoped: &'a ScopedEffectController<'_>,
    ) -> Result<TurnControlBinding<'a>, RuntimeError> {
        let binding_id = super::turn_control_authority::turn_control_binding_id_for_scope(
            &self.turn_control_binding_id(),
            scoped.execution_scope(),
        )?;
        match scoped.controller().turn_control_participation().await? {
            TurnControlParticipation::Local => {
                let resolver = self.await_event_resolver();
                Ok(TurnControlBinding::host_owned(
                    binding_id,
                    resolver,
                    self.scoped(scoped.execution_scope().clone())?,
                    self.turn_attach(),
                ))
            }
            TurnControlParticipation::DurableJournaled => {
                let resolver = scoped.controller();
                let Some(controller_authority_id) = resolver.await_event_authority_binding_id()
                else {
                    return Err(RuntimeError::new(
                        crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                        "durable turn-control controller does not identify its await-event authority",
                    ));
                };
                let controller_binding_id =
                    super::turn_control_authority::turn_control_binding_id_for_scope(
                        &controller_authority_id,
                        scoped.execution_scope(),
                    )?;
                if controller_binding_id != binding_id {
                    return Err(RuntimeError::new(
                        crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                        format!(
                            "turn-control host authority `{binding_id}` does not match controller authority `{controller_binding_id}`"
                        ),
                    ));
                }
                Ok(TurnControlBinding::run_scoped(
                    binding_id,
                    resolver,
                    true,
                    self.turn_attach(),
                ))
            }
        }
    }

    async fn prepare_tool_intent(
        &self,
        sink: &dyn ToolIntentOutcomeSink,
        identity: &crate::ToolIntentIdentity,
        intent: crate::ToolIntent,
    ) -> Result<ToolIntentPreparation, RuntimeError> {
        let guard = sink.lock_submission_gate(&identity.replay_key).await;
        let record =
            crate::ToolIntentSubmissionRecord::new(identity.clone(), intent).map_err(|error| {
                RuntimeError::new(
                    RuntimeErrorCode::RecordEncodingFailed,
                    format!("failed to hash tool-intent submission: {error}"),
                )
            })?;
        let admission = sink.admit(record).await?;
        Ok(ToolIntentPreparation::RuntimeOwned {
            admission,
            _guard: guard,
        })
    }

    async fn record_tool_intent_outcome(
        &self,
        sink: &dyn ToolIntentOutcomeSink,
        identity: &crate::ToolIntentIdentity,
        _submitted: crate::ToolIntent,
        outcome: crate::ToolIntentExecutionOutcome,
    ) -> Result<(), RuntimeError> {
        sink.complete_submission(identity, outcome).await
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

    /// Lift the scope-retirement fence of `scope` because its owner is being
    /// registered again: a pruned process id that a host re-registers starts
    /// its new incarnation unfenced, with the empty journal the prune left
    /// (ADR 0049). The fence row alone is removed; nothing is re-created.
    /// Session-bearing scopes are refused with
    /// `await_event_scope_not_retirable`, exactly as the scope lever refuses
    /// to retire them. A host that never fences (it does not implement
    /// scope-exact retirement) has nothing to lift and answers `Ok`.
    async fn reinstate_effect_scope(&self, scope: &ExecutionScope) -> Result<(), RuntimeError> {
        if scope.session_id().is_some() {
            return Err(await_event_scope_not_retirable(scope));
        }
        Ok(())
    }

    /// The SQLite database file holding this host's effect journal, when the
    /// journal lives in a file of its own that a session-store factory
    /// attaches for the retention sweep (ADR 0067). A host whose journal
    /// shares the store's database, keeps it in memory, or has no durable
    /// journal answers `None`.
    fn effect_scope_fence_database(&self) -> Option<std::path::PathBuf> {
        None
    }

    /// Bind the process registry that owns the process-scope fence
    /// (ADR 0049). Called by [`ProcessRegistrar::bind_effect_host`]
    /// (crate::ProcessRegistrar::bind_effect_host); idempotent.
    ///
    /// A host with a durable journal of its own in a file beside the
    /// registry's attaches [`ProcessRegistryBinding::fence_database`]
    /// (crate::ProcessRegistryBinding::fence_database) and reads and writes
    /// process-scope fences there, so registration's commit point and
    /// retirement's commit point are the same file. A host whose fence is a
    /// cache (the Restate durable-wait index) keeps
    /// [`ProcessRegistryBinding::registrations`]
    /// (crate::ProcessRegistryBinding::registrations) and treats a fence
    /// on a registered process scope as stale. A host that never fences
    /// ignores the binding.
    fn bind_process_registry(&self, _binding: crate::ProcessRegistryBinding) {}

    /// Register one durable session catalog as a lifetime participant in a
    /// non-session scope. The owner serializes this with irreversible scope
    /// retirement; an existing retirement fence refuses registration.
    async fn register_turn_cancel_closure_participant(
        &self,
        _participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        if scope.session_id().is_some() {
            return Ok(());
        }
        Err(RuntimeError::new(
            RuntimeErrorCode::EffectJournalRetirementUnsupported,
            "this effect host does not implement cancellation-closure lifecycle participation",
        ))
    }

    /// Release a catalog's scope participant after that catalog has durably
    /// fenced new authorizations and proved that no authorization remains.
    async fn release_turn_cancel_closure_participant(
        &self,
        _participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        if scope.session_id().is_some() {
            return Ok(());
        }
        Err(RuntimeError::new(
            RuntimeErrorCode::EffectJournalRetirementUnsupported,
            "this effect host does not implement cancellation-closure lifecycle participation",
        ))
    }
}

/// A session catalog's stable registration at the physical promise owner.
///
/// Catalog authorization first registers this participant at the owner and
/// then commits its local pin. Scope retirement reverses that order: it first
/// fences and drains the catalog, then releases the participant. This ordering
/// leaves only conservative retained participants across crashes, never an
/// unprotected authorization.
#[derive(Clone)]
pub struct TurnCancelClosureOwnerBinding {
    participant_id: Arc<str>,
    owner: Arc<dyn EffectHost>,
}

impl std::fmt::Debug for TurnCancelClosureOwnerBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TurnCancelClosureOwnerBinding")
            .field("participant_id", &self.participant_id)
            .finish_non_exhaustive()
    }
}

impl TurnCancelClosureOwnerBinding {
    pub fn new(participant_id: impl Into<Arc<str>>, owner: Arc<dyn EffectHost>) -> Self {
        Self {
            participant_id: participant_id.into(),
            owner,
        }
    }

    pub async fn register(
        &self,
        scope: &ExecutionScope,
        admitted_binding_id: &str,
    ) -> Result<(), RuntimeError> {
        let owner_binding_id = super::turn_control_authority::turn_control_binding_id_for_scope(
            &self.owner.turn_control_binding_id(),
            scope,
        )?;
        if owner_binding_id != admitted_binding_id {
            return Err(RuntimeError::new(
                RuntimeErrorCode::InvalidTurnCancelRequest,
                format!(
                    "session catalog cancellation owner `{owner_binding_id}` does not match admitted authority `{admitted_binding_id}`"
                ),
            ));
        }
        self.owner
            .register_turn_cancel_closure_participant(&self.participant_id, scope)
            .await
    }

    pub async fn release(&self, scope: &ExecutionScope) -> Result<(), RuntimeError> {
        self.owner
            .release_turn_cancel_closure_participant(&self.participant_id, scope)
            .await
    }
}

/// Boundary for nondeterministic runtime work.
#[async_trait::async_trait]
pub trait RuntimeEffectController: AwaitEventResolver {
    /// Whether an engine owns pacing for commits made by this controller.
    /// Store-backed replay controllers leave this false: durable journal
    /// participation alone does not imply engine-owned backpressure.
    fn owns_commit_backpressure(&self) -> bool {
        false
    }

    async fn runtime_effect_failure_disposition(
        &self,
        _code: RuntimeErrorCode,
    ) -> Result<RuntimeEffectFailureDisposition, RuntimeError> {
        Ok(RuntimeEffectFailureDisposition::RecordTurnFailure)
    }

    async fn turn_control_participation(&self) -> Result<TurnControlParticipation, RuntimeError> {
        Ok(TurnControlParticipation::Local)
    }

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
    /// A host may answer `true` only if it owns a registered
    /// [`GroupExecutors`](super::super::group_drain::GroupExecutors) resolver,
    /// because that resolver is where the children's `'static` executors come
    /// from: a child must be able to outlive its caller to honor
    /// [`LoserPolicy::RunToCompletion`], and a host with nothing to resolve
    /// a journaled child's envelope through cannot run one child, let alone
    /// outlive a caller with it. The capability and the resolver are one
    /// question and must not drift apart.
    ///
    /// It is therefore a **per-deployment fact established at wiring time**:
    /// before the resolver is registered a host answers `false`, and deployment
    /// validation reads it after wiring. The coherence law that follows binds the
    /// whole surface — a host answering `false` refuses `open_effect_group`,
    /// `await_next_settlement` and `close_effect_group` alike with
    /// [`EffectGroupUnsupported`](crate::RuntimeErrorCode::EffectGroupUnsupported),
    /// because a `false` flag beside a method that works, or an `Ok` from a host
    /// that says it has no groups, is the drift this relation exists to catch.
    fn supports_effect_groups(&self) -> bool {
        false
    }

    /// Open — or replay — a group of independently journaled child effects.
    ///
    /// Returns once the group is durably recorded, **not** when a child settles.
    ///
    /// The one parameter is the group itself: **envelopes, and nothing else**.
    /// A caller does not supply the code that runs a child, because a caller
    /// cannot: three of the four paths that execute a grouped child — a retry, a
    /// drain of a group whose caller is gone, and an engine tier's own child
    /// invocation — happen where no caller is in scope, so a host that could
    /// only run what a caller handed it could not honor the contract at all. The
    /// host resolves every child from its journaled envelope through its
    /// registered [`GroupExecutors`](super::super::group_drain::GroupExecutors),
    /// which is the same seam the loser drain resolves through, so one host has
    /// one answer to "what code runs this child" on every path.
    ///
    /// The `'static` property is unchanged and still ratified — children must
    /// outlive the caller's future under
    /// [`LoserPolicy::RunToCompletion`], and the borrow-scoped
    /// `RuntimeEffectLocalExecutor<'_>` taken by
    /// [`execute_effect`](Self::execute_effect) carries the one lifetime this
    /// contract exists to break — it just lives at the resolver now rather than
    /// at the argument.
    ///
    /// **A child with no runner is a routing fact, not an outcome.** On a
    /// **first open** a host resolves *all* of the group's children **before it
    /// records the group**, and refuses the whole open with a typed
    /// [`RuntimeEffectGroupShape`](crate::RuntimeErrorCode::RuntimeEffectGroupShape)
    /// error if any child resolves to `None`. Recording first and discovering
    /// the gap later would leave a recorded group holding a child that can never
    /// settle, so every rank above it is unservable and the caller waits forever
    /// — the same failure the retired executor-vec arity check existed to
    /// prevent, now unrepresentable by absence because there is no vec to
    /// misalign. **The refusal must journal nothing, including the group row**:
    /// a host that recorded the group and then refused would answer the retry as
    /// a reopen, and a reopen passes the miss through, so the second attempt
    /// would succeed around a child that can never settle — a strand one attempt
    /// later, which is worse than the refusal it replaced. On a **reopen** the
    /// same miss is not an open refusal: the group is already journaled, and a
    /// deployment that has lost one child's runner is the drain's `NoExecutor`
    /// case (ADR 0065).
    ///
    /// A host with **no registered resolver at all** is a different fact from a
    /// child it cannot route, and answers differently: it reports
    /// `supports_effect_groups() == false` and refuses all three methods with
    /// [`EffectGroupUnsupported`](crate::RuntimeErrorCode::EffectGroupUnsupported),
    /// which is the flag's coherence law. Such a refusal journals nothing.
    ///
    /// A reopen must be fenced on group shape: a host that finds a recorded group
    /// under this key whose child count or wake rule differs from the group
    /// passed here must refuse rather than reopen, because a shrunk child vec
    /// under one key silently renumbers every rank above the truncation and the
    /// per-child envelope-hash fence cannot see it.
    ///
    /// The default errors loudly rather than mis-executing a group, matching
    /// [`AwaitEventResolver::cancel_await_events_for_session`]: an out-of-tree
    /// controller that has not implemented groups fails closed with a named
    /// error.
    async fn open_effect_group(
        &self,
        _group: RuntimeEffectGroup,
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
    /// `(handle.consumed() + 1)`-th smallest sequence — never by literal sequence
    /// equality, because sequences are monotonic without being gapless.
    ///
    /// The handle is the sole cursor of record and is taken by `&mut`: an
    /// implementation calls [`EffectGroupHandle::advance`] on exactly the
    /// settlements it returns and keeps no per-caller consumption state of its
    /// own, which is what makes consumption exactly-once across a crash. See
    /// [`EffectGroupHandle`] for the full normative rule.
    ///
    /// Cancellation returns
    /// [`RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled`](crate::RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled)
    /// and leaves the cursor and the durable rank untouched, so a later await
    /// resumes at the same rank. Exhaustion has no code because it is the
    /// caller's arithmetic: check
    /// [`EffectGroupHandle::is_exhausted`] rather than awaiting past the last
    /// child.
    async fn await_next_settlement(
        &self,
        _handle: &mut EffectGroupHandle,
        _cancel: CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::EffectGroupUnsupported,
            "this effect controller does not implement durable effect groups",
        ))
    }

    /// Release the caller's interest in the group.
    ///
    /// Under [`LoserPolicy::RunToCompletion`] the remaining children keep
    /// running under host ownership and journal their own settlements; the host
    /// owns their redrive. Under [`LoserPolicy::Cancel`] the host cancels
    /// them and journals each cancellation as that child's terminal. Either way
    /// the caller may not observe further settlements.
    ///
    /// `disposition` may only **narrow** the one the group declared at open:
    /// resolve it through
    /// [`LoserPolicy::resolve_close`] and refuse a widening request. The
    /// declared disposition is authoritative — it is journaled with the group
    /// row, so a group abandoned by a crash before its close is drained under it
    /// too, and no policy is invented at drain time.
    ///
    /// Close is **idempotent**, and its failure is retryable. Taking the handle
    /// by value blocks reuse only in-process: the handle is `Deserialize`, so a
    /// crash between a successful close and the continuation commit means a
    /// replayed frame closes the same group again by construction. A second close
    /// under the same disposition must therefore succeed rather than raise on a
    /// healthy replay path.
    async fn close_effect_group(
        &self,
        _handle: EffectGroupHandle,
        _disposition: LoserPolicy,
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
#[path = "control/tests.rs"]
mod tests;
